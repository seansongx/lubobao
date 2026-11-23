//! 任务核心：任务模型、SQLite 持久化、动态记录、设置，以及给前端的命令和事件。
//!
//! [`TaskManager`] 持有全部任务状态。前端通过桌面端 `src-tauri/src/tasks/commands.rs` 中的 Tauri 命令读取快照和
//! 提交操作，通过 `task-updated`、`task-removed`、`activity` 事件接收变化。后续的轮询
//! 调度和录制引擎通过 `set_live_state`、`set_record_state`、`update_record_progress`
//! 回写状态，由这里负责持久化、写动态和推送事件。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::platforms::{LiveInfo, Platform, PlatformError, Quality, adapter_for};

pub mod activity;
pub mod dedup;
pub mod model;
pub mod settings;
pub mod store;

#[cfg(test)]
mod tests;

pub use activity::{ActivityEntry, ActivityKind, ActivityPage, ActivityQuery, NewActivity};
pub use model::{
    DisplayStatus, LiveState, Monitoring, PlatformErrorKind, RecordState, Task, TaskConfig, TaskId,
    TaskSnapshot, derive_display, now_ms,
};
use store::{Store, StoreError};

pub const TASK_UPDATED_EVENT: &str = "task-updated";
pub const TASK_REMOVED_EVENT: &str = "task-removed";
pub const ACTIVITY_EVENT: &str = "activity";

/// 启动时发现上次录制未正常结束，写入动态所用的说明。
pub const INTERRUPTED_REASON: &str = "录制异常中断：上次退出时录制未正常结束";

/// 推送给前端的变化。
#[derive(Debug, Clone, PartialEq)]
pub enum TaskEvent {
    Updated(Box<TaskSnapshot>),
    Removed(Vec<TaskId>),
    Activity(ActivityEntry),
}

pub type Listener = Box<dyn Fn(TaskEvent) + Send + Sync>;

/// 调度器提供给任务核心的回调。任务核心不持有调度器（用 `Weak` 避免循环引用），
/// 调用时不持有任务锁。
pub trait SchedulerHook: Send + Sync {
    /// 任务集合或监控意图变了，请尽快重新排期（新任务、恢复监控立即检测）。
    fn wake(&self);
    /// 请求停止这个任务的录制（包括正在启动的录制）。有录制时返回 true。`origin` 写进
    /// 「录制结束」动态，区分手动停止和暂停监控。
    fn stop_recording(&self, id: TaskId, origin: StopOrigin) -> bool;
    /// 设置已保存，下一轮按新设置检测。
    fn settings_changed(&self);
}

/// 用户发起的停止录制来自哪个操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOrigin {
    /// 「停止录制」。
    Manual,
    /// 「暂停监控」。
    Paused,
    /// 删除任务。
    Deleted,
}

/// 录制正常结束的原因，写进「录制结束」动态（`detail.endCause`）。需要处理的结束
/// （磁盘不足、FFmpeg 异常）记为录制失败，不在这里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EndCause {
    /// 用户点了「停止录制」或 CLI `stop`。
    Manual,
    /// 暂停监控时一并停止。
    Paused,
    /// 应用或 `run` 退出。
    Shutdown,
    /// 直播结束或断流。
    StreamEnded,
    NetworkError,
    /// 其他停止（例如任务已删除）。
    Stopped,
}

impl EndCause {
    pub fn label(self) -> &'static str {
        match self {
            EndCause::Manual => "手动停止",
            EndCause::Paused => "暂停监控",
            EndCause::Shutdown => "应用退出",
            EndCause::StreamEnded => "直播结束或断流",
            EndCause::NetworkError => "网络错误",
            EndCause::Stopped => "已停止",
        }
    }
}

/// 一次录制结束时实际生成的文件和结束原因。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingEnd {
    /// 实际生成的文件，分段时按序号排列；为空时用录制中记下的文件。
    pub files: Vec<PathBuf>,
    pub cause: EndCause,
}

/// 「录制结束」动态的说明：写明原因，多个分段时注明段数。
pub fn record_end_message(cause: EndCause, segments: usize) -> String {
    if segments > 1 {
        format!("录制结束：{}，共 {segments} 个分段", cause.label())
    } else {
        format!("录制结束：{}", cause.label())
    }
}

/// 命令失败时返回给前端的结构化错误：`{ kind, message }`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    // 与 `PlatformError` 一一对应。
    Network,
    InvalidUrl,
    AuthRequired,
    RateLimited,
    Parse,
    Config,
    /// 没有适配器能识别这个链接。
    UnsupportedPlatform,
    NotFound,
    InvalidInput,
    /// 任务正在录制，不能执行该操作。
    RecordingActive,
    /// 功能尚未接入。
    NotImplemented,
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandError {
    pub kind: ErrorKind,
    pub message: String,
}

