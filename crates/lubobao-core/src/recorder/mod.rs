//! 录制引擎：用 ffmpeg 把一路直播流复制成本地 TS 文件；开启
//! [`RecordOptions::direct_download`] 且源是 HTTP(S) FLV 时改为直接下载成 FLV（[`direct`]），
//! 对外的句柄、事件和结果相同。
//!
//! 调用 [`start`] 得到 [`RecordingHandle`]，用它停止录制、读取快照或等待结束；
//! 过程中的状态通过 [`EventListener`] 回调发出（[`RecorderEvent`] 可直接用 serde 序列化）。
//! 引擎不关心任务、调度和界面，只负责一次录制从启动到收尾。应用退出前调用 [`stop_all`]
//! 正常收尾所有录制；异常退出时由 [`process_guard`] 结束 ffmpeg。
//!
//! 行为参照 StreamCap `app/core/recording/stream_manager.py` 的 `start_ffmpeg` 与
//! `app/core/media/ffmpeg_builders/`。有意的差异：
//!
//! - 不加 `-reconnect*`。StreamCap 把这些选项写在 `-i` 之后，对输入并不生效；放到 `-i`
//!   之前时，FFmpeg 9 在直播源断开后会无限重连（实测 90 秒仍未退出），无法判断下播。
//!   断流后由调度重新检测开播状态，再开始新文件。
//! - 不加 `-re`。直播源本身按实时速率到达，`-re` 只会让 CDN 首包缓存读得更慢。
//! - 不加 `-bufsize`，它只对编码器码率控制有效，复制流时无作用。
//! - `-correct_ts_overflow` 放到输入选项，HLS 输入另加 `-m3u8_hold_counters 10`，
//!   让下播后的 HLS 录制及时结束。
//! - 请求头中的 User-Agent、Referer 分别用 `-user_agent`、`-referer` 传递，其余放进
//!   `-headers`；没有 User-Agent 时沿用 StreamCap 的默认值。
//! - 停止时在所有系统上都向 stdin 写 `q`（StreamCap 在 Windows 写 `q`、其他系统发
//!   SIGINT），超时后强制结束。TS 即使被强制结束也能播放。
//! - 增加看门狗：长时间没有新数据时主动停止并报告网络错误，避免进程挂起。
//! - 文件名去掉 `%`（分段文件名会被当作格式串），名称截断到 60 个字符，避开 Windows
//!   保留设备名；同一秒重复开始时追加序号，不覆盖已有录像。保存目录里的 `%` 在交给
//!   segment 封装器时转义为 `%%`。

pub mod command;
pub mod direct;
pub mod naming;
pub mod outcome;
pub mod postprocess;
pub mod process_guard;
pub mod progress;
pub mod subtitle;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::Local;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};

pub use crate::platforms::SourceFormat;
pub use outcome::{EndReason, StopCause};
pub use progress::Progress;

use command::CommandSpec;
use outcome::ExitSummary;
use progress::{ErrorTail, LineSplitter};
use subtitle::SubtitleWriter;

/// 默认分段时长（秒），StreamCap `video_segment_time`。
pub const DEFAULT_SEGMENT_SECS: u64 = 1800;
/// 默认磁盘阈值 2 GB，StreamCap `recording_space_threshold`（按 1024³ 字节计）。
pub const DEFAULT_DISK_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 录制中检查磁盘空间的间隔（秒）。
pub const DEFAULT_DISK_CHECK_INTERVAL_SECS: u64 = 10;
/// 写入 `q` 后等待 ffmpeg 收尾的时间（秒），超时强制结束。
pub const DEFAULT_STOP_TIMEOUT_SECS: u64 = 15;
/// 多久没有新数据就判定为卡住（秒）。
pub const DEFAULT_STALL_TIMEOUT_SECS: u64 = 90;
/// 看门狗时限的下限（秒）。FLV 开始写出数据前，ffmpeg 最多可能花 `-rw_timeout`（15 秒）
/// 加 `-analyzeduration`（20 秒）连接和探测，低于这个值会误判正常的探测。
pub const MIN_STALL_TIMEOUT_SECS: u64 = 40;

