//! 调度：按检测间隔轮询监控中的任务，开播后自动录制，录制结束后决定下一步。
//!
//! 行为参照 StreamCap `record_manager.py`、`stream_manager.py`，有意差异如下：
//! - 手动停止录制后，本次直播不再自动录制，直到检测到下播或用户重新启用监控。
//!   StreamCap 在下一轮检测时会重置 `manually_stopped` 并重新开始录制。
//! - 断流或网络错误结束后立即重新检测，仍在播就续录。StreamCap 对抖音等 FLV 优先的
//!   平台不立即重检，要等下一轮。录制不足 60 秒就结束的，重检前按 5 秒起翻倍退避。
//! - 检测失败按类别退避：网络、限流、解析失败 10 秒起翻倍，不超过检测间隔；需要用户
//!   处理的失败至少隔 10 分钟，修改设置后立即重试。StreamCap 按固定间隔重试。
//! - 暂停监控与 StreamCap `stop_monitor_recording` 一样同时停止录制。
//!
//! 平台请求和录制引擎都通过 [`LiveSource`]、[`RecordEngine`] 接入，测试中替换为假实现。
//! Rust 持有全部状态；前端通过 [`get_scheduler_status`] 和 `scheduler-status` 事件取得
//! 下次检测时间。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde::Serialize;
use tauri::{Emitter, Manager};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::ffmpeg::FfmpegManager;
use crate::platforms::{BoxFuture, FetchOptions, LiveInfo, Platform, PlatformError};
use crate::recorder::{EventListener, RecorderEvent, RecordingOutcome, RecordingRequest};
use crate::tasks::{
    LiveState, Monitoring, PlatformErrorKind, RecordState, SchedulerHook, TaskId, TaskManager,
    TaskSnapshot, now_ms,
};

mod engine;
pub mod policy;

#[cfg(test)]
mod tests;

pub use engine::{FfmpegEngine, PlatformSource};

pub const STATUS_EVENT: &str = "scheduler-status";

/// 没有更早的事件时，调度循环最多睡这么久就重新核对一次任务列表。
const IDLE_TICK: Duration = Duration::from_secs(1);
/// 退出时等待所有录制回写结束状态的上限（录制引擎每个录制最多等 15 秒）。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// 查询直播间状态。
pub trait LiveSource: Send + Sync {
    fn fetch(
        &self,
        url: String,
        options: FetchOptions,
    ) -> BoxFuture<'static, Result<LiveInfo, PlatformError>>;
}

/// 一个正在进行的录制。
pub trait RecordingControl: Send + Sync {
    /// 请求正常停止，立即返回。
    fn stop(&self);
}

/// 录制引擎。
pub trait RecordEngine: Send + Sync {
    /// 已校验的 ffmpeg；还没装好时为 `None`。
    fn ffmpeg_path(&self) -> Option<PathBuf>;
    /// 开始录制。事件经 `listener` 送回，最后一个总是 `finished`。
    fn start(
        &self,
        request: RecordingRequest,
        listener: EventListener,
    ) -> BoxFuture<'static, Arc<dyn RecordingControl>>;
    /// 停止所有录制并等待收尾。
    fn stop_all(&self) -> BoxFuture<'static, ()>;
}

/// 状态栏用的调度概况。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerStatus {
    /// 最近一次排定的检测时间（Unix 毫秒）。没有排定的检测时为 `null`，例如没有监控中的
    /// 任务，或者所有任务都在检测或录制。
    pub next_check_at: Option<i64>,
    /// 正在请求平台的任务数。
    pub checking: usize,
    /// 正在录制（含启动中、收尾中）的任务数。
    pub recording: usize,
}

pub type StatusListener = Box<dyn Fn(&SchedulerStatus) + Send + Sync>;

/// 一个监控中任务的排期。
struct Entry {
    next_at: Instant,
    checking: bool,
    /// 连续检测失败次数。
    failures: u32,
    /// 连续「秒断」的录制次数。
    short_runs: u32,
}

/// 一个任务的录制。从决定开始录制起就占位，直到收到 `finished`，保证同一任务同时
/// 只有一个录制。
struct Slot {
    id: u64,
    control: Option<Arc<dyn RecordingControl>>,
    stop_requested: bool,
    reserved_at: Instant,
    file_path: Option<PathBuf>,
}

#[derive(Default)]
struct State {
    entries: HashMap<TaskId, Entry>,
    slots: HashMap<TaskId, Slot>,
    inflight: HashMap<Platform, usize>,
    next_slot: u64,
    shutting_down: bool,
    settings_changed: bool,
    published: Option<(Option<Instant>, usize, usize)>,
}