impl CommandError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        CommandError {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CommandError {}

impl From<StoreError> for CommandError {
    fn from(error: StoreError) -> Self {
        CommandError::new(ErrorKind::Storage, error.0)
    }
}

impl From<PlatformError> for CommandError {
    fn from(error: PlatformError) -> Self {
        let kind = match PlatformErrorKind::of(&error) {
            PlatformErrorKind::Network => ErrorKind::Network,
            PlatformErrorKind::InvalidUrl => ErrorKind::InvalidUrl,
            PlatformErrorKind::AuthRequired => ErrorKind::AuthRequired,
            PlatformErrorKind::RateLimited => ErrorKind::RateLimited,
            PlatformErrorKind::Parse => ErrorKind::Parse,
            PlatformErrorKind::Config => ErrorKind::Config,
        };
        CommandError::new(kind, error.message())
    }
}

pub type CommandResult<T> = Result<T, CommandError>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AddTaskInput {
    pub url: String,
    /// 缺省时用设置 `record_quality`。
    pub quality: Option<Quality>,
    /// 缺省时用设置 `only_notify_no_record`。
    pub notify_only: Option<bool>,
    pub note: Option<String>,
    /// 用户确认重复后强制添加。
    pub force: bool,
}

/// 只更新给出的字段。改链接时会重新解析并检查重复。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateTaskInput {
    pub id: TaskId,
    pub url: Option<String>,
    pub quality: Option<Quality>,
    pub notify_only: Option<bool>,
    pub note: Option<String>,
    pub force: bool,
}

/// 解析链接得到的直播间信息，重复时返回给前端用于提示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRoom {
    pub url: String,
    pub platform: Platform,
    pub platform_name: &'static str,
    pub room_id: String,
    pub anchor_id: Option<String>,
    pub anchor_name: String,
    pub live_url: Option<String>,
    pub is_live: bool,
    pub title: Option<String>,
}

impl ResolvedRoom {
    pub fn from_live_info(url: &str, info: &LiveInfo) -> Self {
        ResolvedRoom {
            url: url.trim().to_string(),
            platform: info.platform,
            platform_name: info.platform.display_name(),
            room_id: info.room_id.clone(),
            anchor_id: info.anchor_id.clone(),
            anchor_name: info.anchor_name.clone(),
            live_url: info.live_url.clone(),
            is_live: info.is_live,
            title: info.title.clone(),
        }
    }
}

/// `add_task`、`update_task` 的结果。重复时不写入，由前端确认后带 `force` 重新提交。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "result",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TaskMutation {
    Added {
        task: TaskSnapshot,
    },
    Updated {
        task: TaskSnapshot,
    },
    Duplicate {
        existing: Vec<TaskSnapshot>,
        candidate: ResolvedRoom,
    },
}

struct Inner {
    store: Store,
    tasks: BTreeMap<TaskId, Task>,
    /// 最近一次成功检测的结果，用来判断开播、下播的转变；检测失败不改变它。
    last_known_live: HashMap<TaskId, bool>,
    /// 最近一次检测失败、尚未成功过的任务。连续失败（中间穿插检测中）只写一条动态。
    failing: HashSet<TaskId>,
}

pub struct TaskManager {
    inner: Mutex<Inner>,
    defaults: Map<String, Value>,
    listener: Listener,
    hook: OnceLock<Weak<dyn SchedulerHook>>,
}

fn user_action(task: &Task, message: &str, detail: Option<Value>) -> NewActivity {
    NewActivity {
        task_id: Some(task.id),
        anchor_name: task.config.anchor_name.clone(),
        kind: ActivityKind::UserAction,
        message: message.into(),
        detail,
    }
}

impl TaskManager {
    /// 读出任务。无法识别的行跳过并记日志；上次未正常结束的录制各写一条动态，任务回到空闲。
    pub fn open(store: Store, save_dir: &Path, listener: Listener) -> CommandResult<TaskManager> {
        let manager = Self::load(store, save_dir, listener)?;
        manager.recover_interrupted()?;
        Ok(manager)
    }

    /// 读出任务，不处理上次异常中断的录制，不写数据库。配合 [`Store::open_read_only`]
    /// 给只读查询用；之后的写操作会失败。
    pub fn open_read_only(
        store: Store,
        save_dir: &Path,
        listener: Listener,
    ) -> CommandResult<TaskManager> {
        Self::load(store, save_dir, listener)
    }

