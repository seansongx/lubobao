//! 录后处理：把录好的 TS 逐个转成同名 MP4（只换封装，不重新编码）。
//!
//! 调用 [`PostProcessor::enqueue`] 把一次录制的文件排进队列，转换在后台按顺序进行，
//! 结果通过 [`ConvertListener`] 回调发出。应用退出时调用 [`PostProcessor::shutdown`]：
//! 正在转换的中止并删除不完整的 MP4，排队中的放弃，原件都保留。本模块不依赖 Tauri，
//! 桌面应用和命令行程序共用。
//!
//! 行为参照 StreamCap `stream_manager.py` 的 `converts_mp4`，有意差异：
//!
//! - ffmpeg 参数：`-c copy -bsf:a aac_adtstoasc -movflags +faststart -f mp4`，另加
//!   `-sn -dn`（TS 里的字幕、数据流 MP4 装不下，会让整个转换失败）。StreamCap 只用
//!   `-c:v copy -c:a copy -f mp4`。
//! - 先写到 `<名称>.mp4.part`，读得出时长后再改名为 `.mp4`；失败时删除 `.part`。
//!   StreamCap 直接写目标文件，失败时留下不完整的 MP4。
//! - 成功的判断除退出码为 0 外，还要求输出非空、能读出时长（`ffmpeg -i` 只读文件头）。
//! - 不删除原件时，TS 留在原处。StreamCap 会把它移进同目录的 `original/` 子目录。
//! - 同名 MP4 已存在时不覆盖，改名为 `_2`、`_3`。
//! - 同时只转换 [`DEFAULT_CONCURRENCY`] 个文件，其余排队，不与录制抢 CPU 和磁盘。
//!   StreamCap 每个文件各开一个协程，分段多时同时转换。
//! - 转换前检查磁盘剩余空间，不足以写下与原件同样大小的 MP4 时直接失败。
//! - 退出时中止转换（StreamCap 交给后台服务继续转换）。下次启动不会补做。

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
/// 保留的 stderr 字节数，用于失败说明。
const STDERR_LIMIT: usize = 16 * 1024;

/// 转换选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertOptions {
    /// 已校验的 ffmpeg 可执行文件。
    pub ffmpeg_path: PathBuf,
    /// 确认 MP4 有效后删除原件。StreamCap `delete_original` 默认 `false`。
    pub delete_original: bool,
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
    /// 转换失败，原件保留，不完整的 MP4 已删除。
    Failed {
        source: PathBuf,
        message: String,
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
            (job.listener)(&ConvertEvent::Cancelled {
                source: job.source,
                started: false,
            });
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
            (job.listener)(&event);
        }
    }
}

