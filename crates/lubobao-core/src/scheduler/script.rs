//! 录后脚本：一次录制结束、这次录制的转 MP4 全部结束（成功或失败都算）之后，按设置执行一条
//! 用户命令。
//!
//! - 设置 `execute_custom_script` 打开、`custom_script_command` 去空白后非空时生效。
//! - 每次录制执行一次（分段录制也只执行一次）。没有产出文件的录制、因应用退出而结束的录制
//!   不执行。没有开转换或没有可转的文件时，录制结束后就执行。
//! - 命令交给系统命令行：Windows `cmd /S /C "<命令>"`（不弹控制台窗口），其他系统
//!   `sh -c <命令>`；工作目录为录像所在目录，录像信息通过环境变量传入（见 [`environment`]）。
//! - 脚本逐个执行（同时最多一个，其余排队），不阻塞调度和录制；超过 [`TIMEOUT`] 结束整个
//!   进程树。应用退出时（[`ScriptRunner::shutdown`]）排队未开始的丢弃，正在运行的结束进程树，
//!   不为等脚本拖延退出。
//! - 结果写进动态：成功记为「录后脚本」，失败（无法启动、非零退出、超时）和退出时中止记为
//!   提醒（`detail.kind` 为 `script`），带退出码和 stderr 最后一行。

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, watch};

use crate::recorder::postprocess::ConvertEvent;
use crate::recorder::process_guard;
use crate::tasks::{ActivityKind, EndCause, LiveState, NewActivity, TaskManager, TaskSnapshot};

/// 单次脚本的时限，超过后结束进程树。
pub const TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 退出时等正在运行的脚本被结束、写完动态的上限。
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// 保留的 stderr 字节数，用于失败说明。
const STDERR_LIMIT: usize = 16 * 1024;

/// 设置里的录后脚本命令：开关打开且命令去空白后非空时返回命令。
pub fn command(settings: &Map<String, Value>) -> Option<String> {
    let enabled = settings
        .get("execute_custom_script")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let command = settings
        .get("custom_script_command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if !enabled || command.is_empty() {
        return None;
    }
    Some(command.to_string())
}

/// 传给脚本的录像信息（文件和目录之外的部分）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptVars {
    pub anchor: String,
    /// 平台显示名，例如「抖音」。
    pub platform: String,
    /// 直播标题，没有时为空。
    pub title: String,
    pub room_url: String,
}

impl ScriptVars {
    pub fn of(task: &TaskSnapshot) -> ScriptVars {
        let title = match &task.live {
            LiveState::Live { title, .. } => title.clone().unwrap_or_default(),
            _ => String::new(),
        };
        ScriptVars {
            anchor: task.config.anchor_name.clone(),
            platform: task.platform_name.to_string(),
            title,
            room_url: task.config.url.clone(),
        }
    }
}

/// 一次录制的最终文件：转出了 MP4 的 TS 换成 MP4（原件即使保留也不再列出），其余用原文件，
/// 保持原顺序。不检查文件是否存在，见 [`existing`]。
pub fn final_files(files: &[PathBuf], converted: &HashMap<PathBuf, PathBuf>) -> Vec<PathBuf> {
    files
        .iter()
        .map(|file| converted.get(file).unwrap_or(file).clone())
        .collect()
}

/// 只留下磁盘上存在的文件。
pub fn existing(files: Vec<PathBuf>) -> Vec<PathBuf> {
    files.into_iter().filter(|file| file.is_file()).collect()
}

/// 脚本的环境变量。`files` 至少有一个。
pub fn environment(
    files: &[PathBuf],
    dir: &Path,
    vars: &ScriptVars,
) -> Vec<(&'static str, OsString)> {
    let mut list = OsString::new();
    for (index, file) in files.iter().enumerate() {
        if index > 0 {
            list.push("\n");
        }
        list.push(file.as_os_str());
    }
    let first = files.first().map(PathBuf::as_os_str).unwrap_or_default();
    vec![
        ("LUBOBAO_FILE", first.to_owned()),
        ("LUBOBAO_FILES", list),
        ("LUBOBAO_DIR", dir.as_os_str().to_owned()),
        ("LUBOBAO_ANCHOR", vars.anchor.clone().into()),
        ("LUBOBAO_PLATFORM", vars.platform.clone().into()),
        ("LUBOBAO_TITLE", vars.title.clone().into()),
        ("LUBOBAO_ROOM_URL", vars.room_url.clone().into()),
    ]
}

