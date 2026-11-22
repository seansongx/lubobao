//! 录后处理：把录好的 TS 逐个转成同名 MP4（只换封装，不重新编码）。
//!
//! 调用 [`PostProcessor::enqueue`] 把一次录制的文件排进队列，转换在后台按顺序进行，
//! 结果通过 [`ConvertListener`] 回调发出。应用退出时调用 [`PostProcessor::shutdown`]：
//! 正在转换的中止并删除不完整的 MP4，排队中的放弃，原件都保留。本模块不依赖 Tauri，
//! 桌面应用和命令行程序共用。
//!
//! 行为参照 StreamCap `stream_manager.py` 的 `converts_mp4`，有意差异：
//!
//! - ffmpeg 参数：`-c copy -sn -dn -movflags +faststart -f mp4`，HEVC 视频另加
//!   `-tag:v hvc1`。`-sn -dn` 去掉 MP4 装不下的字幕、数据流（否则整个转换失败）；
//!   `+faststart` 把索引放到文件开头；`hvc1` 让 QuickTime、Safari、iOS 能播放 HEVC。
//!   ADTS 格式的 AAC 由 MP4 封装器自动转换，不强制 `aac_adtstoasc`，MP3、AC-3 音频
//!   也能转换。StreamCap 只用 `-c:v copy -c:a copy -f mp4`。
//! - 先写到 `<名称>.mp4.part`，读得出时长后再改名为 `.mp4`；失败时删除 `.part`。
//!   StreamCap 直接写目标文件，失败时留下不完整的 MP4。
//! - 成功的判断除退出码为 0 外，还要求输出非空、能读出时长（`ffmpeg -i` 只读文件头）。
//! - 不删除原件时，TS 留在原处。StreamCap 会把它移进同目录的 `original/` 子目录。
//! - 同名 MP4 已存在时不覆盖，改名为 `_2`、`_3`；转换期间出现的同名文件也不覆盖。
//! - 同时只转换 [`DEFAULT_CONCURRENCY`] 个文件，其余排队，不与录制抢 CPU 和磁盘。
//!   StreamCap 每个文件各开一个协程，分段多时同时转换。
//! - 转换前检查磁盘剩余空间：要写得下与原件同样大小的 MP4，并且转完后仍留有录制的
//!   磁盘阈值，否则直接失败。
//! - 退出时中止转换（StreamCap 交给后台服务继续转换）。下次启动不会补做，但会用
//!   [`clean_partial_files`] 删除上次留下的 `.mp4.part`。
//! - 文件系统操作和事件回调放进 `spawn_blocking`，保存目录在网络盘上也不卡异步线程。

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Notify, watch};

use super::process_guard;

/// 同时进行的转换数。
pub const DEFAULT_CONCURRENCY: usize = 1;
/// 退出时等待正在转换的 ffmpeg 结束、清理临时文件的上限。
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// 单个文件转换的时限：基础时长加上按大小（每 10 MB 一秒）推算的时间。
const BASE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const BYTES_PER_TIMEOUT_SEC: u64 = 10 * 1024 * 1024;
/// 读时长的时限。
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// 磁盘余量：除了与原件同样大小的 MP4，再多留这么多。
const SPACE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
/// 启动时清理残留临时文件时向下查找的目录层数（录像根目录/平台/主播）。
pub const CLEANUP_DEPTH: usize = 3;
/// 保留的 stderr 字节数，用于失败说明。
const STDERR_LIMIT: usize = 16 * 1024;

/// 转换选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertOptions {
    /// 已校验的 ffmpeg 可执行文件。
    pub ffmpeg_path: PathBuf,
    /// 确认 MP4 有效后删除原件。StreamCap `delete_original` 默认 `false`。
    pub delete_original: bool,
    /// 转换后磁盘上至少还要剩下的空间，通常是录制的磁盘阈值（`recording_space_threshold`），
    /// 避免转换占满空间，让其他直播间的录制因磁盘不足停止。
    pub reserve_bytes: u64,
}