/// 转换一个文件，返回最终事件（开始事件在过程中发出）。
async fn convert_one(
    source: &Path,
    options: &ConvertOptions,
    space_probe: &SpaceProbe,
    mut cancel: watch::Receiver<bool>,
    listener: &ConvertListener,
) -> ConvertEvent {
    let failed = |message: String| ConvertEvent::Failed {
        source: source.to_path_buf(),
        message,
    };
    if *cancel.borrow() {
        return ConvertEvent::Cancelled {
            source: source.to_path_buf(),
            started: false,
        };
    }
    let size = match std::fs::metadata(source) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => meta.len(),
        Ok(_) => return failed("原文件为空".into()),
        Err(error) => return failed(format!("读不到原文件：{error}")),
    };
    let Some(dir) = source.parent() else {
        return failed("原文件路径没有所在目录".into());
    };
    match space_probe(dir) {
        Ok(free) if free < size.saturating_add(SPACE_MARGIN_BYTES) => {
            return failed(format!(
                "磁盘剩余空间不足（剩余 {}，需要约 {}）",
                format_mb(free),
                format_mb(size + SPACE_MARGIN_BYTES)
            ));
        }
        // 查询失败时照常转换，空间真的不够时 ffmpeg 会报错。
        _ => {}
    }
    let target = unique_target(source);
    let temp = part_path(&target);
    let _ = std::fs::remove_file(&temp);
    listener(&ConvertEvent::Started {
        source: source.to_path_buf(),
        target: target.clone(),
    });

    let timeout = BASE_TIMEOUT + Duration::from_secs(size / BYTES_PER_TIMEOUT_SEC);
    #[allow(unused_mut)]
    let mut args = convert_args(source, &temp);
    #[cfg(test)]
    if tests::is_slow(source) {
        // 按实时速率读取，让测试有时间在转换中途退出。
        args.splice(0..0, ["-readrate".into(), "1".into()]);
    }
    let result = run_ffmpeg(&options.ffmpeg_path, &args, timeout, &mut cancel).await;
    let message = match result {
        Run::Cancelled => {
            remove_quietly(&temp);
            return ConvertEvent::Cancelled {
                source: source.to_path_buf(),
                started: true,
            };
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
        remove_quietly(&temp);
        return failed(message);
    }

    let duration = match std::fs::metadata(&temp) {
        Ok(meta) if meta.len() > 0 => {
            probe_duration(&options.ffmpeg_path, &temp, &mut cancel).await
        }
        _ => Err("生成的 MP4 为空".into()),
    };
    let duration_secs = match duration {
        Ok(secs) => secs,
        Err(message) => {
            remove_quietly(&temp);
            if *cancel.borrow() {
                return ConvertEvent::Cancelled {
                    source: source.to_path_buf(),
                    started: true,
                };
            }
            return failed(message);
        }
    };
    if let Err(error) = std::fs::rename(&temp, &target) {
        remove_quietly(&temp);
        return failed(format!("无法把临时文件改名为 MP4：{error}"));
    }

    let (original_deleted, delete_error) = if options.delete_original {
        match std::fs::remove_file(source) {
            Ok(()) => (true, None),
            Err(error) => (false, Some(error.to_string())),
        }
    } else {
        (false, None)
    };
    ConvertEvent::Succeeded {
        source: source.to_path_buf(),
        target,
        duration_secs,
        original_deleted,
        delete_error,
    }
}

/// 转换用的 ffmpeg 参数。
pub fn convert_args(source: &Path, temp: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        ["-hide_banner", "-nostdin", "-loglevel", "error", "-y", "-i"]
            .iter()
            .map(Into::into)
            .collect();
    args.push(source.as_os_str().to_owned());
    for arg in [
        "-c",
        "copy",
        "-sn",
        "-dn",
        "-bsf:a",
        "aac_adtstoasc",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
    ] {
        args.push(arg.into());
    }
    args.push(temp.as_os_str().to_owned());
    args
}

/// 同目录下同名的 `.mp4`；已存在（或临时文件被占用）时追加 `_2`、`_3`。
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

/// 用 `ffmpeg -i` 只读文件头，取 `Duration:` 行。不解码，几十毫秒即可完成。
async fn probe_duration(
    ffmpeg: &Path,
    file: &Path,
    cancel: &mut watch::Receiver<bool>,
) -> Result<f64, String> {
    let args: Vec<std::ffi::OsString> =
        vec!["-hide_banner".into(), "-i".into(), file.as_os_str().into()];
    match run_ffmpeg(ffmpeg, &args, PROBE_TIMEOUT, cancel).await {
        // 没有输出文件时 ffmpeg 总是以 1 退出，只看输出内容。
        Run::Exited { stderr, .. } => {
            parse_duration(&stderr).ok_or_else(|| match last_line(&stderr) {
                Some(line) => format!("生成的 MP4 读不出时长：{line}"),
                None => "生成的 MP4 读不出时长".into(),
            })
        }
        Run::TimedOut => Err("读取 MP4 时长超时".into()),
        Run::Cancelled => Err("已中止".into()),
        Run::SpawnFailed(error) => Err(format!("无法启动 ffmpeg 读取时长：{error}")),
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