/// 录像所在目录：第一个文件的上级目录。
fn directory_of(files: &[PathBuf]) -> PathBuf {
    files
        .first()
        .and_then(|file| file.parent())
        .filter(|dir| !dir.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// 脚本失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptFailure {
    /// 无法启动命令行。
    Spawn(String),
    /// 非零退出；`None` 为被信号结束。
    Exit(Option<i32>),
    /// 超时，进程树已结束。
    TimedOut(Duration),
}

/// 一次脚本的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptEvent {
    Succeeded {
        files: Vec<PathBuf>,
    },
    Failed {
        files: Vec<PathBuf>,
        failure: ScriptFailure,
        /// stderr 最后一个非空行。
        stderr: Option<String>,
    },
    /// 应用退出时中止（`started` 为 `true`）或丢弃排队中的脚本。
    Cancelled {
        started: bool,
    },
    /// 执行前发现录像文件都已不在，没有执行。
    Skipped,
}

pub type ScriptListener = Arc<dyn Fn(&ScriptEvent) + Send + Sync>;

/// 排队等待执行的一次脚本。
pub struct ScriptJob {
    pub command: String,
    /// 最终文件；执行前去掉磁盘上已经不存在的。
    pub files: Vec<PathBuf>,
    pub vars: ScriptVars,
    pub listener: ScriptListener,
}

#[derive(Default)]
struct State {
    queue: VecDeque<ScriptJob>,
    running: bool,
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    timeout: Duration,
    cancel: watch::Sender<bool>,
    idle: Notify,
}

/// 录后脚本队列：同时只执行一个。可 Clone，各份共用同一个队列。
#[derive(Clone)]
pub struct ScriptRunner {
    inner: Arc<Inner>,
}

impl Default for ScriptRunner {
    fn default() -> Self {
        Self::new(TIMEOUT)
    }
}

impl ScriptRunner {
    /// 创建队列，每次脚本最多运行 `timeout`。不需要在 Tokio 运行时中调用。
    pub fn new(timeout: Duration) -> Self {
        let (cancel, _) = watch::channel(false);
        ScriptRunner {
            inner: Arc::new(Inner {
                state: Mutex::default(),
                timeout,
                cancel,
                idle: Notify::new(),
            }),
        }
    }

    /// 把脚本排进队列。必须在 Tokio 运行时中调用。已调用 [`shutdown`](Self::shutdown) 时
    /// 不接收，返回 `false`。
    pub fn enqueue(&self, job: ScriptJob) -> bool {
        let spawn = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return false;
            }
            state.queue.push_back(job);
            !std::mem::replace(&mut state.running, true)
        };
        if spawn {
            tokio::spawn(Arc::clone(&self.inner).work());
        }
        true
    }

    /// 排队中和正在执行的脚本数。
    pub fn pending(&self) -> usize {
        let state = lock(&self.inner.state);
        state.queue.len() + usize::from(state.running)
    }

    /// 停止接收新脚本，丢弃排队中的，结束正在运行的进程树，最多等 `wait`。超时返回
    /// `false`。关闭在调用时立即生效，不必等返回的 future 被轮询。
    pub fn shutdown(&self, wait: Duration) -> impl Future<Output = bool> + Send + 'static {
        let dropped: Vec<ScriptJob> = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            state.queue.drain(..).collect()
        };
        self.inner.cancel.send_replace(true);
        let inner = Arc::clone(&self.inner);
        async move {
            for job in dropped {
                log::info!("应用退出，丢弃排队中的录后脚本：{}", job.command);
                emit(&job.listener, ScriptEvent::Cancelled { started: false }).await;
            }
            let deadline = tokio::time::Instant::now() + wait;
            loop {
                let idle = inner.idle.notified();
                if !lock(&inner.state).running {
                    return true;
                }
                if tokio::time::timeout_at(deadline, idle).await.is_err() {
                    return false;
                }
            }
        }
    }
}