    fn load(store: Store, save_dir: &Path, listener: Listener) -> CommandResult<TaskManager> {
        let (tasks, skipped) = store.db().load_tasks()?;
        for row in &skipped {
            eprintln!("跳过无法读取的任务 #{}：{}", row.id, row.reason);
        }
        let manager = TaskManager {
            inner: Mutex::new(Inner {
                store,
                tasks: tasks.into_iter().map(|task| (task.id, task)).collect(),
                last_known_live: HashMap::new(),
                failing: HashSet::new(),
            }),
            defaults: settings::defaults(save_dir),
            listener,
            hook: OnceLock::new(),
        };
        Ok(manager)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 登记调度器。只能登记一次，之后的调用被忽略。
    pub fn set_scheduler_hook(&self, hook: Weak<dyn SchedulerHook>) {
        let _ = self.hook.set(hook);
    }

    fn scheduler(&self) -> Option<Arc<dyn SchedulerHook>> {
        self.hook.get().and_then(Weak::upgrade)
    }

    fn wake_scheduler(&self) {
        if let Some(hook) = self.scheduler() {
            hook.wake();
        }
    }

    fn emit_all(&self, events: Vec<TaskEvent>) {
        for event in events {
            (self.listener)(event);
        }
    }

    /// 录制中、收尾中的任务说明上次退出时录制被打断。文件已经写到哪里就是哪里，用户
    /// 无从「处理」，所以只写一条失败动态，任务回到空闲，等下一次检测重新开始录制。
    fn recover_interrupted(&self) -> CommandResult<()> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let interrupted: Vec<Task> = inner
            .tasks
            .values()
            .filter(|task| task.record.is_active())
            .cloned()
            .collect();
        if interrupted.is_empty() {
            return Ok(());
        }
        let entries = inner.store.transaction(|db| {
            let mut entries = Vec::new();
            for task in &interrupted {
                let file_path = match &task.record {
                    RecordState::Recording { file_path, .. }
                    | RecordState::Finishing { file_path, .. } => Some(file_path.clone()),
                    _ => None,
                };
                db.set_record_state(task.id, &RecordState::Idle)?;
                entries.push(db.insert_activity(&NewActivity {
                    task_id: Some(task.id),
                    anchor_name: task.config.anchor_name.clone(),
                    kind: ActivityKind::Failure,
                    message: INTERRUPTED_REASON.into(),
                    detail: Some(json!({ "filePath": file_path, "previous": task.record })),
                })?);
            }
            Ok(entries)
        })?;
        for task in &interrupted {
            if let Some(task) = inner.tasks.get_mut(&task.id) {
                task.record = RecordState::Idle;
            }
        }
        drop(guard);
        self.emit_all(entries.into_iter().map(TaskEvent::Activity).collect());
        Ok(())
    }

    // ---- 查询 ----

    pub fn list(&self) -> Vec<TaskSnapshot> {
        self.lock().tasks.values().map(Task::snapshot).collect()
    }

    pub fn get(&self, id: TaskId) -> Option<TaskSnapshot> {
        self.lock().tasks.get(&id).map(Task::snapshot)
    }

    /// 监控已启用的任务，供调度使用。
    pub fn monitored(&self) -> Vec<TaskSnapshot> {
        self.lock()
            .tasks
            .values()
            .filter(|task| task.monitoring == Monitoring::Enabled)
            .map(Task::snapshot)
            .collect()
    }

    // ---- 添加与编辑 ----

    /// 识别平台并实际请求一次直播间，按设置使用这个平台的代理和 Cookie。
    pub async fn resolve(
        url: &str,
        quality: Quality,
        settings: &Map<String, Value>,
    ) -> CommandResult<LiveInfo> {
        let url = url.trim();
        if url.is_empty() {
            return Err(CommandError::new(
                ErrorKind::InvalidInput,
                "请输入直播间链接",
            ));
        }
        let adapter = adapter_for(url).ok_or_else(|| {
            CommandError::new(
                ErrorKind::UnsupportedPlatform,
                format!("暂不支持这个链接的平台：{url}"),
            )
        })?;
        let options = settings::fetch_options(settings, adapter.platform(), quality);
        Ok(adapter.fetch(url, &options).await?)
    }

    pub fn default_quality(&self) -> Quality {
        self.settings()
            .get("record_quality")
            .and_then(Value::as_str)
            .map(Quality::parse_lenient)
            .unwrap_or_default()
    }

    pub async fn add_task(&self, input: AddTaskInput) -> CommandResult<TaskMutation> {
        let quality = input.quality.unwrap_or_else(|| self.default_quality());
        let info = Self::resolve(&input.url, quality, &self.settings()).await?;
        self.add_resolved(input, &info)
    }