/// 保留的 stderr 错误行数。
const ERROR_TAIL_LINES: usize = 20;
/// ffmpeg 退出后，等待 stderr 读完的时间。
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// 用户停止时，进度超过这么久没有前进就判定为断流，直接结束 ffmpeg。
///
/// 统计行每秒一行（`-stats_period 1`），正常推流时时长和大小每秒都会增加；3 秒相当于
/// 连续漏掉 3 次进度，CDN 一两秒的抖动不会误判。断流时 ffmpeg 卡在网络读取里，读不到
/// stdin 上的 `q`，只能等 `-rw_timeout` 或 `stop_timeout_secs`；TS 没有文件尾，直接结束
/// 也能完整播放已写入的部分。
const STOP_IDLE_THRESHOLD: Duration = Duration::from_secs(3);

/// 输出容器。目前只支持 TS。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputFormat {
    #[default]
    Ts,
}

/// 录制选项。缺省值与 StreamCap `config/default_settings.json` 一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RecordOptions {
    pub format: OutputFormat,
    /// 是否分段，StreamCap `segmented_recording_enabled` 默认 `true`。
    pub segment_enabled: bool,
    /// 分段时长（秒）。
    pub segment_secs: u64,
    /// 磁盘剩余空间低于该值时不开始录制，录制中则正常停止。0 表示不检查。
    pub disk_threshold_bytes: u64,
    /// 代理地址；为空或 `None` 时不用代理。ffmpeg 录制只支持 HTTP 代理，直接下载
    /// （[`direct_download`](Self::direct_download)）还支持 HTTPS 和 SOCKS 代理。
    pub proxy: Option<String>,
    /// 源是 HTTP(S) 的 FLV 时不经 ffmpeg，直接把响应体保存为 `.flv`，不分段，见 [`direct`]。
    /// 其他源忽略这一项。
    pub direct_download: bool,
    /// 目录是否包含平台名，StreamCap `folder_name_platform` 默认 `true`。
    pub folder_by_platform: bool,
    /// 目录是否包含主播名，StreamCap `folder_name_author` 默认 `true`。
    pub folder_by_anchor: bool,
    /// 目录是否再加开始录制当天的日期（`YYYY-MM-DD`），StreamCap `folder_name_time`
    /// 默认 `false`。
    pub folder_by_date: bool,
    /// 目录是否再加 `标题_主播名` 一层（标题清理后为空时不加），StreamCap
    /// `folder_name_title` 默认 `false`。
    pub folder_by_title: bool,
    /// 文件名是否带直播标题，StreamCap `filename_includes_title` 默认 `false`。
    pub filename_includes_title: bool,
    /// 名称里是否去掉 emoji。StreamCap `remove_emojis` 默认 `false`；录播宝一直去掉，
    /// 默认保持 `true`，让已有用户的目录名不变。
    pub remove_emojis: bool,
    /// 未分段录制时是否在录像旁写每秒一条的时间字幕（`.srt`），StreamCap
    /// `generate_time_subtitle_file` 默认 `false`。分段录制不生成。
    pub time_subtitle: bool,
    pub disk_check_interval_secs: u64,
    pub stop_timeout_secs: u64,
    /// 看门狗时限（秒）。0 表示关闭；非 0 时不低于 [`MIN_STALL_TIMEOUT_SECS`]。
    pub stall_timeout_secs: u64,
    /// HLS 输入不检查分片扩展名（`-extension_picky 0`），CHZZK 需要，见
    /// [`crate::platforms::Platform::relaxed_hls_extension`]。
    pub relaxed_hls_extension: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::Ts,
            segment_enabled: true,
            segment_secs: DEFAULT_SEGMENT_SECS,
            disk_threshold_bytes: DEFAULT_DISK_THRESHOLD_BYTES,
            proxy: None,
            direct_download: false,
            folder_by_platform: true,
            folder_by_anchor: true,
            folder_by_date: false,
            folder_by_title: false,
            filename_includes_title: false,
            remove_emojis: true,
            time_subtitle: false,
            disk_check_interval_secs: DEFAULT_DISK_CHECK_INTERVAL_SECS,
            stop_timeout_secs: DEFAULT_STOP_TIMEOUT_SECS,
            stall_timeout_secs: DEFAULT_STALL_TIMEOUT_SECS,
            relaxed_hls_extension: false,
        }
    }
}

/// 实际生效的看门狗时限：0 表示关闭，其余值不低于 `minimum`。
pub fn effective_stall_timeout(secs: u64, minimum: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs.max(minimum).max(1)))
}