/// 转换过程中的事件。serde 以 `type` 区分，字段用 camelCase。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConvertEvent {
    Started {
        source: PathBuf,
        target: PathBuf,
    },
    Succeeded {
        source: PathBuf,
        target: PathBuf,
        duration_secs: f64,
        /// 原件是否已删除。要求删除但删除失败时为 `false`，见 `delete_error`。
        original_deleted: bool,
        delete_error: Option<String>,
    },
    /// 转换失败，不完整的 MP4 已删除。`original_kept` 为原件是否还在（读不到原文件时为 false）。
    Failed {
        source: PathBuf,
        message: String,
        original_kept: bool,
    },
    /// 应用退出时中止（`started` 为 `true`）或放弃排队中的转换，原件保留。
    Cancelled {
        source: PathBuf,
        started: bool,
    },
}

pub type ConvertListener = Arc<dyn Fn(&ConvertEvent) + Send + Sync>;

/// 测磁盘剩余空间，测试中替换。
pub type SpaceProbe = Arc<dyn Fn(&Path) -> io::Result<u64> + Send + Sync>;

struct Job {
    source: PathBuf,
    options: ConvertOptions,
    listener: ConvertListener,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    /// 正在运行的工作协程数。
    workers: usize,
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    concurrency: usize,
    cancel: watch::Sender<bool>,
    idle: Notify,
    space_probe: SpaceProbe,
}

/// 转换队列。可 Clone，各份共用同一个队列。
#[derive(Clone)]
pub struct PostProcessor {
    inner: Arc<Inner>,
}

impl Default for PostProcessor {
    fn default() -> Self {
        Self::new(DEFAULT_CONCURRENCY)
    }
}

impl PostProcessor {
    /// 创建队列，最多同时转换 `concurrency` 个文件（至少 1 个）。不需要在 Tokio 运行时中
    /// 调用；工作协程在 [`enqueue`](Self::enqueue) 时按需启动。
    pub fn new(concurrency: usize) -> Self {
        Self::with_space_probe(
            concurrency,
            Arc::new(|path: &Path| fs4::available_space(path)),
        )
    }

    pub fn with_space_probe(concurrency: usize, space_probe: SpaceProbe) -> Self {
        let (cancel, _) = watch::channel(false);
        PostProcessor {
            inner: Arc::new(Inner {
                state: Mutex::default(),
                concurrency: concurrency.max(1),
                cancel,
                idle: Notify::new(),
                space_probe,
            }),
        }
    }

    /// 把 `files` 按顺序排进队列，每个文件转成同目录下同名的 `.mp4`。必须在 Tokio 运行时
    /// 中调用。已调用 [`shutdown`](Self::shutdown) 时不接收，返回 `false`。
    pub fn enqueue(
        &self,
        files: Vec<PathBuf>,
        options: ConvertOptions,
        listener: ConvertListener,
    ) -> bool {
        let spawn = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return false;
            }
            for source in files {
                state.queue.push_back(Job {
                    source,
                    options: options.clone(),
                    listener: Arc::clone(&listener),
                });
            }
            let wanted = state.queue.len().min(self.inner.concurrency);
            let spawn = wanted.saturating_sub(state.workers);
            state.workers += spawn;
            spawn
        };
        for _ in 0..spawn {
            tokio::spawn(Arc::clone(&self.inner).work());
        }
        true
    }

    /// 排队中和正在转换的文件数。
    pub fn pending(&self) -> usize {
        let state = lock(&self.inner.state);
        state.queue.len() + state.workers.min(self.inner.concurrency)
    }

    /// 停止接收新文件，放弃排队中的，中止正在转换的（删除不完整的 MP4，保留原件），
    /// 最多等待 `timeout`。超时返回 `false`；此时 ffmpeg 已被要求结束，`kill_on_drop`
    /// 和进程守护保证它不会留在后台。
    pub async fn shutdown(&self, timeout: Duration) -> bool {
        let dropped: Vec<Job> = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            state.queue.drain(..).collect()
        };
        self.inner.cancel.send_replace(true);
        for job in dropped {
            emit(
                &job.listener,
                ConvertEvent::Cancelled {
                    source: job.source,
                    started: false,
                },
            )
            .await;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let idle = self.inner.idle.notified();
            if lock(&self.inner.state).workers == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, idle).await.is_err() {
                return false;
            }
        }
    }
}