    /// 用已解析的直播间信息添加任务。去重检查与写入在同一把锁内完成；初始直播状态
    /// 取自这次解析，不必等第一次轮询。
    pub fn add_resolved(
        &self,
        input: AddTaskInput,
        info: &LiveInfo,
    ) -> CommandResult<TaskMutation> {
        let settings = self.settings();
        let config = TaskConfig {
            url: input.url.trim().to_string(),
            platform: info.platform,
            room_id: info.room_id.clone(),
            anchor_id: info.anchor_id.clone(),
            anchor_name: info.anchor_name.clone(),
            live_url: info.live_url.clone(),
            quality: input.quality.unwrap_or_else(|| {
                settings
                    .get("record_quality")
                    .and_then(Value::as_str)
                    .map(Quality::parse_lenient)
                    .unwrap_or_default()
            }),
            notify_only: input.notify_only.unwrap_or_else(|| {
                settings
                    .get("only_notify_no_record")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            }),
            note: input.note.unwrap_or_default(),
            created_at: now_ms(),
        };
        let checked_at = now_ms();
        let live = if info.is_live {
            LiveState::Live {
                checked_at,
                title: info.title.clone(),
            }
        } else {
            LiveState::Offline { checked_at }
        };

        let mut guard = self.lock();
        let inner = &mut *guard;
        let duplicates = dedup::find_duplicates(inner.tasks.values(), &config, None);
        if !duplicates.is_empty() && !input.force {
            return Ok(TaskMutation::Duplicate {
                existing: duplicates
                    .iter()
                    .filter_map(|id| inner.tasks.get(id).map(Task::snapshot))
                    .collect(),
                candidate: ResolvedRoom::from_live_info(&config.url, info),
            });
        }
        let detail = json!({ "url": config.url, "forced": !duplicates.is_empty() });
        let (id, entry) = inner.store.transaction(|db| {
            let id = db.insert_task(&config, Monitoring::Enabled)?;
            let entry = db.insert_activity(&NewActivity {
                task_id: Some(id),
                anchor_name: config.anchor_name.clone(),
                kind: ActivityKind::UserAction,
                message: "添加任务".into(),
                detail: Some(detail),
            })?;
            Ok((id, entry))
        })?;
        let task = Task {
            id,
            config,
            monitoring: Monitoring::Enabled,
            live,
            record: RecordState::Idle,
            held: false,
        };
        let snapshot = task.snapshot();
        inner.tasks.insert(id, task);
        inner.last_known_live.insert(id, info.is_live);
        drop(guard);
        self.emit_all(vec![
            TaskEvent::Updated(Box::new(snapshot.clone())),
            TaskEvent::Activity(entry),
        ]);
        self.wake_scheduler();
        Ok(TaskMutation::Added { task: snapshot })
    }

    pub async fn update_task(&self, input: UpdateTaskInput) -> CommandResult<TaskMutation> {
        let current = self.get(input.id).ok_or_else(|| not_found(input.id))?;
        let new_url =
            input.url.as_deref().map(str::trim).filter(|url| {
                dedup::normalize_url(url) != dedup::normalize_url(&current.config.url)
            });
        let info = match new_url {
            Some(url) => {
                let quality = input.quality.unwrap_or(current.config.quality);
                Some(Self::resolve(url, quality, &self.settings()).await?)
            }
            None => None,
        };
        self.update_resolved(input, info.as_ref())
    }

    /// `info` 为新链接的解析结果；链接未变时为 `None`。
    pub fn update_resolved(
        &self,
        input: UpdateTaskInput,
        info: Option<&LiveInfo>,
    ) -> CommandResult<TaskMutation> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let task = inner
            .tasks
            .get(&input.id)
            .cloned()
            .ok_or_else(|| not_found(input.id))?;
        let mut config = task.config.clone();
        let relinked = match (info, input.url.as_deref()) {
            (Some(info), Some(url)) => {
                if task.record.is_active() {
                    return Err(CommandError::new(
                        ErrorKind::RecordingActive,
                        "正在录制，停止录制后才能修改链接",
                    ));
                }
                config.url = url.trim().to_string();
                config.platform = info.platform;
                config.room_id = info.room_id.clone();
                config.anchor_id = info.anchor_id.clone();
                config.anchor_name = info.anchor_name.clone();
                config.live_url = info.live_url.clone();
                let duplicates =
                    dedup::find_duplicates(inner.tasks.values(), &config, Some(task.id));
                if !duplicates.is_empty() && !input.force {
                    return Ok(TaskMutation::Duplicate {
                        existing: duplicates
                            .iter()
                            .filter_map(|id| inner.tasks.get(id).map(Task::snapshot))
                            .collect(),
                        candidate: ResolvedRoom::from_live_info(&config.url, info),
                    });
                }
                Some(info)
            }
            _ => None,
        };
        if let Some(quality) = input.quality {
            config.quality = quality;
        }
        if let Some(notify_only) = input.notify_only {
            config.notify_only = notify_only;
        }
        if let Some(note) = input.note {
            config.note = note;
        }