/// 一次录制的全部输入。
#[derive(Debug, Clone)]
pub struct RecordingRequest {
    /// 已校验的 ffmpeg 可执行文件，通常来自 `FfmpegManager::ffmpeg_path()`。
    pub ffmpeg_path: PathBuf,
    /// 取流地址，通常来自 `SelectedStream::source_url`。
    pub source_url: String,
    pub source_format: SourceFormat,
    /// 取流请求头，通常是 `LiveInfo::stream_headers`。
    pub headers: BTreeMap<String, String>,
    /// 录像根目录；相对路径按当前工作目录转为绝对路径。引擎在其下按平台、主播、日期、
    /// 标题建子目录（见 [`naming::recording_dir`]）。
    pub output_dir: PathBuf,
    /// 主播名，用于目录和文件名。
    pub anchor_name: String,
    /// 直播标题，来自检测结果；`filename_includes_title`、`folder_by_title` 用它命名。
    pub title: Option<String>,
    /// 平台显示名（例如 `抖音`），用于目录名。
    pub platform: String,
    pub options: RecordOptions,
}

/// 录制状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecorderState {
    /// 正在准备目录、检查磁盘和启动 ffmpeg。
    Starting,
    /// ffmpeg 已启动。
    Recording,
    /// 已请求停止，正在等 ffmpeg 收尾。
    Stopping,
    /// 已结束，见 `outcome`。
    Finished,
}

/// 一次录制的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingOutcome {
    pub reason: EndReason,
    /// 给用户看的补充说明，例如 ffmpeg 最后一行错误。
    pub message: Option<String>,
    /// ffmpeg 退出码；没有启动或被强制结束时可能为 `None`。
    pub exit_code: Option<i32>,
    /// 生成的非空录像文件；分段时按序号排序。
    pub files: Vec<PathBuf>,
    /// 所有录像文件的总字节数。
    pub bytes_written: u64,
    /// 最后一次进度报告的媒体时长（秒）。
    pub duration_secs: Option<f64>,
    /// 录像目录；在创建目录之前失败时为 `None`。
    pub output_dir: Option<PathBuf>,
    /// 开始和结束时间（Unix 毫秒）。
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
}

/// 句柄可以随时读取的快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderSnapshot {
    pub state: RecorderState,
    pub output_dir: Option<PathBuf>,
    /// 录像文件路径；分段时为 `stem_%03d.ts` 形式的模板（目录中的 `%` 不转义）。
    pub output_path: Option<PathBuf>,
    /// ffmpeg 进程号。
    pub pid: Option<u32>,
    pub progress: Option<Progress>,
    /// 当前生效的停止原因；停止期间用户停止会覆盖看门狗或磁盘停止。
    pub stop_cause: Option<StopCause>,
    pub outcome: Option<RecordingOutcome>,
}

/// 录制过程中的事件，按发生顺序发出：`started` → 若干 `progress`/`warning` →
/// （一到两次 `stopping`）→ `finished`。启动前就失败时只有 `finished`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RecorderEvent {
    Started {
        output_dir: PathBuf,
        output_path: PathBuf,
        source_format: SourceFormat,
        segmented: bool,
        pid: Option<u32>,
        started_at_ms: i64,
    },
    Progress {
        progress: Progress,
    },
    /// 不影响录制、但用户应当知道的问题，例如无法查询磁盘空间。
    Warning {
        message: String,
    },
    /// 开始停止。引擎因看门狗或磁盘停止后，用户再点停止会以 `user` 再发一次。
    Stopping {
        cause: StopCause,
    },
    Finished {
        outcome: RecordingOutcome,
    },
}

/// 事件回调。会在 Tokio 任务中调用，不要在里面阻塞。
pub type EventListener = Arc<dyn Fn(&RecorderEvent) + Send + Sync>;

/// 查询某个目录所在磁盘的可用字节数。
pub type SpaceProbe = Arc<dyn Fn(&Path) -> io::Result<u64> + Send + Sync>;

/// 默认的磁盘空间查询，使用 `fs4::available_space`（当前用户可用的空间）。
pub fn default_space_probe() -> SpaceProbe {
    Arc::new(|path: &Path| fs4::available_space(path))
}

/// 录制句柄。可以克隆；丢弃句柄不会停止录制。
#[derive(Clone)]
pub struct RecordingHandle {
    stop_tx: Arc<watch::Sender<bool>>,
    snapshot_rx: watch::Receiver<RecorderSnapshot>,
}

impl RecordingHandle {
    /// 请求正常停止，立即返回。重复调用无副作用。引擎已在因看门狗或磁盘不足停止时，
    /// 结束原因改记为用户停止。
    pub fn stop(&self) {
        self.stop_tx.send_replace(true);
    }