impl Inner {
    async fn work(self: Arc<Self>) {
        // 工作协程意外退出（panic）时也要减掉计数，否则退出时会白等。
        struct WorkerGuard {
            inner: Arc<Inner>,
            armed: bool,
        }
        impl Drop for WorkerGuard {
            fn drop(&mut self) {
                if self.armed {
                    lock(&self.inner.state).workers -= 1;
                    self.inner.idle.notify_waiters();
                }
            }
        }
        let mut guard = WorkerGuard {
            inner: Arc::clone(&self),
            armed: true,
        };
        loop {
            let job = {
                let mut state = lock(&self.state);
                match state.queue.pop_front() {
                    Some(job) => job,
                    None => {
                        // 在锁内减计数，保证 enqueue 看到的数目准确。
                        state.workers -= 1;
                        guard.armed = false;
                        drop(state);
                        self.idle.notify_waiters();
                        return;
                    }
                }
            };
            let event = convert_one(
                &job.source,
                &job.options,
                &self.space_probe,
                self.cancel.subscribe(),
                &job.listener,
            )
            .await;
            emit(&job.listener, event).await;
        }
    }
}

/// 转换一个文件，返回最终事件（开始事件在过程中发出）。文件系统操作都放进
/// `spawn_blocking`，保存目录在网络盘上时不会卡住异步线程。
async fn convert_one(
    source: &Path,
    options: &ConvertOptions,
    space_probe: &SpaceProbe,
    mut cancel: watch::Receiver<bool>,
    listener: &ConvertListener,
) -> ConvertEvent {
    let cancelled = |started: bool| ConvertEvent::Cancelled {
        source: source.to_path_buf(),
        started,
    };
    if *cancel.borrow() {
        return cancelled(false);
    }
    let prepared = {
        let (source, probe, reserve) = (
            source.to_path_buf(),
            Arc::clone(space_probe),
            options.reserve_bytes,
        );
        blocking(move || prepare(&source, &probe, reserve)).await
    };
    let (size, target) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(message)) | Err(message) => return failed(source, message).await,
    };
    let temp = part_path(&target);
    emit(
        listener,
        ConvertEvent::Started {
            source: source.to_path_buf(),
            target: target.clone(),
        },
    )
    .await;

    // HEVC 复制进 MP4 时默认标为 hev1，QuickTime、Safari、iOS 打不开，改标 hvc1。
    // 只有 HEVC 才能加这个参数，H.264 加了会报错。读不出编码时按非 HEVC 处理。
    let hevc = match probe_header(&options.ffmpeg_path, source, &mut cancel).await {
        Ok(header) => video_is_hevc(&header),
        Err(_) if *cancel.borrow() => return cancelled(true),
        Err(_) => false,
    };
    let timeout = BASE_TIMEOUT + Duration::from_secs(size / BYTES_PER_TIMEOUT_SEC);
    #[allow(unused_mut)]
    let mut args = convert_args(source, &temp, hevc);
    #[cfg(test)]
    if tests::is_slow(source) {
        // 按实时速率读取，让测试有时间在转换中途退出。
        args.splice(0..0, ["-readrate".into(), "1".into()]);
    }
    let result = run_ffmpeg(&options.ffmpeg_path, &args, timeout, &mut cancel).await;
    let message = match result {
        Run::Cancelled => {
            remove_part(&temp).await;
            return cancelled(true);
        }
        Run::Exited { code: Some(0), .. } => None,
        Run::Exited { code, stderr } => Some(match last_line(&stderr) {
            Some(line) => format!("ffmpeg 出错：{line}"),
            None => format!("ffmpeg 异常退出（退出码 {}）", format_code(code)),
        }),
        Run::TimedOut => Some(format!("转换超过 {} 分钟未完成", timeout.as_secs() / 60)),
        Run::SpawnFailed(error) => Some(format!(
            "无法启动 ffmpeg（{}）：{error}",
            options.ffmpeg_path.display()
        )),
    };
    if let Some(message) = message {
        remove_part(&temp).await;
        return failed(source, message).await;
    }

    let non_empty = {
        let temp = temp.clone();
        blocking(move || std::fs::metadata(&temp).is_ok_and(|meta| meta.len() > 0)).await
    };
    let duration = if non_empty == Ok(true) {
        probe_header(&options.ffmpeg_path, &temp, &mut cancel)
            .await
            .and_then(|header| {
                parse_duration(&header).ok_or_else(|| match last_line(&header) {
                    Some(line) => format!("生成的 MP4 读不出时长：{line}"),
                    None => "生成的 MP4 读不出时长".into(),
                })
            })
    } else {
        Err("生成的 MP4 为空".into())
    };
    let duration_secs = match duration {
        Ok(secs) => secs,
        Err(message) => {
            remove_part(&temp).await;
            if *cancel.borrow() {
                return cancelled(true);
            }
            return failed(source, message).await;
        }
    };

    let finished = {
        let (source, temp, target, delete) = (
            source.to_path_buf(),
            temp.clone(),
            target.clone(),
            options.delete_original,
        );
        blocking(move || {
            let target = place_without_overwrite(&temp, &source, target)?;
            let deleted = if delete {
                match std::fs::remove_file(&source) {
                    Ok(()) => (true, None),
                    Err(error) => (false, Some(error.to_string())),
                }
            } else {
                (false, None)
            };
            Ok::<_, String>((target, deleted))
        })
        .await
    };
    match finished {
        Ok(Ok((target, (original_deleted, delete_error)))) => ConvertEvent::Succeeded {
            source: source.to_path_buf(),
            target,
            duration_secs,
            original_deleted,
            delete_error,
        },
        Ok(Err(message)) | Err(message) => {
            remove_part(&temp).await;
            failed(source, message).await
        }
    }
}