struct Core {
    tasks: Arc<TaskManager>,
    source: Arc<dyn LiveSource>,
    engine: Arc<dyn RecordEngine>,
    state: Mutex<State>,
    wake: Notify,
    slots_changed: Notify,
    status_listener: StatusListener,
}

pub struct Scheduler {
    core: Arc<Core>,
}

impl Scheduler {
    /// 创建调度器并登记到任务核心。调用 [`Scheduler::run`] 开始轮询。
    pub fn new(
        tasks: Arc<TaskManager>,
        source: Arc<dyn LiveSource>,
        engine: Arc<dyn RecordEngine>,
        status_listener: StatusListener,
    ) -> Arc<Scheduler> {
        let core = Arc::new(Core {
            tasks,
            source,
            engine,
            state: Mutex::default(),
            wake: Notify::new(),
            slots_changed: Notify::new(),
            status_listener,
        });
        let weak: Weak<Core> = Arc::downgrade(&core);
        let hook: Weak<dyn SchedulerHook> = weak;
        core.tasks.set_scheduler_hook(hook);
        Arc::new(Scheduler { core })
    }

    /// 调度循环，直到 [`Scheduler::shutdown`]。在 Tokio 运行时中执行。
    pub fn run(&self) -> impl Future<Output = ()> + Send + 'static {
        Arc::clone(&self.core).run()
    }

    pub fn status(&self) -> SchedulerStatus {
        let state = self.core.lock();
        Core::status_of(&state).0
    }

    /// 停止轮询，停止所有录制并等它们回写结束状态。之后不再开始新的检测或录制。
    pub async fn shutdown(&self) {
        self.core.shutdown().await;
    }
}