    /// 请求停止并等待收尾完成。
    pub async fn stop_and_wait(&self) -> RecordingOutcome {
        self.stop();
        self.wait().await
    }

    /// 等待录制结束。
    pub async fn wait(&self) -> RecordingOutcome {
        let mut rx = self.snapshot_rx.clone();
        let result = rx
            .wait_for(|snapshot| snapshot.outcome.is_some())
            .await
            .ok()
            .and_then(|snapshot| snapshot.outcome.clone());
        result.unwrap_or_else(|| {
            let now = Local::now().timestamp_millis();
            RecordingOutcome {
                reason: EndReason::FfmpegFailed,
                message: Some("录制任务意外结束".to_string()),
                exit_code: None,
                files: Vec::new(),
                bytes_written: 0,
                duration_secs: None,
                output_dir: None,
                started_at_ms: now,
                ended_at_ms: now,
            }
        })
    }

    /// 当前快照。
    pub fn snapshot(&self) -> RecorderSnapshot {
        self.snapshot_rx.borrow().clone()
    }

    /// 是否已经结束。
    pub fn is_finished(&self) -> bool {
        self.snapshot_rx.borrow().outcome.is_some()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 正在运行的录制，供 [`stop_all`] 使用。
static ACTIVE: Mutex<BTreeMap<u64, RecordingHandle>> = Mutex::new(BTreeMap::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// 正在运行的录制数量。
pub fn active_recordings() -> usize {
    lock(&ACTIVE).len()
}

/// 停止所有正在运行的录制并等待收尾，返回各自的结果。应用在
/// `RunEvent::ExitRequested` 中先 `prevent_exit`，等它完成后再退出。
/// 每个录制最多等待 `stop_timeout_secs` 后被强制结束，所以总耗时有上限；这也是用户停止，
/// 断流的录制会立即结束，见 [`STOP_IDLE_THRESHOLD`]。
pub async fn stop_all() -> Vec<RecordingOutcome> {
    let handles: Vec<RecordingHandle> = lock(&ACTIVE).values().cloned().collect();
    for handle in &handles {
        handle.stop();
    }
    let mut outcomes = Vec::with_capacity(handles.len());
    for handle in &handles {
        outcomes.push(handle.wait().await);
    }
    outcomes
}

/// 进程内已分配但可能还没被 ffmpeg 创建的文件，防止同一秒开始的两次录制写进同一个文件。
static RESERVED_FILES: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// 占用一个文件名，drop 时释放。
struct NameReservation {
    first_file: PathBuf,
}

impl Drop for NameReservation {
    fn drop(&mut self) {
        lock(&RESERVED_FILES).remove(&self.first_file);
    }
}

/// 选一个既不在磁盘上、也没被本进程其他录制占用的文件名主体，并占用它。
fn reserve_stem(
    dir: &Path,
    base: &str,
    segmented: bool,
    extension: &str,
) -> (String, NameReservation) {
    let mut reserved = lock(&RESERVED_FILES);
    let stem = naming::unique_stem(base, |candidate| {
        let first = naming::first_file_path(dir, candidate, segmented, extension);
        reserved.contains(&first) || first.exists()
    });
    let first_file = naming::first_file_path(dir, &stem, segmented, extension);
    reserved.insert(first_file.clone());
    (stem, NameReservation { first_file })
}

/// 开始录制。必须在 Tokio 运行时中调用（Tauri 的异步命令和 `tauri::async_runtime` 都可以）。
///
/// 返回前会建好目录、检查磁盘并启动 ffmpeg；这些步骤失败时返回的句柄已经结束，
/// 回调也会收到 `finished`。录制由后台任务管理，与句柄和调用方的生命周期无关。
pub async fn start(request: RecordingRequest, listener: EventListener) -> RecordingHandle {
    start_with_probe(request, listener, default_space_probe()).await
}

/// 与 [`start`] 相同，但可以替换磁盘空间查询（用于测试）。
pub async fn start_with_probe(
    request: RecordingRequest,
    listener: EventListener,
    probe: SpaceProbe,
) -> RecordingHandle {
    start_inner(request, listener, probe, MIN_STALL_TIMEOUT_SECS).await
}

async fn start_inner(
    request: RecordingRequest,
    listener: EventListener,
    probe: SpaceProbe,
    min_stall_secs: u64,
) -> RecordingHandle {
    let (stop_tx, stop_rx) = watch::channel(false);
    let (snapshot_tx, snapshot_rx) = watch::channel(RecorderSnapshot {
        state: RecorderState::Starting,
        output_dir: None,
        output_path: None,
        pid: None,
        progress: None,
        stop_cause: None,
        outcome: None,
    });
    let handle = RecordingHandle {
        stop_tx: Arc::new(stop_tx),
        snapshot_rx,
    };
    let started_at = Local::now();
    let started_at_ms = started_at.timestamp_millis();
    let started_instant = Instant::now();
    let early_failure = |reason: EndReason, message: String, output_dir: Option<PathBuf>| {
        let outcome = RecordingOutcome {
            reason,
            message: Some(message),
            exit_code: None,
            files: Vec::new(),
            bytes_written: 0,
            duration_secs: None,
            output_dir,
            started_at_ms,
            ended_at_ms: Local::now().timestamp_millis(),
        };
        finish(&snapshot_tx, &listener, outcome);
    };

    let options = &request.options;
    let direct = direct::applies(&request);
    // 直接下载由 reqwest 连接，代理在 `direct::Download::prepare` 里检查。
    let proxy = if direct {
        None
    } else {
        match command::ffmpeg_proxy(options.proxy.as_deref().unwrap_or_default()) {
            Ok(proxy) => proxy,
            Err(message) => {
                early_failure(EndReason::FfmpegFailed, message, None);
                return handle;
            }
        }
    };

    // 相对路径会让磁盘查询落到系统盘，先转为绝对路径。
    let root = match std::path::absolute(&request.output_dir) {
        Ok(root) => root,
        Err(error) => {
            early_failure(
                EndReason::FfmpegFailed,
                format!("录像目录无效 {}：{error}", request.output_dir.display()),
                None,
            );
            return handle;
        }
    };
    let name_parts = naming::NameParts {
        platform: &request.platform,
        anchor_name: &request.anchor_name,
        title: request.title.as_deref(),
        started: started_at.naive_local(),
    };
    let dir = naming::recording_dir(&root, &name_parts, options);
    if let Err(error) = tokio::fs::create_dir_all(&dir).await {
        early_failure(
            EndReason::FfmpegFailed,
            format!("无法创建录像目录 {}：{error}", dir.display()),
            Some(dir),
        );
        return handle;
    }

    // 查询根目录：同一块磁盘，路径更短。
    let mut warnings = Vec::new();
    match check_space(&probe, &root, options.disk_threshold_bytes).await {
        SpaceCheck::Low(free) => {
            early_failure(
                EndReason::DiskFull,
                format!(
                    "磁盘剩余 {}，低于阈值 {}，未开始录制",
                    format_gb(free),
                    format_gb(options.disk_threshold_bytes)
                ),
                Some(dir),
            );
            return handle;
        }
        SpaceCheck::Failed(message) => warnings.push(message),
        SpaceCheck::Enough | SpaceCheck::Skipped => {}
    }

    // 直接下载不分段。
    let segmented = options.segment_enabled && !direct;
    let extension = if direct {
        naming::FLV_EXTENSION
    } else {
        naming::TS_EXTENSION
    };
    let base_stem = naming::file_stem(&name_parts, options);
    let (stem, reservation) = reserve_stem(&dir, &base_stem, segmented, extension);
    let output_path = naming::output_path(&dir, &stem, segmented, extension);
    let engine = if direct {
        match direct::Download::prepare(&request) {
            Ok(download) => Engine::Direct(download),
            Err(message) => {
                early_failure(EndReason::FfmpegFailed, message, Some(dir));
                return handle;
            }
        }
    } else {
        let ffmpeg_output = if segmented {
            naming::ffmpeg_segment_pattern(&dir, &stem)
        } else {
            output_path.clone()
        };
        let args = command::build_args(&CommandSpec {
            source_url: &request.source_url,
            source_format: request.source_format,
            headers: &request.headers,
            output_path: &ffmpeg_output,
            segment_secs: segmented.then_some(options.segment_secs),
            proxy: proxy.as_deref(),
            relaxed_hls_extension: options.relaxed_hls_extension,
        });

        let mut command = Command::new(&request.ffmpeg_path);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        process_guard::configure(&mut command);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                early_failure(
                    EndReason::FfmpegFailed,
                    format!(
                        "无法启动 ffmpeg（{}）：{error}",
                        request.ffmpeg_path.display()
                    ),
                    Some(dir),
                );
                return handle;
            }
        };
        if let Err(message) = process_guard::attach(&child) {
            warnings.push(message);
        }
        Engine::Ffmpeg(child)
    };
    let pid = match &engine {
        Engine::Ffmpeg(child) => child.id(),
        Engine::Direct(_) => None,
    };

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    lock(&ACTIVE).insert(id, handle.clone());
    snapshot_tx.send_modify(|snapshot| {
        snapshot.state = RecorderState::Recording;
        snapshot.output_dir = Some(dir.clone());
        snapshot.output_path = Some(output_path.clone());
        snapshot.pid = pid;
    });
    listener(&RecorderEvent::Started {
        output_dir: dir.clone(),
        output_path: output_path.clone(),
        source_format: request.source_format,
        segmented,
        pid,
        started_at_ms,
    });
    for message in warnings {
        listener(&RecorderEvent::Warning { message });
    }

    // 分段录制时不生成字幕（与 StreamCap 一致）。
    let subtitle = (options.time_subtitle && !segmented)
        .then(|| SubtitleWriter::new(&output_path, started_at));
    let supervisor = Supervisor {
        id,
        listener,
        probe,
        snapshot_tx,
        stop_rx,
        root,
        dir,
        stem,
        output_path,
        segmented,
        threshold: options.disk_threshold_bytes,
        disk_check_interval: Duration::from_secs(options.disk_check_interval_secs.max(1)),
        stop_timeout: Duration::from_secs(options.stop_timeout_secs.max(1)),
        stall_timeout: effective_stall_timeout(options.stall_timeout_secs, min_stall_secs),
        started_at_ms,
        started_instant,
        subtitle,
        _reservation: reservation,
    };
    match engine {
        Engine::Ffmpeg(child) => tokio::spawn(supervisor.run(child)),
        Engine::Direct(download) => tokio::spawn(supervisor.run_direct(download)),
    };
    handle
}

/// 这次录制由谁写文件。
enum Engine {
    Ffmpeg(Child),
    /// FLV 直接下载，见 [`direct`]。
    Direct(direct::Download),
}

fn finish(
    snapshot_tx: &watch::Sender<RecorderSnapshot>,
    listener: &EventListener,
    outcome: RecordingOutcome,
) {
    listener(&RecorderEvent::Finished {
        outcome: outcome.clone(),
    });
    snapshot_tx.send_modify(|snapshot| {
        snapshot.state = RecorderState::Finished;
        if snapshot.output_dir.is_none() {
            snapshot.output_dir = outcome.output_dir.clone();
        }
        snapshot.outcome = Some(outcome);
    });
}

enum SpaceCheck {
    Enough,
    /// 剩余字节数低于阈值。
    Low(u64),
    /// 查询失败，附说明。
    Failed(String),
    /// 阈值为 0，不检查。
    Skipped,
}

async fn check_space(probe: &SpaceProbe, dir: &Path, threshold: u64) -> SpaceCheck {
    if threshold == 0 {
        return SpaceCheck::Skipped;
    }
    let probe = probe.clone();
    let path = dir.to_path_buf();
    match tokio::task::spawn_blocking(move || probe(&path)).await {
        Ok(Ok(free)) if free < threshold => SpaceCheck::Low(free),
        Ok(Ok(_)) => SpaceCheck::Enough,
        Ok(Err(error)) => SpaceCheck::Failed(format!(
            "无法查询 {} 的磁盘剩余空间，磁盘不足时不会自动停止：{error}",
            dir.display()
        )),
        Err(error) => SpaceCheck::Failed(format!("磁盘空间查询异常：{error}")),
    }
}

fn format_gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

struct Supervisor {
    id: u64,
    listener: EventListener,
    probe: SpaceProbe,
    snapshot_tx: watch::Sender<RecorderSnapshot>,
    stop_rx: watch::Receiver<bool>,
    root: PathBuf,
    dir: PathBuf,
    stem: String,
    output_path: PathBuf,
    segmented: bool,
    threshold: u64,
    disk_check_interval: Duration,
    stop_timeout: Duration,
    stall_timeout: Option<Duration>,
    started_at_ms: i64,
    /// 开始录制的时刻，时间字幕的时间轴从这里算起。
    started_instant: Instant,
    /// 时间字幕写入器；只在未分段且开启 `time_subtitle` 时有。
    subtitle: Option<SubtitleWriter>,
    /// 录制结束前一直占用文件名。
    _reservation: NameReservation,
}

/// 运行中的统计。
struct RunState {
    stop_cause: Option<StopCause>,
    kill_deadline: Option<Instant>,
    last_progress: Option<Progress>,
    received_media: bool,
    last_advance: Instant,
    tail: ErrorTail,
    /// 分段录制时磁盘上分段文件的总大小，每秒更新。
    segment_bytes: u64,
    /// 上一次磁盘查询是否失败，失败只在状态变化时报告一次。
    space_probe_failed: bool,
}

impl Supervisor {
    async fn run(mut self, mut child: Child) {
        let mut stdin = child.stdin.take();
        let (line_tx, mut line_rx) = mpsc::unbounded_channel();
        let reader = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(read_stderr(stderr, line_tx)));