/// 转换前的检查：原件存在且非空，磁盘空间够用，选好目标文件名并清掉旧的临时文件。
fn prepare(
    source: &Path,
    space_probe: &SpaceProbe,
    reserve: u64,
) -> Result<(u64, PathBuf), String> {
    let size = match std::fs::metadata(source) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => meta.len(),
        Ok(_) => return Err("原文件为空".into()),
        Err(error) => return Err(format!("读不到原文件：{error}")),
    };
    let dir = source.parent().ok_or("原文件路径没有所在目录")?;
    // 转完还要留出录制的磁盘阈值，否则其他直播间的录制会因磁盘不足停止。
    let needed = size
        .saturating_add(reserve)
        .saturating_add(SPACE_MARGIN_BYTES);
    // 查询失败时照常转换，空间真的不够时 ffmpeg 会报错。
    if let Ok(free) = space_probe(dir)
        && free < needed
    {
        return Err(format!(
            "磁盘剩余空间不足（剩余 {}，转换后仍需保留录制的磁盘阈值，共需约 {}）",
            format_mb(free),
            format_mb(needed)
        ));
    }
    let target = unique_target(source);
    let temp = part_path(&target);
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("无法清理旧的临时文件：{error}")),
    }
    Ok((size, target))
}

/// 把临时文件放到目标位置，不覆盖已有文件。转换期间同名文件被占用时改用下一个
/// 可用的名字，返回最终路径。
fn place_without_overwrite(
    temp: &Path,
    source: &Path,
    mut target: PathBuf,
) -> Result<PathBuf, String> {
    for _ in 0..100 {
        // 硬链接在目标已存在时失败，不会覆盖；成功后再删掉临时文件。
        match std::fs::hard_link(temp, &target) {
            Ok(()) => {
                remove_quietly(temp);
                return Ok(target);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                target = unique_target(source);
            }
            // 文件系统不支持硬链接（FAT、部分网络盘）：确认目标不存在后再改名。
            Err(_) => {
                if target.exists() {
                    target = unique_target(source);
                    continue;
                }
                return std::fs::rename(temp, &target)
                    .map(|()| target)
                    .map_err(|error| format!("无法把临时文件改名为 MP4：{error}"));
            }
        }
    }
    Err("找不到可用的 MP4 文件名".into())
}