impl Inner {
    async fn work(self: Arc<Self>) {
        // 意外退出（panic）时也要清掉运行标记，否则退出时会白等，之后的脚本也不再执行。
        struct WorkerGuard {
            inner: Arc<Inner>,
            armed: bool,
        }
        impl Drop for WorkerGuard {
            fn drop(&mut self) {
                if self.armed {
                    lock(&self.inner.state).running = false;
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
                        state.running = false;
                        guard.armed = false;
                        drop(state);
                        self.idle.notify_waiters();
                        return;
                    }
                }
            };
            let event = run_job(&job, self.timeout, self.cancel.subscribe()).await;
            emit(&job.listener, event).await;
        }
    }
}

async fn run_job(
    job: &ScriptJob,
    timeout: Duration,
    mut cancel: watch::Receiver<bool>,
) -> ScriptEvent {
    if *cancel.borrow() {
        return ScriptEvent::Cancelled { started: false };
    }
    let candidates = job.files.clone();
    let files = tokio::task::spawn_blocking(move || existing(candidates))
        .await
        .unwrap_or_default();
    if files.is_empty() {
        log::info!("录后脚本：录像文件都已不在，不执行");
        return ScriptEvent::Skipped;
    }
    let dir = directory_of(&files);
    let env = environment(&files, &dir, &job.vars);
    let (shown, command) = (dir.display(), &job.command);
    log::info!("执行录后脚本（目录 {shown}）：{command}");
    let run = run_command(&job.command, &dir, &env, timeout, &mut cancel).await;
    let (failure, stderr) = match run {
        Run::Exited { code: Some(0), .. } => return ScriptEvent::Succeeded { files },
        Run::Exited { code, stderr } => (ScriptFailure::Exit(code), stderr),
        Run::TimedOut { stderr } => (ScriptFailure::TimedOut(timeout), stderr),
        Run::SpawnFailed(error) => (ScriptFailure::Spawn(error.to_string()), String::new()),
        Run::Cancelled => return ScriptEvent::Cancelled { started: true },
    };
    ScriptEvent::Failed {
        files,
        failure,
        stderr: last_line(&stderr).map(str::to_string),
    }
}

enum Run {
    Exited { code: Option<i32>, stderr: String },
    TimedOut { stderr: String },
    Cancelled,
    SpawnFailed(io::Error),
}

/// 交给系统命令行执行的命令。
#[cfg(windows)]
fn shell(script: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut command = Command::new("cmd.exe");
    // 原样传参：标准库按 C 运行时规则给参数加引号、转义，cmd 不认这套规则。`/S` 让 cmd
    // 只去掉最外层的一对引号，命令里的引号保持原样。
    let arg = format!("/S /C \"{script}\"");
    command.as_std_mut().raw_arg(arg);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(not(windows))]
fn shell(script: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    // 单独的进程组：结束时连同脚本启动的子进程一起结束，终端的 Ctrl+C 也不会直接发给它。
    command.as_std_mut().process_group(0);
    command
}