impl Core {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn run(self: Arc<Self>) {
        loop {
            if self.lock().shutting_down {
                break;
            }
            let now = Instant::now();
            for task in self.plan(now) {
                let core = Arc::clone(&self);
                tokio::spawn(async move { core.check(task).await });
            }
            self.publish_status();
            let deadline = self.next_deadline(now);
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    /// 与任务列表核对排期，挑出到期且未超出平台并发上限的任务，标记为检测中。
    fn plan(&self, now: Instant) -> Vec<TaskSnapshot> {
        let monitored = self.tasks.monitored();
        let limit = policy::max_concurrent(&self.tasks.settings());
        let mut guard = self.lock();
        let state = &mut *guard;
        let ids: HashSet<TaskId> = monitored.iter().map(|task| task.id).collect();
        // 暂停或删除的任务不再排期；正在检测的等检测结束再清理。
        state
            .entries
            .retain(|id, entry| ids.contains(id) || entry.checking);
        let settings_changed = std::mem::take(&mut state.settings_changed);

        let mut due = Vec::new();
        for task in monitored {
            let entry = state.entries.entry(task.id).or_insert(Entry {
                next_at: now,
                checking: false,
                failures: 0,
                short_runs: 0,
            });
            if entry.checking {
                continue;
            }
            // 启动后、重新启用监控后直播状态为未知：立即检测。
            if task.live == LiveState::Unknown {
                entry.next_at = now;
            }
            if settings_changed
                && matches!(task.live, LiveState::CheckFailed { kind, .. } if !kind.is_retryable())
            {
                entry.next_at = now;
            }
            // 录制期间不检测，录制结束后再决定。
            if state.slots.contains_key(&task.id) || task.record.is_active() {
                continue;
            }
            if entry.next_at <= now {
                due.push((entry.next_at, task));
            }
        }
        due.sort_by_key(|(at, task)| (*at, task.id));

        let mut picked = Vec::new();
        for (_, task) in due {
            let inflight = state.inflight.entry(task.config.platform).or_default();
            if *inflight >= limit {
                continue;
            }
            *inflight += 1;
            if let Some(entry) = state.entries.get_mut(&task.id) {
                entry.checking = true;
            }
            picked.push(task);
        }
        picked
    }

    fn next_deadline(&self, now: Instant) -> Instant {
        let state = self.lock();
        let idle = now + IDLE_TICK;
        // 已到期却因并发上限排队的任务不算：等检测结束时的唤醒，否则会空转。
        state
            .entries
            .iter()
            .filter(|(id, entry)| !entry.checking && !state.slots.contains_key(id))
            .map(|(_, entry)| entry.next_at)
            .filter(|at| *at > now)
            .min()
            .map_or(idle, |at| at.min(idle))
    }

    /// 排定的最早一次检测。
    fn earliest(state: &State) -> Option<Instant> {
        state
            .entries
            .iter()
            .filter(|(id, entry)| !entry.checking && !state.slots.contains_key(id))
            .map(|(_, entry)| entry.next_at)
            .min()
    }

    fn status_of(state: &State) -> (SchedulerStatus, (Option<Instant>, usize, usize)) {
        let earliest = Self::earliest(state);
        let checking = state
            .entries
            .values()
            .filter(|entry| entry.checking)
            .count();
        let recording = state.slots.len();
        let next_check_at = earliest.map(|at| {
            let remaining = at.saturating_duration_since(Instant::now());
            now_ms() + remaining.as_millis() as i64
        });
        (
            SchedulerStatus {
                next_check_at,
                checking,
                recording,
            },
            (earliest, checking, recording),
        )
    }

    fn publish_status(&self) {
        let status = {
            let mut state = self.lock();
            let (status, key) = Self::status_of(&state);
            if state.published == Some(key) {
                return;
            }
            state.published = Some(key);
            status
        };
        (self.status_listener)(&status);
    }

    /// 检测一次。调用前 `plan` 已标记为检测中并占用平台并发名额。
    async fn check(self: Arc<Self>, task: TaskSnapshot) {
        let id = task.id;
        self.tasks
            .set_live_state(id, LiveState::Checking { since: now_ms() });
        let settings = self.tasks.settings();
        let result = self
            .source
            .fetch(
                task.config.url.clone(),
                policy::fetch_options(&task, &settings),
            )
            .await;
        let interval = policy::interval(&settings);

        // 检测期间被暂停或删除的任务丢弃这次结果。
        let still_monitored = self
            .tasks
            .get(id)
            .is_some_and(|current| current.monitoring == Monitoring::Enabled);
        let mut start = None;
        let mut delay = interval;
        let mut succeeded = true;
        if still_monitored {
            match result {
                Ok(info) if info.is_live => {
                    let snapshot = self.tasks.set_live_state(
                        id,
                        LiveState::Live {
                            checked_at: now_ms(),
                            title: info.title.clone(),
                        },
                    );
                    if let Some(snapshot) = snapshot.filter(wants_recording) {
                        start = Some((snapshot, info));
                    }
                }
                Ok(_) => {
                    self.tasks.set_live_state(
                        id,
                        LiveState::Offline {
                            checked_at: now_ms(),
                        },
                    );
                }
                Err(error) => {
                    succeeded = false;
                    delay = self.record_check_failure(id, &error, interval);
                }
            }
        }

        {
            let mut guard = self.lock();
            let state = &mut *guard;
            if let Some(inflight) = state.inflight.get_mut(&task.config.platform) {
                *inflight = inflight.saturating_sub(1);
            }
            if let Some(entry) = state.entries.get_mut(&id) {
                entry.checking = false;
                if succeeded {
                    entry.failures = 0;
                }
                entry.next_at = Instant::now() + delay;
            }
        }
        self.wake.notify_one();

        if let Some((snapshot, info)) = start {
            self.start_recording(snapshot, info, interval).await;
        }
    }

    /// 写入检测失败，返回到下一次检测的等待时长。
    fn record_check_failure(
        &self,
        id: TaskId,
        error: &PlatformError,
        interval: Duration,
    ) -> Duration {
        let kind = PlatformErrorKind::of(error);
        let attempt = self
            .lock()
            .entries
            .get_mut(&id)
            .map(|entry| {
                entry.failures += 1;
                entry.failures
            })
            .unwrap_or(1);
        let delay = policy::retry_delay(kind, attempt, interval);
        let failed_at = now_ms();
        self.tasks.set_live_state(
            id,
            LiveState::CheckFailed {
                kind,
                message: error.message().to_string(),
                failed_at,
                next_retry_at: Some(failed_at + delay.as_millis() as i64),
                attempt,
            },
        );
        delay
    }

    fn reschedule(&self, id: TaskId, delay: Duration) {
        if let Some(entry) = self.lock().entries.get_mut(&id) {
            entry.next_at = Instant::now() + delay;
        }
        self.wake.notify_one();
    }

    async fn start_recording(
        self: &Arc<Self>,
        task: TaskSnapshot,
        info: LiveInfo,
        interval: Duration,
    ) {
        let id = task.id;
        let Some(ffmpeg) = self.engine.ffmpeg_path() else {
            // FFmpeg 还在下载或校验：保持已开播，稍后再检测。
            self.reschedule(id, policy::FFMPEG_WAIT.min(interval));
            return;
        };
        let settings = self.tasks.settings();
        let request = match policy::recording_request(&task, &info, &settings, ffmpeg) {
            Ok(request) => request,
            Err(error) => {
                let delay = self.record_check_failure(id, &error, interval);
                self.reschedule(id, delay);
                return;
            }
        };
        let Some(slot_id) = self.reserve(id) else {
            return;
        };
        let weak = Arc::downgrade(self);
        let listener: EventListener = Arc::new(move |event: &RecorderEvent| {
            if let Some(core) = weak.upgrade() {
                core.handle_event(id, slot_id, event);
            }
        });
        let control = self.engine.start(request, listener).await;
        let mut guard = self.lock();
        let shutting_down = guard.shutting_down;
        // 启动期间就失败的录制已经收到 `finished`，占位已释放。
        if let Some(slot) = guard.slots.get_mut(&id).filter(|slot| slot.id == slot_id) {
            if slot.stop_requested || shutting_down {
                control.stop();
            }
            slot.control = Some(control);
        }
    }

    /// 为任务占一个录制位置。已有录制或正在退出时返回 `None`。
    fn reserve(&self, id: TaskId) -> Option<u64> {
        let mut state = self.lock();
        if state.shutting_down || state.slots.contains_key(&id) {
            return None;
        }
        state.next_slot += 1;
        let slot_id = state.next_slot;
        state.slots.insert(
            id,
            Slot {
                id: slot_id,
                control: None,
                stop_requested: false,
                reserved_at: Instant::now(),
                file_path: None,
            },
        );
        drop(state);
        self.wake.notify_one();
        Some(slot_id)
    }

    fn handle_event(&self, id: TaskId, slot_id: u64, event: &RecorderEvent) {
        match event {
            RecorderEvent::Started {
                output_path,
                started_at_ms,
                ..
            } => {
                {
                    let mut state = self.lock();
                    match state.slots.get_mut(&id) {
                        Some(slot) if slot.id == slot_id => {
                            slot.file_path = Some(output_path.clone());
                        }
                        _ => return,
                    }
                }
                let recording = RecordState::Recording {
                    file_path: output_path.clone(),
                    started_at: *started_at_ms,
                    bytes_written: 0,
                    bitrate_kbps: 0.0,
                };
                if self.tasks.set_record_state(id, recording).is_err() {
                    // 任务在启动期间被删除。
                    self.stop_slot(id);
                }
            }
            RecorderEvent::Progress { progress } => {
                if let Some(bytes) = progress.size_bytes {
                    self.tasks.update_record_progress(
                        id,
                        bytes,
                        progress.bitrate_kbps.unwrap_or(0.0),
                    );
                }
            }
            RecorderEvent::Stopping { .. } => {
                if let Some(task) = self.tasks.get(id)
                    && let RecordState::Recording {
                        file_path,
                        started_at,
                        ..
                    } = task.record
                {
                    let _ = self.tasks.set_record_state(
                        id,
                        RecordState::Finishing {
                            file_path,
                            started_at,
                        },
                    );
                }
            }
            RecorderEvent::Warning { message } => {
                eprintln!("录制警告（任务 {id}）：{message}");
            }
            RecorderEvent::Finished { outcome } => self.finish(id, slot_id, outcome),
        }
    }

    fn stop_slot(&self, id: TaskId) -> bool {
        let mut state = self.lock();
        let Some(slot) = state.slots.get_mut(&id) else {
            return false;
        };
        slot.stop_requested = true;
        if let Some(control) = &slot.control {
            control.stop();
        }
        true
    }

    /// 录制结束：回写录制状态，再按结束原因安排下一次检测。
    fn finish(&self, id: TaskId, slot_id: u64, outcome: &RecordingOutcome) {
        let slot = {
            let mut state = self.lock();
            match state.slots.get(&id) {
                Some(slot) if slot.id == slot_id => state.slots.remove(&id),
                _ => None,
            }
        };
        let Some(slot) = slot else {
            return;
        };
        self.slots_changed.notify_waiters();

        if self.tasks.get(id).is_some() {
            let record = if outcome.reason.needs_attention() {
                RecordState::Failed {
                    reason: describe(outcome),
                    file_path: outcome.files.first().cloned().or(slot.file_path),
                    failed_at: outcome.ended_at_ms,
                }
            } else {
                RecordState::Idle
            };
            if let Err(error) = self.tasks.set_record_state(id, record) {
                eprintln!("写入录制结束状态失败（任务 {id}）：{error}");
            }
        }

        let interval = policy::interval(&self.tasks.settings());
        let mut state = self.lock();
        if state.shutting_down {
            return;
        }
        if let Some(entry) = state.entries.get_mut(&id) {
            let delay = if outcome.reason.should_recheck() {
                entry.short_runs = if slot.reserved_at.elapsed() < policy::MIN_VALID_RECORDING {
                    entry.short_runs + 1
                } else {
                    0
                };
                policy::recheck_delay(entry.short_runs, interval)
            } else {
                // 手动停止、需处理：照常按间隔检测，开播时由保持状态或失败状态决定不录。
                interval
            };
            entry.next_at = Instant::now() + delay;
        }
        drop(state);
        self.wake.notify_one();
    }

    async fn shutdown(&self) {
        let controls: Vec<Arc<dyn RecordingControl>> = {
            let mut state = self.lock();
            state.shutting_down = true;
            state
                .slots
                .values_mut()
                .filter_map(|slot| {
                    slot.stop_requested = true;
                    slot.control.clone()
                })
                .collect()
        };
        for control in controls {
            control.stop();
        }
        self.wake.notify_one();
        self.engine.stop_all().await;
        // 等每个录制都回写结束状态，包括 stop_all 时还没启动完的。
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            let changed = self.slots_changed.notified();
            let remaining = self.lock().slots.len();
            if remaining == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                eprintln!("退出时仍有 {remaining} 个录制未结束");
                break;
            }
        }
    }
}