        let mut state = RunState {
            stop_cause: None,
            kill_deadline: None,
            last_progress: None,
            received_media: false,
            last_advance: Instant::now(),
            tail: ErrorTail::new(ERROR_TAIL_LINES),
            segment_bytes: 0,
            space_probe_failed: false,
        };
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_disk_check = Instant::now();
        let mut stop_channel_open = true;

        let status = loop {
            tokio::select! {
                status = child.wait() => break status,
                Some(line) = line_rx.recv() => self.handle_line(&mut state, line),
                // 引擎自己停止期间也要响应用户停止，让结束原因记为用户停止。
                changed = self.stop_rx.changed(),
                    if stop_channel_open && state.stop_cause != Some(StopCause::User) =>
                {
                    match changed {
                        Ok(()) if *self.stop_rx.borrow_and_update() => {
                            self.user_stop(&mut state, &mut stdin).await;
                        }
                        Ok(()) => {}
                        // 所有句柄都已丢弃，录制继续，直到自然结束。
                        Err(_) => stop_channel_open = false,
                    }
                }
                _ = tick.tick() => {
                    if let Some(writer) = self.subtitle.as_mut() {
                        writer.write_until(self.started_instant.elapsed().as_secs()).await;
                    }
                    if self.segmented {
                        state.segment_bytes =
                            collect_files(&self.dir, &self.stem, &self.output_path, true)
                                .await
                                .iter()
                                .map(|(_, size)| size)
                                .sum();
                    }
                    if state.stop_cause.is_none() {
                        let stalled = self
                            .stall_timeout
                            .is_some_and(|limit| state.last_advance.elapsed() >= limit);
                        if stalled {
                            self.begin_stop(&mut state, &mut stdin, StopCause::Stalled).await;
                        } else if last_disk_check.elapsed() >= self.disk_check_interval {
                            last_disk_check = Instant::now();
                            self.check_disk(&mut state, &mut stdin).await;
                        }
                    }
                }
            }
            // 每轮都检查，让断流时的停止不必等下一次 tick。
            if let Some(deadline) = state.kill_deadline
                && Instant::now() >= deadline
            {
                let _ = child.start_kill();
                state.kill_deadline = None;
            }
        };