/// 转换失败：说明原件是否还在（读不到原文件时不说「已保留」）。
async fn failed(source: &Path, message: String) -> ConvertEvent {
    let path = source.to_path_buf();
    let original_kept = blocking(move || path.is_file()).await.unwrap_or(false);
    ConvertEvent::Failed {
        source: source.to_path_buf(),
        message,
        original_kept,
    }
}

/// 在阻塞线程池里调用监听器（调度的监听器会写 SQLite）。
async fn emit(listener: &ConvertListener, event: ConvertEvent) {
    let listener = Arc::clone(listener);
    if let Err(error) = tokio::task::spawn_blocking(move || listener(&event)).await {
        eprintln!("转换事件回调出错：{error}");
    }
}

/// 在阻塞线程池里执行文件系统操作。
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| format!("内部错误：{error}"))
}

async fn remove_part(temp: &Path) {
    let temp = temp.to_path_buf();
    let _ = blocking(move || remove_quietly(&temp)).await;
}

/// 转换用的 ffmpeg 参数。`hevc` 为 true 时把视频标为 `hvc1`。
///
/// 不加 `-bsf:a aac_adtstoasc`：它只接受 AAC，遇到 MP3、AC-3 会报错；MP4 封装器遇到
/// ADTS 格式的 AAC 会自动插入它。
pub fn convert_args(source: &Path, temp: &Path, hevc: bool) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        ["-hide_banner", "-nostdin", "-loglevel", "error", "-y", "-i"]
            .iter()
            .map(Into::into)
            .collect();
    args.push(source.as_os_str().to_owned());
    for arg in ["-c", "copy", "-sn", "-dn"] {
        args.push(arg.into());
    }
    if hevc {
        args.extend(["-tag:v".into(), "hvc1".into()]);
    }
    for arg in ["-movflags", "+faststart", "-f", "mp4"] {
        args.push(arg.into());
    }
    args.push(temp.as_os_str().to_owned());
    args
}

/// 第一路视频流是不是 HEVC（`ffmpeg -i` 输出里的 `Video: hevc`）。
pub fn video_is_hevc(header: &str) -> bool {
    header
        .lines()
        .find_map(|line| line.split_once("Video: "))
        .is_some_and(|(_, codec)| codec.starts_with("hevc"))
}

/// 同目录下同名的 `.mp4`；已存在时追加 `_2`、`_3`。
fn unique_target(source: &Path) -> PathBuf {
    let base = source.with_extension("mp4");
    if !base.exists() {
        return base;
    }
    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    (2..)
        .map(|n| source.with_file_name(format!("{stem}_{n}.mp4")))
        .find(|path| !path.exists())
        .unwrap_or(base)
}

/// 删除 `root` 下（最多 [`CLEANUP_DEPTH`] 层子目录）上次崩溃或退出超时留下的
/// `*.mp4.part`，返回删除的个数。只删这一种扩展名，不跟随符号链接。启动时调用，
/// 此时还没有转换在进行。
pub fn clean_partial_files(root: &Path) -> usize {
    fn walk(dir: &Path, depth: usize, removed: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < CLEANUP_DEPTH {
                    walk(&path, depth + 1, removed);
                }
            } else if kind.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.to_ascii_lowercase().ends_with(".mp4.part"))
                && std::fs::remove_file(&path).is_ok()
            {
                *removed += 1;
            }
        }
    }
    let mut removed = 0;
    walk(root, 0, &mut removed);
    removed
}