impl SchedulerHook for Core {
    fn wake(&self) {
        self.wake.notify_one();
    }

    fn stop_recording(&self, id: TaskId) -> bool {
        self.stop_slot(id)
    }

    fn settings_changed(&self) {
        self.lock().settings_changed = true;
        self.wake.notify_one();
    }
}

/// 检测到开播后是否开始录制。
fn wants_recording(task: &TaskSnapshot) -> bool {
    task.monitoring == Monitoring::Enabled
        && !task.config.notify_only
        && !task.record_held
        && !task.record.is_active()
        && !matches!(task.record, RecordState::Failed { .. })
}

/// 需处理时显示的原因：中文标签，加上引擎给出的说明。
fn describe(outcome: &RecordingOutcome) -> String {
    match outcome.message.as_deref().map(str::trim) {
        Some(message) if !message.is_empty() => format!("{}：{message}", outcome.reason.label()),
        _ => outcome.reason.label().to_string(),
    }
}

/// 在 Tauri `setup` 中、`tasks::init` 和 `ffmpeg::init` 之后调用：创建调度器并在后台开始轮询。
/// 任务数据没打开时（`tasks::init` 已提示退出）不启动。
pub fn init(app: &tauri::AppHandle) -> tauri::Result<()> {
    let Some(tasks) = app.try_state::<Arc<TaskManager>>() else {
        return Ok(());
    };
    let ffmpeg = app.state::<Arc<FfmpegManager>>();
    let handle = app.clone();
    let scheduler = Scheduler::new(
        Arc::clone(tasks.inner()),
        Arc::new(PlatformSource),
        Arc::new(FfmpegEngine::new(Arc::clone(ffmpeg.inner()))),
        Box::new(move |status: &SchedulerStatus| {
            let _ = handle.emit(STATUS_EVENT, status);
        }),
    );
    tauri::async_runtime::spawn(scheduler.run());
    app.manage(scheduler);
    Ok(())
}

static EXIT_STARTED: AtomicBool = AtomicBool::new(false);
static EXIT_READY: AtomicBool = AtomicBool::new(false);

/// 处理 `RunEvent::ExitRequested`：第一次先阻止退出，停止调度和所有录制、等文件收尾后
/// 再退出；收尾完成后的退出请求直接放行。
pub fn on_exit_requested(app: &tauri::AppHandle, api: &tauri::ExitRequestApi) {
    if EXIT_READY.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_exit();
    if EXIT_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match app.try_state::<Arc<Scheduler>>() {
            Some(scheduler) => scheduler.shutdown().await,
            None => {
                crate::recorder::stop_all().await;
            }
        }
        EXIT_READY.store(true, Ordering::SeqCst);
        app.exit(0);
    });
}

/// 调度概况，供状态栏显示「下次检测」。变化时另有 `scheduler-status` 事件。
#[tauri::command]
pub fn get_scheduler_status(scheduler: tauri::State<'_, Arc<Scheduler>>) -> SchedulerStatus {
    scheduler.status()
}