        // 读完 ffmpeg 退出前写出的最后几行。
        let _ = tokio::time::timeout(STDERR_DRAIN_TIMEOUT, async {
            while let Some(line) = line_rx.recv().await {
                self.handle_line(&mut state, line);
            }
        })
        .await;
        if let Some(reader) = reader {
            reader.abort();
        }
        drop(stdin);

        let files = collect_files(&self.dir, &self.stem, &self.output_path, self.segmented).await;
        if let Some(writer) = self.subtitle.take() {
            writer.finish(!files.is_empty()).await;
        }
        let bytes_written = files.iter().map(|(_, size)| size).sum::<u64>();
        let tail = state.tail.lines();
        let exit_code = status.ok().and_then(|status| status.code());
        let (reason, message) = outcome::classify(&ExitSummary {
            stop_cause: state.stop_cause,
            exit_code,
            received_media: state.received_media || bytes_written > 0,
            stderr_tail: &tail,
        });
        let outcome = RecordingOutcome {
            reason,
            message,
            exit_code,
            files: files.into_iter().map(|(path, _)| path).collect(),
            bytes_written,
            duration_secs: state
                .last_progress
                .as_ref()
                .and_then(|progress| progress.duration_secs),
            output_dir: Some(self.dir.clone()),
            started_at_ms: self.started_at_ms,
            ended_at_ms: Local::now().timestamp_millis(),
        };
        lock(&ACTIVE).remove(&self.id);
        finish(&self.snapshot_tx, &self.listener, outcome);
    }