async fn run_command(
    script: &str,
    dir: &Path,
    env: &[(&'static str, OsString)],
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
) -> Run {
    let mut command = shell(script);
    command
        .current_dir(dir)
        .envs(env.iter().map(|(name, value)| (*name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    process_guard::configure(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Run::SpawnFailed(error),
    };
    if let Err(message) = process_guard::attach(&child) {
        log::warn!("录后脚本：{message}");
    }
    let stderr = child.stderr.take();
    let mut reader = tokio::spawn(async move {
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
        Wait::Exited(Ok(status)) => Run::Exited {
            code: status.code(),
            stderr: collect_stderr(&mut reader, Duration::from_secs(3)).await,
        },
        Wait::Exited(Err(error)) => Run::SpawnFailed(error),
        Wait::TimedOut => {
            kill_tree(&mut child).await;
            Run::TimedOut {
                stderr: collect_stderr(&mut reader, Duration::from_secs(1)).await,
            }
        }
        Wait::Cancelled => {
            kill_tree(&mut child).await;
            reader.abort();
            Run::Cancelled
        }
    }
}

/// 取收集到的 stderr。脚本启动的后台进程可能一直占着 stderr，最多再等 `wait`。
async fn collect_stderr(reader: &mut tokio::task::JoinHandle<String>, wait: Duration) -> String {
    match tokio::time::timeout(wait, &mut *reader).await {
        Ok(Ok(stderr)) => stderr,
        _ => {
            reader.abort();
            String::new()
        }
    }
}

/// 结束脚本和它启动的子进程。
async fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let pid = pid.to_string();
        let mut taskkill = Command::new("taskkill");
        taskkill
            .args(["/PID", pid.as_str(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        let _ = tokio::time::timeout(Duration::from_secs(5), taskkill.status()).await;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(pid) = child.id() {
        // SAFETY: 只发信号。脚本用 `process_group(0)` 启动，进程组号就是它的进程号。
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
}

/// 在阻塞线程池里调用监听器（调度的监听器会写 SQLite）。
async fn emit(listener: &ScriptListener, event: ScriptEvent) {
    let listener = Arc::clone(listener);
    if let Err(error) = tokio::task::spawn_blocking(move || listener(&event)).await {
        log::warn!("录后脚本回调出错：{error}");
    }
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---- 与录制、转换衔接 ----

/// 一次录制结束后待执行的脚本：等这次录制排进转换队列的文件都有了结果再排队执行。
///
/// 调度在排队转换前调用 [`converting`](Self::converting)，在转换事件里调用
/// [`observe`](Self::observe)，不再排队转换后调用 [`release`](Self::release)。三者都到齐时
/// 才排进脚本队列；任何一个转换因退出被中止时不执行。
#[derive(Clone)]
pub(super) struct AfterRecording {
    pending: Arc<Mutex<Pending>>,
}

struct Pending {
    runner: ScriptRunner,
    handle: tokio::runtime::Handle,
    /// 执行后清空，保证只执行一次。
    job: Option<ScriptJob>,
    waiting: HashSet<PathBuf>,
    converted: HashMap<PathBuf, PathBuf>,
    released: bool,
    cancelled: bool,
}

impl AfterRecording {
    /// 按设置为这次录制准备脚本。开关关闭、命令为空、没有产出文件、因应用退出而结束时
    /// 返回 `None`。必须在 Tokio 运行时中调用。
    pub(super) fn for_recording(
        runner: &ScriptRunner,
        tasks: &Arc<TaskManager>,
        task: &TaskSnapshot,
        files: &[PathBuf],
        cause: EndCause,
    ) -> Option<AfterRecording> {
        if files.is_empty() || cause == EndCause::Shutdown {
            return None;
        }
        let command = command(&tasks.settings())?;
        AfterRecording::new(
            runner,
            command,
            files.to_vec(),
            ScriptVars::of(task),
            activity_listener(Arc::clone(tasks), task),
        )
    }

    pub(super) fn new(
        runner: &ScriptRunner,
        command: String,
        files: Vec<PathBuf>,
        vars: ScriptVars,
        listener: ScriptListener,
    ) -> Option<AfterRecording> {
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(error) => {
                log::warn!("录后脚本：不在 Tokio 运行时中，不执行（{error}）");
                return None;
            }
        };
        Some(AfterRecording {
            pending: Arc::new(Mutex::new(Pending {
                runner: runner.clone(),
                handle,
                job: Some(ScriptJob {
                    command,
                    files,
                    vars,
                    listener,
                }),
                waiting: HashSet::new(),
                converted: HashMap::new(),
                released: false,
                cancelled: false,
            })),
        })
    }

    /// 这些文件已排进转换队列，等它们的结果。
    pub(super) fn converting(&self, files: &[PathBuf]) {
        lock(&self.pending).waiting.extend(files.iter().cloned());
    }

    /// 收到一个转换事件。
    pub(super) fn observe(&self, event: &ConvertEvent) {
        let mut pending = lock(&self.pending);
        match event {
            ConvertEvent::Started { .. } => return,
            ConvertEvent::Succeeded { source, target, .. } => {
                pending.converted.insert(source.clone(), target.clone());
                pending.waiting.remove(source);
            }
            ConvertEvent::Failed { source, .. } => {
                pending.waiting.remove(source);
            }
            ConvertEvent::Cancelled { source, .. } => {
                pending.cancelled = true;
                pending.waiting.remove(source);
            }
        }
        pending.try_start();
    }

    /// 这次录制不再有文件排进转换队列。
    pub(super) fn release(&self) {
        let mut pending = lock(&self.pending);
        pending.released = true;
        pending.try_start();
    }
}

impl Pending {
    fn try_start(&mut self) {
        if !self.released || !self.waiting.is_empty() {
            return;
        }
        let Some(mut job) = self.job.take() else {
            return;
        };
        if self.cancelled {
            log::info!("应用退出，转换已中止，不执行录后脚本");
            return;
        }
        job.files = final_files(&job.files, &self.converted);
        let _entered = self.handle.enter();
        if !self.runner.enqueue(job) {
            log::info!("应用正在退出，不执行录后脚本");
        }
    }
}

/// 把脚本结果写进这个任务的动态，同时写日志。
fn activity_listener(tasks: Arc<TaskManager>, task: &TaskSnapshot) -> ScriptListener {
    let (task_id, anchor_name) = (task.id, task.config.anchor_name.clone());
    Arc::new(move |event: &ScriptEvent| {
        let Some((kind, message, detail)) = describe(event) else {
            return;
        };
        if kind == ActivityKind::Script {
            log::info!("{message}（任务 {task_id}）");
        } else {
            log::warn!("{message}（任务 {task_id}）");
        }
        let entry = NewActivity {
            task_id: Some(task_id),
            anchor_name: anchor_name.clone(),
            kind,
            message,
            detail: Some(detail),
        };
        if let Err(error) = tasks.record_activity(entry) {
            log::error!("写入录后脚本动态失败（任务 {task_id}）：{error}");
        }
    })
}

/// 脚本结果对应的动态：类型、说明、附加信息。丢弃排队和跳过的不记动态。
pub fn describe(event: &ScriptEvent) -> Option<(ActivityKind, String, Value)> {
    match event {
        ScriptEvent::Succeeded { files } => Some((
            ActivityKind::Script,
            "录后脚本已执行".to_string(),
            json!({ "kind": "script", "filePath": files.first(), "files": files }),
        )),
        ScriptEvent::Failed {
            files,
            failure,
            stderr,
        } => {
            let mut message = failure_message(failure);
            let exit_code = match failure {
                ScriptFailure::Exit(code) => *code,
                _ => None,
            };
            if let Some(line) = stderr {
                message.push('：');
                message.push_str(line);
            }
            Some((
                ActivityKind::Warning,
                message,
                json!({
                    "kind": "script",
                    "filePath": files.first(),
                    "files": files,
                    "exitCode": exit_code,
                    "stderr": stderr,
                }),
            ))
        }
        ScriptEvent::Cancelled { started: true } => Some((
            ActivityKind::Warning,
            "应用退出，录后脚本已中止".to_string(),
            json!({ "kind": "script" }),
        )),
        ScriptEvent::Cancelled { started: false } | ScriptEvent::Skipped => None,
    }
}

fn failure_message(failure: &ScriptFailure) -> String {
    match failure {
        ScriptFailure::Spawn(error) => format!("录后脚本无法启动：{error}"),
        ScriptFailure::Exit(Some(code)) => format!("录后脚本执行失败（退出码 {code}）"),
        ScriptFailure::Exit(None) => "录后脚本执行失败（进程被结束）".to_string(),
        ScriptFailure::TimedOut(limit) => {
            let limit = format_limit(*limit);
            format!("录后脚本超过 {limit}未结束，已结束进程")
        }
    }
}

fn format_limit(limit: Duration) -> String {
    let secs = limit.as_secs();
    if secs >= 60 {
        format!("{} 分钟", secs / 60)
    } else {
        format!("{secs} 秒")
    }
}

#[cfg(test)]
mod tests;