/// 临时文件：`<名称>.mp4.part`，不在录像库的媒体扩展名里。
fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

enum Run {
    Exited { code: Option<i32>, stderr: String },
    TimedOut,
    Cancelled,
    SpawnFailed(io::Error),
}

async fn run_ffmpeg(
    ffmpeg: &Path,
    args: &[std::ffi::OsString],
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
) -> Run {
    let mut command = Command::new(ffmpeg);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    process_guard::configure(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Run::SpawnFailed(error),
    };
    if let Err(message) = process_guard::attach(&child) {
        eprintln!("转换 MP4：{message}");
    }
    let stderr = child.stderr.take();
    let reader = tokio::spawn(async move {
        let mut collected = Vec::new();
        if let Some(mut stderr) = stderr {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                collected.extend_from_slice(&buf[..n]);
                if collected.len() > STDERR_LIMIT {
                    collected.drain(..collected.len() - STDERR_LIMIT);
                }
            }
        }
        String::from_utf8_lossy(&collected).into_owned()
    });

    enum Wait {
        Exited(io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }
    let outcome = tokio::select! {
        status = child.wait() => Wait::Exited(status),
        _ = tokio::time::sleep(timeout) => Wait::TimedOut,
        _ = cancel.wait_for(|cancelled| *cancelled) => Wait::Cancelled,
    };
    match outcome {
        Wait::Cancelled => {
            let _ = child.kill().await;
            reader.abort();
            Run::Cancelled
        }
        Wait::Exited(status) => {
            let stderr = tokio::time::timeout(Duration::from_secs(3), reader)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            match status {
                Ok(status) => Run::Exited {
                    code: status.code(),
                    stderr,
                },
                Err(error) => Run::SpawnFailed(error),
            }
        }
        Wait::TimedOut => {
            let _ = child.kill().await;
            reader.abort();
            Run::TimedOut
        }
    }
}

/// 用 `ffmpeg -i` 只读文件头（不解码，几十毫秒即可完成），返回它的输出。
async fn probe_header(
    ffmpeg: &Path,
    file: &Path,
    cancel: &mut watch::Receiver<bool>,
) -> Result<String, String> {
    let args: Vec<std::ffi::OsString> =
        vec!["-hide_banner".into(), "-i".into(), file.as_os_str().into()];
    match run_ffmpeg(ffmpeg, &args, PROBE_TIMEOUT, cancel).await {
        // 没有输出文件时 ffmpeg 总是以 1 退出，只看输出内容。
        Run::Exited { stderr, .. } => Ok(stderr),
        Run::TimedOut => Err("读取文件头超时".into()),
        Run::Cancelled => Err("已中止".into()),
        Run::SpawnFailed(error) => Err(format!("无法启动 ffmpeg 读取文件头：{error}")),
    }
}

/// 从 `ffmpeg -i` 的输出里取第一个 `Duration: HH:MM:SS.xx`，必须大于 0。
pub fn parse_duration(stderr: &str) -> Option<f64> {
    let rest = &stderr[stderr.find("Duration:")? + "Duration:".len()..];
    let value = rest.trim_start().split([',', '\r', '\n']).next()?.trim();
    let mut parts = value.split(':');
    let (h, m, s) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let secs =
        h.parse::<f64>().ok()? * 3600.0 + m.parse::<f64>().ok()? * 60.0 + s.parse::<f64>().ok()?;
    (secs.is_finite() && secs > 0.0).then_some(secs)
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty())
}

fn format_code(code: Option<i32>) -> String {
    code.map_or_else(|| "无".into(), |code| code.to_string())
}

fn format_mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / 1024.0 / 1024.0)
}

fn remove_quietly(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("删除不完整的 MP4 失败（{}）：{error}", path.display()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
