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
//! - 录制结束后按设置把 TS 交给 [`PostProcessor`] 转 MP4（逐个排队）。因磁盘不足结束的
//!   不转换，避免把磁盘写满；StreamCap 只要 ffmpeg 正常退出就转换。
//!
//! 平台请求和录制引擎都通过 [`LiveSource`]、[`RecordEngine`] 接入，测试中替换为假实现。
//!
//! 除 [`tauri_glue`] 外，本模块不依赖 Tauri，只用 Tokio，桌面应用和命令行程序共用：
//!
//! ```ignore
//! let tasks = Arc::new(tasks::open_in(&data_dir, &save_dir, Box::new(|_event| {}))?);
//! let ffmpeg = Arc::new(FfmpegManager::new(ManagerConfig::for_current_platform(root), listener));
//! tokio::spawn({ let ffmpeg = Arc::clone(&ffmpeg); async move { ffmpeg.ensure().await; } });
//! let scheduler = Scheduler::with_defaults(tasks, ffmpeg, Box::new(|_status| {}));
//! let join = scheduler.spawn();   // 在 Tokio 运行时中开始轮询
//! tokio::signal::ctrl_c().await?; // 收到退出信号后：
//! scheduler.shutdown().await;     // 停止轮询，停止所有录制并等收尾
//! join.await.ok();
//! ```

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::ffmpeg::FfmpegManager;
use crate::platforms::{BoxFuture, FetchOptions, LiveInfo, Platform, PlatformError};
use crate::recorder::postprocess::{self, ConvertEvent, ConvertListener, PostProcessor};
use crate::recorder::{
    EndReason, EventListener, RecorderEvent, RecordingOutcome, RecordingRequest,
};
use crate::tasks::{
    ActivityKind, LiveState, Monitoring, NewActivity, PlatformErrorKind, RecordState,
    SchedulerHook, TaskId, TaskManager, TaskSnapshot, now_ms,
};