        let mut events = Vec::new();
        if config != task.config {
            let entry = inner.store.transaction(|db| {
                db.update_task_config(task.id, &config)?;
                let mut updated = task.clone();
                updated.config = config.clone();
                db.insert_activity(&user_action(&updated, "修改任务", None))
            })?;
            let updated = inner.tasks.get_mut(&task.id).expect("task exists");
            updated.config = config;
            if let Some(info) = relinked {
                let checked_at = now_ms();
                updated.live = if info.is_live {
                    LiveState::Live {
                        checked_at,
                        title: info.title.clone(),
                    }
                } else {
                    LiveState::Offline { checked_at }
                };
                inner.last_known_live.insert(task.id, info.is_live);
                inner.failing.remove(&task.id);
            }
            events.push(TaskEvent::Activity(entry));
        }
        let snapshot = inner.tasks[&task.id].snapshot();
        drop(guard);
        events.insert(0, TaskEvent::Updated(Box::new(snapshot.clone())));
        self.emit_all(events);
        self.wake_scheduler();
        Ok(TaskMutation::Updated { task: snapshot })
    }

    // ---- 批量操作 ----

    /// 删除任务。任一任务正在录制时整批不删除。整批在一个事务里完成。返回实际删除的 id。
    pub fn delete_tasks(&self, ids: &[TaskId]) -> CommandResult<Vec<TaskId>> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let targets: Vec<Task> = ids
            .iter()
            .filter_map(|id| inner.tasks.get(id).cloned())
            .collect();
        if let Some(task) = targets.iter().find(|task| task.record.is_active()) {
            return Err(CommandError::new(
                ErrorKind::RecordingActive,
                format!("{} 正在录制，停止录制后才能删除", task.config.anchor_name),
            ));
        }
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let entries = inner.store.transaction(|db| {
            let mut entries = Vec::new();
            for task in &targets {
                db.delete_task(task.id)?;
                entries.push(db.insert_activity(&user_action(
                    task,
                    "删除任务",
                    Some(json!({ "url": task.config.url })),
                ))?);
            }
            Ok(entries)
        })?;
        let removed: Vec<TaskId> = targets.iter().map(|task| task.id).collect();
        for id in &removed {
            inner.tasks.remove(id);
            inner.last_known_live.remove(id);
            inner.failing.remove(id);
        }
        drop(guard);
        let mut events = vec![TaskEvent::Removed(removed.clone())];
        events.extend(entries.into_iter().map(TaskEvent::Activity));
        self.emit_all(events);
        // 正在启动、还没写入录制状态的录制不会被上面的检查拦住，一并停止。
        if let Some(hook) = self.scheduler() {
            for id in &removed {
                hook.stop_recording(*id, StopOrigin::Deleted);
            }
            hook.wake();
        }
        Ok(removed)
    }

    /// 批量启用或暂停监控。重新启用时一并清除录制失败（例如磁盘不足）和手动停止的
    /// 保持状态，给用户一个「处理完了，重新开始」的入口。暂停时与 StreamCap 一样
    /// 停止正在进行的录制。返回这些任务的快照（含未变化的任务）。
    pub fn set_monitoring(
        &self,
        ids: &[TaskId],
        enabled: bool,
    ) -> CommandResult<Vec<TaskSnapshot>> {
        let monitoring = if enabled {
            Monitoring::Enabled
        } else {
            Monitoring::Paused
        };
        let mut guard = self.lock();
        let inner = &mut *guard;
        let changed: Vec<Task> = ids
            .iter()
            .filter_map(|id| inner.tasks.get(id))
            .filter(|task| task.monitoring != monitoring)
            .cloned()
            .collect();
        let clears_failure =
            |task: &Task| enabled && matches!(task.record, RecordState::Failed { .. });
        let entries = inner.store.transaction(|db| {
            let mut entries = Vec::new();
            for task in &changed {
                db.set_monitoring(task.id, monitoring)?;
                if clears_failure(task) {
                    db.set_record_state(task.id, &RecordState::Idle)?;
                }
                let message = if enabled {
                    "开始监控"
                } else {
                    "暂停监控"
                };
                entries.push(db.insert_activity(&user_action(task, message, None))?);
            }
            Ok(entries)
        })?;
        let mut events = Vec::new();
        for (task, entry) in changed.iter().zip(entries) {
            let updated = inner.tasks.get_mut(&task.id).expect("task exists");
            updated.monitoring = monitoring;
            if clears_failure(task) {
                updated.record = RecordState::Idle;
            }
            if enabled {
                updated.held = false;
            }
            // 暂停后旧的检测结果不再有效，从未知开始。重新启用时一律重置（包括暂停时停下、
            // 还在收尾的录制），调度据此立即检测；暂停时正在写文件的保留直播状态。
            if enabled || !updated.record.is_active() {
                updated.live = LiveState::Unknown;
            }
            events.push(TaskEvent::Updated(Box::new(updated.snapshot())));
            events.push(TaskEvent::Activity(entry));
            inner.last_known_live.remove(&task.id);
            inner.failing.remove(&task.id);
        }
        let snapshots = ids
            .iter()
            .filter_map(|id| inner.tasks.get(id).map(Task::snapshot))
            .collect();
        drop(guard);
        self.emit_all(events);
        if let Some(hook) = self.scheduler() {
            if !enabled {
                for task in &changed {
                    hook.stop_recording(task.id, StopOrigin::Paused);
                }
            }
            hook.wake();
        }
        Ok(snapshots)
    }

    // ---- 调度与录制引擎回写 ----

    /// 更新直播状态（只在内存中）。开播、下播时写动态；检测失败只在连续失败的第一次
    /// 写动态，中间穿插的「检测中」不打断连续，直到一次检测成功。
    pub fn set_live_state(&self, id: TaskId, live: LiveState) -> Option<TaskSnapshot> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let task = inner.tasks.get(&id)?.clone();
        let previous_live = inner.last_known_live.get(&id).copied();
        let activity = match &live {
            LiveState::Live { title, .. } => {
                inner.failing.remove(&id);
                inner.last_known_live.insert(id, true);
                (previous_live != Some(true)).then(|| {
                    (
                        ActivityKind::LiveStart,
                        "检测到开播".to_string(),
                        title.as_ref().map(|title| json!({ "title": title })),
                    )
                })
            }
            LiveState::Offline { .. } => {
                inner.failing.remove(&id);
                inner.last_known_live.insert(id, false);
                (previous_live == Some(true))
                    .then(|| (ActivityKind::LiveEnd, "已下播".to_string(), None))
            }
            LiveState::CheckFailed { kind, message, .. } => inner.failing.insert(id).then(|| {
                (
                    ActivityKind::Failure,
                    format!("检测失败：{message}"),
                    Some(json!({ "kind": kind })),
                )
            }),
            LiveState::Checking { .. } | LiveState::Unknown => None,
        };
        let entry = activity.and_then(|(kind, message, detail)| {
            inner
                .store
                .db()
                .insert_activity(&NewActivity {
                    task_id: Some(id),
                    anchor_name: task.config.anchor_name.clone(),
                    kind,
                    message,
                    detail,
                })
                .inspect_err(|error| eprintln!("写入动态失败：{error}"))
                .ok()
        });
        let updated = inner.tasks.get_mut(&id).expect("task exists");
        // 下播后手动停止的保持状态结束，下一次开播照常录制。
        if matches!(live, LiveState::Offline { .. }) {
            updated.held = false;
        }
        updated.live = live;
        let snapshot = updated.snapshot();
        drop(guard);
        let mut events = vec![TaskEvent::Updated(Box::new(snapshot.clone()))];
        events.extend(entry.map(TaskEvent::Activity));
        self.emit_all(events);
        Some(snapshot)
    }

    /// 更新录制状态并持久化。在开始录制、录制结束和录制失败时写动态，与状态在同一事务。
    pub fn set_record_state(&self, id: TaskId, record: RecordState) -> CommandResult<TaskSnapshot> {
        self.write_record_state(id, record, None)
    }

    /// 录制结束时回写状态。与 [`TaskManager::set_record_state`] 相同，「录制结束」动态
    /// 写明结束原因和实际生成的文件（多个分段时显示第一个并注明段数）。
    pub fn finish_recording(
        &self,
        id: TaskId,
        record: RecordState,
        end: &RecordingEnd,
    ) -> CommandResult<TaskSnapshot> {
        self.write_record_state(id, record, Some(end))
    }

    fn write_record_state(
        &self,
        id: TaskId,
        record: RecordState,
        end: Option<&RecordingEnd>,
    ) -> CommandResult<TaskSnapshot> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let task = inner.tasks.get(&id).cloned().ok_or_else(|| not_found(id))?;
        let activity = match (&task.record, &record) {
            (previous, RecordState::Recording { file_path, .. })
                if !matches!(previous, RecordState::Recording { .. }) =>
            {
                Some((
                    ActivityKind::RecordStart,
                    "开始录制".to_string(),
                    Some(json!({ "filePath": file_path, "quality": task.config.quality })),
                ))
            }
            (
                RecordState::Recording {
                    file_path,
                    started_at,
                    ..
                }
                | RecordState::Finishing {
                    file_path,
                    started_at,
                },
                RecordState::Idle,
            ) => {
                let files = match end {
                    Some(end) if !end.files.is_empty() => end.files.clone(),
                    _ => vec![file_path.clone()],
                };
                let cause = end.map_or(EndCause::Stopped, |end| end.cause);
                Some((
                    ActivityKind::RecordEnd,
                    record_end_message(cause, files.len()),
                    Some(json!({
                        "filePath": files[0],
                        "files": files,
                        "segments": files.len(),
                        "endCause": cause,
                        "durationMs": now_ms() - started_at,
                    })),
                ))
            }
            (
                _,
                RecordState::Failed {
                    reason, file_path, ..
                },
            ) => Some((
                ActivityKind::Failure,
                format!("录制失败：{reason}"),
                Some(json!({ "filePath": file_path })),
            )),
            _ => None,
        };
        let entry = inner.store.transaction(|db| {
            db.set_record_state(id, &record)?;
            activity
                .map(|(kind, message, detail)| {
                    db.insert_activity(&NewActivity {
                        task_id: Some(id),
                        anchor_name: task.config.anchor_name.clone(),
                        kind,
                        message,
                        detail,
                    })
                })
                .transpose()
        })?;
        let updated = inner.tasks.get_mut(&id).expect("task exists");
        updated.record = record;
        let snapshot = updated.snapshot();
        drop(guard);
        let mut events = vec![TaskEvent::Updated(Box::new(snapshot.clone()))];
        events.extend(entry.map(TaskEvent::Activity));
        self.emit_all(events);
        Ok(snapshot)
    }

    /// 更新录制进度（只在内存中，不写数据库），仅在录制中时生效。
    pub fn update_record_progress(
        &self,
        id: TaskId,
        bytes: u64,
        bitrate: f64,
    ) -> Option<TaskSnapshot> {
        let snapshot = {
            let mut inner = self.lock();
            let task = inner.tasks.get_mut(&id)?;
            let RecordState::Recording {
                bytes_written,
                bitrate_kbps,
                ..
            } = &mut task.record
            else {
                return None;
            };
            *bytes_written = bytes;
            *bitrate_kbps = bitrate;
            task.snapshot()
        };
        (self.listener)(TaskEvent::Updated(Box::new(snapshot.clone())));
        Some(snapshot)
    }

    /// 写一条动态并推送。
    pub fn record_activity(&self, entry: NewActivity) -> CommandResult<ActivityEntry> {
        let entry = self.lock().store.db().insert_activity(&entry)?;
        (self.listener)(TaskEvent::Activity(entry.clone()));
        Ok(entry)
    }

    pub fn list_activity(&self, query: &ActivityQuery) -> CommandResult<ActivityPage> {
        Ok(self.lock().store.db().list_activity(query)?)
    }

    // ---- 设置 ----

    /// 默认值与用户设置合并后的完整设置。
    pub fn settings(&self) -> Map<String, Value> {
        let stored = self.lock().store.db().load_settings().unwrap_or_default();
        let mut merged = self.defaults.clone();
        for (key, value) in stored {
            if merged.contains_key(&key) {
                merged.insert(key, value);
            }
        }
        merged
    }

    pub fn defaults(&self) -> &Map<String, Value> {
        &self.defaults
    }

    /// 校验并保存部分设置，返回完整设置。任一项无效时整批不保存。
    pub fn update_settings(&self, values: Map<String, Value>) -> CommandResult<Map<String, Value>> {
        for (key, value) in &values {
            settings::validate(&self.defaults, key, value)?;
        }
        self.lock()
            .store
            .transaction(|db| db.save_settings(&values))?;
        if let Some(hook) = self.scheduler() {
            hook.settings_changed();
        }
        Ok(self.settings())
    }

    /// 手动停止录制（含正在启动的录制）。停止请求立即返回；录制状态随后经由收尾中
    /// 回到空闲，通过 `task-updated` 推送。被停止的任务在本次直播中不再自动录制，
    /// 直到检测到下播或用户重新启用监控。没有录制的任务原样返回。
    pub fn stop_recording(&self, ids: &[TaskId]) -> CommandResult<Vec<TaskSnapshot>> {
        let hook = self
            .scheduler()
            .ok_or_else(|| CommandError::new(ErrorKind::NotImplemented, "录制调度尚未启动"))?;
        let mut events = Vec::new();
        for id in ids {
            if !hook.stop_recording(*id, StopOrigin::Manual) {
                continue;
            }
            let mut inner = self.lock();
            if let Some(task) = inner.tasks.get_mut(id)
                && !task.held
            {
                task.held = true;
                events.push(TaskEvent::Updated(Box::new(task.snapshot())));
            }
        }
        let snapshots = {
            let inner = self.lock();
            ids.iter()
                .filter_map(|id| inner.tasks.get(id).map(Task::snapshot))
                .collect()
        };
        self.emit_all(events);
        Ok(snapshots)
    }
}