    fn handle_line(&self, state: &mut RunState, line: String) {
        let Some(mut progress) = progress::parse_progress_line(&line) else {
            state.tail.push(line);
            return;
        };
        progress::fill_from_files(&mut progress, state.segment_bytes);
        let advanced = match &state.last_progress {
            None => progress.has_media(),
            Some(previous) => {
                progress.duration_secs.unwrap_or(0.0) > previous.duration_secs.unwrap_or(0.0)
                    || progress.size_bytes.unwrap_or(0) > previous.size_bytes.unwrap_or(0)
            }
        };
        if advanced {
            state.last_advance = Instant::now();
        }
        if progress.has_media() {
            state.received_media = true;
        }
        state.last_progress = Some(progress.clone());
        self.snapshot_tx
            .send_modify(|snapshot| snapshot.progress = Some(progress.clone()));
        (self.listener)(&RecorderEvent::Progress { progress });
    }

    async fn check_disk(&self, state: &mut RunState, stdin: &mut Option<ChildStdin>) {
        match check_space(&self.probe, &self.root, self.threshold).await {
            SpaceCheck::Low(_) => {
                state.space_probe_failed = false;
                self.begin_stop(state, stdin, StopCause::DiskFull).await;
            }
            SpaceCheck::Failed(message) => {
                if !state.space_probe_failed {
                    state.space_probe_failed = true;
                    (self.listener)(&RecorderEvent::Warning { message });
                }
            }
            SpaceCheck::Enough | SpaceCheck::Skipped => state.space_probe_failed = false,
        }
    }