mod engine;
pub mod policy;
pub mod tauri_glue;

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
    /// 等录制结束，返回结果。引擎内部异常退出时也会返回（原因为 FFmpeg 异常退出）。
    fn wait(&self) -> BoxFuture<'static, RecordingOutcome>;
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
    /// 已开始收尾，防止重复处理。
    finishing: bool,
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
    /// 录后转 MP4 的队列。
    post: PostProcessor,
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
            post: PostProcessor::default(),
        });
        let weak: Weak<Core> = Arc::downgrade(&core);
        let hook: Weak<dyn SchedulerHook> = weak;
        core.tasks.set_scheduler_hook(hook);
        Arc::new(Scheduler { core })
    }

    /// 用真实的平台适配器和 FFmpeg 录制引擎创建调度器。
    pub fn with_defaults(
        tasks: Arc<TaskManager>,
        ffmpeg: Arc<FfmpegManager>,
        status_listener: StatusListener,
    ) -> Arc<Scheduler> {
        Scheduler::new(
            tasks,
            Arc::new(PlatformSource),
            Arc::new(FfmpegEngine::new(ffmpeg)),
            status_listener,
        )
    }

    /// 在当前 Tokio 运行时中启动调度循环。
    pub fn spawn(&self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// 调度循环，直到 [`Scheduler::shutdown`]。在 Tokio 运行时中执行。
    pub fn run(&self) -> impl Future<Output = ()> + Send + 'static {
        Arc::clone(&self.core).run()
    }

    /// 录后转 MP4 的队列，可用于查询排队数。退出时由 [`Scheduler::shutdown`] 关闭。
    pub fn post_processor(&self) -> &PostProcessor {
        &self.core.post
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

    /// 检测一次。调用前 `plan` 已标记为检测中并占用平台并发名额；`guard` 保证即使中途
    /// panic 也会归还名额、清除检测中标记。
    async fn check(self: Arc<Self>, task: TaskSnapshot) {
        let id = task.id;
        let mut guard = CheckGuard {
            core: Arc::clone(&self),
            id,
            platform: task.config.platform,
            armed: true,
        };
        self.tasks
            .set_live_state(id, LiveState::Checking { since: now_ms() });
        let settings = self.tasks.settings();
        // 在子任务里请求：适配器 panic 时当作解析失败，不影响调度。
        let fetch = self.source.fetch(
            task.config.url.clone(),
            policy::fetch_options(&task, &settings),
        );
        let result = tokio::spawn(fetch).await.unwrap_or_else(|error| {
            Err(PlatformError::Parse(format!("检测时发生内部错误：{error}")))
        });
        let interval = policy::interval(&settings);

        // 检测期间被暂停或删除的任务丢弃这次结果。
        let current = self
            .tasks
            .get(id)
            .filter(|current| current.monitoring == Monitoring::Enabled);
        let mut start = None;
        let mut delay = interval;
        let mut succeeded = true;
        if let Some(current) = current {
            // 要录制却没有流地址：在写入已开播之前按解析失败处理，让退避生效。
            let result = match result {
                Ok(info)
                    if info.is_live && info.selected.is_none() && wants_recording(&current) =>
                {
                    Err(PlatformError::Parse("已开播，但没有取到直播流地址".into()))
                }
                other => other,
            };
            match result {
                Ok(info) if info.is_live => {
                    let snapshot = self.tasks.set_live_state(
                        id,
                        LiveState::Live {
                            checked_at: now_ms(),
                            title: info.title.clone(),
                        },
                    );
                    if snapshot.as_ref().is_some_and(wants_recording) {
                        start = Some(info);
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

        guard.armed = false;
        self.end_check(id, task.config.platform, succeeded.then_some(()), delay);

        if let Some(info) = start {
            self.start_recording(id, info, interval).await;
        }
    }

    /// 检测结束：归还平台并发名额，排定下一次检测。`succeeded` 为 `Some` 时清零失败计数。
    fn end_check(&self, id: TaskId, platform: Platform, succeeded: Option<()>, delay: Duration) {
        {
            let mut guard = self.lock();
            let state = &mut *guard;
            if let Some(inflight) = state.inflight.get_mut(&platform) {
                *inflight = inflight.saturating_sub(1);
            }
            if let Some(entry) = state.entries.get_mut(&id) {
                entry.checking = false;
                if succeeded.is_some() {
                    entry.failures = 0;
                }
                entry.next_at = Instant::now() + delay;
            }
        }
        self.wake.notify_one();
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

    fn default_save_dir(&self) -> PathBuf {
        PathBuf::from(
            self.tasks
                .defaults()
                .get("live_save_path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        )
    }

    async fn start_recording(self: &Arc<Self>, id: TaskId, info: LiveInfo, interval: Duration) {
        let Some(ffmpeg) = self.engine.ffmpeg_path() else {
            // FFmpeg 还在下载或校验：保持已开播，稍后再检测。
            self.reschedule(id, policy::FFMPEG_WAIT.min(interval));
            return;
        };
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        let settings = self.tasks.settings();
        let plan = match policy::recording_request(
            &task,
            &info,
            &settings,
            ffmpeg,
            &self.default_save_dir(),
        ) {
            Ok(plan) => plan,
            Err(error) => {
                let delay = self.record_check_failure(id, &error, interval);
                self.reschedule(id, delay);
                return;
            }
        };
        let Some(slot_id) = self.reserve(id) else {
            return;
        };
        // 占位之后再核对一次：检测到开播之后、占位之前，用户可能已经暂停监控、改成仅通知
        // 或手动停止。那时 `stop_recording` 找不到占位，只能在这里拦下。
        let Some(task) = self.tasks.get(id).filter(wants_recording) else {
            self.release_slot(id, slot_id);
            return;
        };
        let mut slot_guard = SlotGuard {
            core: Arc::clone(self),
            id,
            slot_id,
            armed: true,
        };
        if let Some(message) = plan.warning {
            self.warn(&task, message);
        }

        let weak = Arc::downgrade(self);
        let listener: EventListener = Arc::new(move |event: &RecorderEvent| {
            if let Some(core) = weak.upgrade() {
                core.handle_event(id, slot_id, event);
            }
        });
        // 在子任务里启动：引擎 panic 时按启动失败收尾，释放占位。
        let start = self.engine.start(plan.request, listener);
        let control = match tokio::spawn(start).await {
            Ok(control) => control,
            Err(error) => {
                slot_guard.armed = false;
                self.finish(
                    id,
                    slot_id,
                    &failed_outcome(format!("启动录制时发生内部错误：{error}")),
                );
                return;
            }
        };
        slot_guard.armed = false;
        {
            let mut guard = self.lock();
            let shutting_down = guard.shutting_down;
            // 启动期间就失败的录制已经收到 `finished`，占位已释放。
            if let Some(slot) = guard.slots.get_mut(&id).filter(|slot| slot.id == slot_id) {
                if slot.stop_requested || shutting_down {
                    control.stop();
                }
                slot.control = Some(Arc::clone(&control));
            }
        }
        // 兜底：引擎内部任务异常退出、没有发出 `finished` 时，靠等待结果收尾。
        // 正常结束时 `finished` 已经处理过，这里不会重复。
        let weak = Arc::downgrade(self);
        let outcome = control.wait();
        tokio::spawn(async move {
            let outcome = outcome.await;
            if let Some(core) = weak.upgrade() {
                core.finish(id, slot_id, &outcome);
            }
        });
    }

    fn warn(&self, task: &TaskSnapshot, message: String) {
        eprintln!("录制提示（任务 {}）：{message}", task.id);
        let _ = self.tasks.record_activity(NewActivity {
            task_id: Some(task.id),
            anchor_name: task.config.anchor_name.clone(),
            kind: ActivityKind::Warning,
            message,
            detail: None,
        });
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
                finishing: false,
                reserved_at: Instant::now(),
                file_path: None,
            },
        );
        drop(state);
        self.wake.notify_one();
        Some(slot_id)
    }

    /// 释放还没启动录制的占位。
    fn release_slot(&self, id: TaskId, slot_id: u64) {
        {
            let mut state = self.lock();
            if state.slots.get(&id).is_some_and(|slot| slot.id == slot_id) {
                state.slots.remove(&id);
            }
        }
        self.slots_changed.notify_waiters();
        self.wake.notify_one();
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
                if let Some(task) = self.tasks.get(id) {
                    self.warn(&task, message.clone());
                }
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

    /// 录制结束：先回写录制状态，再释放占位（退出时等的就是占位清空，状态必须已落盘），
    /// 最后按结束原因安排下一次检测。同一个录制只处理一次。
    fn finish(&self, id: TaskId, slot_id: u64, outcome: &RecordingOutcome) {
        let (file_path, reserved_at) = {
            let mut state = self.lock();
            match state.slots.get_mut(&id) {
                Some(slot) if slot.id == slot_id && !slot.finishing => {
                    slot.finishing = true;
                    (slot.file_path.clone(), slot.reserved_at)
                }
                _ => return,
            }
        };

        if let Some(task) = self.tasks.get(id) {
            let record = if outcome.reason.needs_attention() {
                RecordState::Failed {
                    reason: describe(outcome),
                    file_path: outcome.files.first().cloned().or(file_path),
                    failed_at: outcome.ended_at_ms,
                }
            } else {
                RecordState::Idle
            };
            if let Err(error) = self.tasks.set_record_state(id, record) {
                eprintln!("写入录制结束状态失败（任务 {id}）：{error}");
            }
            self.convert_recording(&task, outcome);
        }

        let interval = policy::interval(&self.tasks.settings());
        {
            let mut state = self.lock();
            state.slots.remove(&id);
            if !state.shutting_down
                && let Some(entry) = state.entries.get_mut(&id)
            {
                let delay = if outcome.reason.should_recheck() {
                    entry.short_runs = if reserved_at.elapsed() < policy::MIN_VALID_RECORDING {
                        entry.short_runs + 1
                    } else {
                        0
                    };
                    policy::recheck_delay(entry.short_runs, interval)
                } else {
                    // 手动停止、需处理：照常按间隔检测，开播时由保持状态或失败状态决定不录。
                    // 暂停后又重新启用的，直播状态已重置为未知，`plan` 会立即检测。
                    interval
                };
                entry.next_at = Instant::now() + delay;
            }
        }
        self.slots_changed.notify_waiters();
        self.wake.notify_one();
    }

    /// 按设置把这次录制的 TS 排进转 MP4 队列。退出期间、因磁盘不足结束的不转换。
    fn convert_recording(&self, task: &TaskSnapshot, outcome: &RecordingOutcome) {
        if outcome.reason == EndReason::DiskFull || self.lock().shutting_down {
            return;
        }
        let files = policy::convertible_files(&outcome.files);
        if files.is_empty() {
            return;
        }
        let Some(ffmpeg) = self.engine.ffmpeg_path() else {
            return;
        };
        let Some(options) = policy::convert_options(&self.tasks.settings(), ffmpeg) else {
            return;
        };
        let tasks = Arc::clone(&self.tasks);
        let (task_id, anchor_name) = (task.id, task.config.anchor_name.clone());
        let listener: ConvertListener = Arc::new(move |event: &ConvertEvent| {
            let (kind, message, detail) = describe_conversion(event);
            let entry = NewActivity {
                task_id: Some(task_id),
                anchor_name: anchor_name.clone(),
                kind,
                message,
                detail: Some(detail),
            };
            if let Err(error) = tasks.record_activity(entry) {
                eprintln!("写入转换动态失败（任务 {task_id}）：{error}");
            }
        });
        self.post.enqueue(files, options, listener);
    }

    async fn shutdown(&self) {
        // 总时限从这里算起，包括 stop_all 本身。转 MP4 队列同时关闭：排队的放弃，
        // 正在转换的中止（最多等 postprocess::SHUTDOWN_TIMEOUT，小于总时限）。
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let post = self.post.shutdown(postprocess::SHUTDOWN_TIMEOUT);
        let recordings = self.stop_recordings(deadline);
        let (post_done, ()) = tokio::join!(post, recordings);
        if !post_done {
            eprintln!("退出时中止转 MP4 超时");
        }
    }

    async fn stop_recordings(&self, deadline: Instant) {
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
        if tokio::time::timeout_at(deadline, self.engine.stop_all())
            .await
            .is_err()
        {
            eprintln!("退出时停止录制超时");
        }
        // 等每个录制都回写结束状态，包括 stop_all 时还没启动完的。
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

/// 检测任务意外退出（panic）时归还平台并发名额，清除检测中标记，稍后重试。
struct CheckGuard {
    core: Arc<Core>,
    id: TaskId,
    platform: Platform,
    armed: bool,
}

impl Drop for CheckGuard {
    fn drop(&mut self) {
        if self.armed {
            self.core
                .end_check(self.id, self.platform, None, policy::RETRY_BASE);
        }
    }
}

/// 启动录制的过程意外退出（panic）时按启动失败收尾，释放占位。
struct SlotGuard {
    core: Arc<Core>,
    id: TaskId,
    slot_id: u64,
    armed: bool,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if self.armed {
            self.core.finish(
                self.id,
                self.slot_id,
                &failed_outcome("启动录制时发生内部错误".to_string()),
            );
        }
    }
}

/// 引擎没有给出结果时（内部错误）使用的结束结果。
fn failed_outcome(message: String) -> RecordingOutcome {
    let now = now_ms();
    RecordingOutcome {
        reason: EndReason::FfmpegFailed,
        message: Some(message),
        exit_code: None,
        files: Vec::new(),
        bytes_written: 0,
        duration_secs: None,
        output_dir: None,
        started_at_ms: now,
        ended_at_ms: now,
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

/// 转换事件对应的动态：类型、说明、附加信息。
fn describe_conversion(event: &ConvertEvent) -> (ActivityKind, String, serde_json::Value) {
    use serde_json::json;
    match event {
        ConvertEvent::Started { source, target } => (
            ActivityKind::Convert,
            "开始转为 MP4".to_string(),
            json!({ "filePath": source, "targetPath": target }),
        ),
        ConvertEvent::Succeeded {
            source,
            target,
            duration_secs,
            original_deleted,
            delete_error,
        } => {
            let message = match (original_deleted, delete_error) {
                (true, _) => "已转为 MP4，已删除原 TS 文件".to_string(),
                (false, Some(error)) => format!("已转为 MP4，但删除原 TS 文件失败：{error}"),
                (false, None) => "已转为 MP4".to_string(),
            };
            (
                ActivityKind::Convert,
                message,
                json!({
                    "filePath": target,
                    "sourcePath": source,
                    "durationSecs": duration_secs,
                    "originalDeleted": original_deleted,
                }),
            )
        }
        ConvertEvent::Failed { source, message } => (
            ActivityKind::Failure,
            format!("转为 MP4 失败：{message}。原 TS 文件已保留"),
            json!({ "filePath": source, "kind": "convert" }),
        ),
        ConvertEvent::Cancelled { source, started } => (
            ActivityKind::Warning,
            if *started {
                "应用退出，转为 MP4 已中止，原 TS 文件已保留".to_string()
            } else {
                "应用退出，未转为 MP4，原 TS 文件已保留".to_string()
            },
            json!({ "filePath": source }),
        ),
    }
}

/// 需处理时显示的原因：中文标签，加上引擎给出的说明。
fn describe(outcome: &RecordingOutcome) -> String {
    match outcome.message.as_deref().map(str::trim) {
        Some(message) if !message.is_empty() => format!("{}：{message}", outcome.reason.label()),
        _ => outcome.reason.label().to_string(),
    }
}