impl TaskManager {
    /// 重新开始录制，参照 StreamCap 单个任务的「开始录制」：清除手动停止后的保持状态和
    /// 录制失败，立即检测一次，仍在播就开始录制。只对监控中、会录制、当前没有录制的
    /// 任务生效，其余跳过（已暂停监控的应先恢复监控）。返回重新开始的任务快照。
    pub fn start_recording(&self, ids: &[TaskId]) -> CommandResult<Vec<TaskSnapshot>> {
        let hook = self
            .scheduler()
            .ok_or_else(|| CommandError::new(ErrorKind::NotImplemented, "录制调度尚未启动"))?;
        let mut guard = self.lock();
        let inner = &mut *guard;
        let mut seen = HashSet::new();
        let targets: Vec<Task> = ids
            .iter()
            .filter(|id| seen.insert(**id))
            .filter_map(|id| inner.tasks.get(id))
            .filter(|task| {
                task.monitoring == Monitoring::Enabled
                    && !task.config.notify_only
                    && !task.record.is_active()
            })
            .cloned()
            .collect();
        let failed = |task: &Task| matches!(task.record, RecordState::Failed { .. });
        let entries = inner.store.transaction(|db| {
            let mut entries = Vec::new();
            for task in &targets {
                if failed(task) {
                    db.set_record_state(task.id, &RecordState::Idle)?;
                }
                entries.push(db.insert_activity(&user_action(task, "重新开始录制", None))?);
            }
            Ok(entries)
        })?;
        let mut events = Vec::new();
        let mut snapshots = Vec::new();
        for (task, entry) in targets.iter().zip(entries) {
            let updated = inner.tasks.get_mut(&task.id).expect("task exists");
            if failed(task) {
                updated.record = RecordState::Idle;
            }
            updated.held = false;
            // 直播状态为未知时调度立即检测。保留上次的开播判断，不重复写「检测到开播」。
            updated.live = LiveState::Unknown;
            let snapshot = updated.snapshot();
            events.push(TaskEvent::Updated(Box::new(snapshot.clone())));
            events.push(TaskEvent::Activity(entry));
            snapshots.push(snapshot);
        }
        drop(guard);
        self.emit_all(events);
        hook.wake();
        Ok(snapshots)
    }
}