    /// 用户停止。引擎已经在停止时只改记原因，不再写 `q`，也不推迟强制结束的时间。
    /// 最近 [`STOP_IDLE_THRESHOLD`] 内没有新数据时不再等 ffmpeg 收尾，立即结束进程。
    async fn user_stop(&self, state: &mut RunState, stdin: &mut Option<ChildStdin>) {
        if state.stop_cause.is_none() {
            self.begin_stop(state, stdin, StopCause::User).await;
        } else {
            self.set_stop_cause(state, StopCause::User);
        }
        if state.last_advance.elapsed() >= STOP_IDLE_THRESHOLD {
            state.kill_deadline = Some(Instant::now());
        }
    }

    async fn begin_stop(
        &self,
        state: &mut RunState,
        stdin: &mut Option<ChildStdin>,
        cause: StopCause,
    ) {
        state.kill_deadline = Some(Instant::now() + self.stop_timeout);
        if let Some(stdin) = stdin.as_mut() {
            // ffmpeg 读到 `q` 后写完文件尾再退出。写失败说明进程已经在退出。
            let _ = stdin.write_all(b"q").await;
            let _ = stdin.flush().await;
        }
        self.set_stop_cause(state, cause);
    }

    fn set_stop_cause(&self, state: &mut RunState, cause: StopCause) {
        state.stop_cause = Some(cause);
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.state = RecorderState::Stopping;
            snapshot.stop_cause = Some(cause);
        });
        (self.listener)(&RecorderEvent::Stopping { cause });
    }
}

async fn read_stderr(mut stderr: ChildStderr, lines: mpsc::UnboundedSender<String>) {
    let mut splitter = LineSplitter::default();
    let mut buffer = [0u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                for line in splitter.push(&buffer[..read]) {
                    if lines.send(line).is_err() {
                        return;
                    }
                }
            }
        }
    }
    if let Some(line) = splitter.finish() {
        let _ = lines.send(line);
    }
}

/// 列出本次录制生成的非空文件及其大小；分段文件按序号排序。
async fn collect_files(
    dir: &Path,
    stem: &str,
    output_path: &Path,
    segmented: bool,
) -> Vec<(PathBuf, u64)> {
    let dir = dir.to_path_buf();
    let stem = stem.to_string();
    let output_path = output_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut candidates: Vec<(u64, PathBuf)> = if segmented {
            std::fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .filter_map(|entry| {
                            let index = entry
                                .file_name()
                                .to_str()
                                .and_then(|name| naming::segment_index(name, &stem))?;
                            Some((index, entry.path()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![(0, output_path)]
        };
        candidates.sort();
        candidates
            .into_iter()
            .filter_map(|(_, path)| {
                let size = std::fs::metadata(&path).ok()?.len();
                (size > 0).then_some((path, size))
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}