fn not_found(id: TaskId) -> CommandError {
    CommandError::new(ErrorKind::NotFound, format!("任务 {id} 不存在"))
}

/// 只读打开 `data_dir/lubobao.db` 并读出任务：不创建文件、不迁移、不处理异常中断的录制。
/// 数据库不存在时视为没有任务、全部为默认设置。
pub fn open_read_only_in(data_dir: &Path, save_dir: &Path) -> Result<TaskManager, String> {
    let db_path = data_dir.join(DB_FILE);
    let store = Store::open_read_only(&db_path).map_err(|error| {
        format!(
            "{error}
数据库：{}",
            db_path.display()
        )
    })?;
    TaskManager::open_read_only(store, save_dir, Box::new(|_| {})).map_err(|error| {
        format!(
            "{}
数据库：{}",
            error.message,
            db_path.display()
        )
    })
}

/// 数据库文件名，位于数据目录下。
pub const DB_FILE: &str = "lubobao.db";

/// 打开 `data_dir/lubobao.db`（不存在时创建）并恢复任务。`save_dir` 是默认录像保存目录
/// （设置 `live_save_path` 的缺省值）。不依赖 Tauri，命令行程序也用它。失败时返回给用户
/// 看的说明，含数据库路径。
pub fn open_in(
    data_dir: &Path,
    save_dir: &Path,
    listener: Listener,
) -> Result<TaskManager, String> {
    let db_path = data_dir.join(DB_FILE);
    let store =
        Store::open(&db_path).map_err(|error| format!("{error}\n数据库：{}", db_path.display()))?;
    TaskManager::open(store, save_dir, listener)
        .map_err(|error| format!("{}\n数据库：{}", error.message, db_path.display()))
}
