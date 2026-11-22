//! 任务核心：任务模型、SQLite 持久化、动态记录、设置，以及给前端的命令和事件。
//!
//! [`TaskManager`] 持有全部任务状态。前端通过 [`commands`] 中的 Tauri 命令读取快照和
//! 提交操作，通过 `task-updated`、`task-removed`、`activity` 事件接收变化。后续的轮询
//! 调度和录制引擎通过 `set_live_state`、`set_record_state`、`update_record_progress`
//! 回写状态，由这里负责持久化、写动态和推送事件。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tauri::{Emitter, Manager};

use crate::platforms::{FetchOptions, LiveInfo, Platform, PlatformError, Quality, adapter_for};

pub mod activity;
pub mod commands;
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

/// 启动时把未正常结束的录制标记为失败所用的原因。
pub const INTERRUPTED_REASON: &str = "录制异常中断：上次退出时录制未正常结束";

/// 推送给前端的变化。
#[derive(Debug, Clone, PartialEq)]
pub enum TaskEvent {
    Updated(Box<TaskSnapshot>),
    Removed(Vec<TaskId>),
    Activity(ActivityEntry),
}

pub type Listener = Box<dyn Fn(TaskEvent) + Send + Sync>;

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
}

pub struct TaskManager {
    inner: Mutex<Inner>,
    defaults: Map<String, Value>,
    listener: Listener,
}

impl TaskManager {
    /// 读出任务，把上次未正常结束的录制标记为异常中断并各写一条动态。
    pub fn open(store: Store, save_dir: &Path, listener: Listener) -> CommandResult<TaskManager> {
        let tasks = store.load_tasks()?;
        let manager = TaskManager {
            inner: Mutex::new(Inner {
                store,
                tasks: tasks.into_iter().map(|task| (task.id, task)).collect(),
                last_known_live: HashMap::new(),
            }),
            defaults: settings::defaults(save_dir),
            listener,
        };
        manager.recover_interrupted()?;
        Ok(manager)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit_all(&self, events: Vec<TaskEvent>) {
        for event in events {
            (self.listener)(event);
        }
    }

    fn recover_interrupted(&self) -> CommandResult<()> {
        let mut events = Vec::new();
        {
            let mut inner = self.lock();
            let interrupted: Vec<TaskId> = inner
                .tasks
                .values()
                .filter(|task| task.record.is_active())
                .map(|task| task.id)
                .collect();
            for id in interrupted {
                let task = inner.tasks.get(&id).cloned().expect("task exists");
                let file_path = match &task.record {
                    RecordState::Recording { file_path, .. }
                    | RecordState::Finishing { file_path, .. } => Some(file_path.clone()),
                    _ => None,
                };
                let record = RecordState::Failed {
                    reason: INTERRUPTED_REASON.into(),
                    file_path: file_path.clone(),
                    failed_at: now_ms(),
                };
                inner.store.set_record_state(id, &record)?;
                let entry = inner.store.insert_activity(&NewActivity {
                    task_id: Some(id),
                    anchor_name: task.config.anchor_name.clone(),
                    kind: ActivityKind::Failure,
                    message: INTERRUPTED_REASON.into(),
                    detail: Some(json!({ "filePath": file_path, "previous": task.record })),
                })?;
                if let Some(task) = inner.tasks.get_mut(&id) {
                    task.record = record;
                }
                events.push(TaskEvent::Activity(entry));
            }
        }
        self.emit_all(events);
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

    /// 识别平台并实际请求一次直播间。
    pub async fn resolve(url: &str, quality: Quality) -> CommandResult<LiveInfo> {
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
        let options = FetchOptions {
            quality,
            ..FetchOptions::default()
        };
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
        let info = Self::resolve(&input.url, quality).await?;
        self.add_resolved(input, &info)
    }

    /// 用已解析的直播间信息添加任务。去重检查与写入在同一把锁内完成。
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
            quality: input.quality.unwrap_or_else(|| self.default_quality()),
            notify_only: input.notify_only.unwrap_or_else(|| {
                settings
                    .get("only_notify_no_record")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            }),
            note: input.note.unwrap_or_default(),
            created_at: now_ms(),
        };
        let mut events = Vec::new();
        let result = {
            let mut inner = self.lock();
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
            let id = inner.store.insert_task(&config, Monitoring::Enabled)?;
            let task = Task {
                id,
                config,
                monitoring: Monitoring::Enabled,
                live: LiveState::Unknown,
                record: RecordState::Idle,
            };
            let snapshot = task.snapshot();
            let detail = json!({ "url": task.config.url, "forced": !duplicates.is_empty() });
            let anchor_name = task.config.anchor_name.clone();
            inner.tasks.insert(id, task);
            events.push(TaskEvent::Updated(Box::new(snapshot.clone())));
            events.push(TaskEvent::Activity(inner.store.insert_activity(
                &NewActivity {
                    task_id: Some(id),
                    anchor_name,
                    kind: ActivityKind::UserAction,
                    message: "添加任务".into(),
                    detail: Some(detail),
                },
            )?));
            TaskMutation::Added { task: snapshot }
        };
        self.emit_all(events);
        Ok(result)
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
                Some(Self::resolve(url, quality).await?)
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
        let mut events = Vec::new();
        let result = {
            let mut inner = self.lock();
            let task = inner
                .tasks
                .get(&input.id)
                .cloned()
                .ok_or_else(|| not_found(input.id))?;
            let mut config = task.config.clone();
            if let (Some(info), Some(url)) = (info, input.url.as_deref()) {
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
            }
            if let Some(quality) = input.quality {
                config.quality = quality;
            }
            if let Some(notify_only) = input.notify_only {
                config.notify_only = notify_only;
            }
            if let Some(note) = input.note {
                config.note = note;
            }
            if config != task.config {
                inner.store.update_task_config(task.id, &config)?;
                let url_changed = config.url != task.config.url;
                let updated = inner.tasks.get_mut(&task.id).expect("task exists");
                updated.config = config;
                if url_changed {
                    updated.live = LiveState::Unknown;
                    inner.last_known_live.remove(&task.id);
                }
                let anchor_name = inner.tasks[&task.id].config.anchor_name.clone();
                events.push(TaskEvent::Activity(inner.store.insert_activity(
                    &NewActivity {
                        task_id: Some(task.id),
                        anchor_name,
                        kind: ActivityKind::UserAction,
                        message: "修改任务".into(),
                        detail: None,
                    },
                )?));
            }
            let snapshot = inner.tasks[&task.id].snapshot();
            events.insert(0, TaskEvent::Updated(Box::new(snapshot.clone())));
            TaskMutation::Updated { task: snapshot }
        };
        self.emit_all(events);
        Ok(result)
    }

    // ---- 批量操作 ----

    /// 删除任务。任一任务正在录制时整批不删除。返回实际删除的 id。
    pub fn delete_tasks(&self, ids: &[TaskId]) -> CommandResult<Vec<TaskId>> {
        let mut events = Vec::new();
        let removed = {
            let mut inner = self.lock();
            if let Some(task) = ids
                .iter()
                .filter_map(|id| inner.tasks.get(id))
                .find(|task| task.record.is_active())
            {
                return Err(CommandError::new(
                    ErrorKind::RecordingActive,
                    format!("{} 正在录制，停止录制后才能删除", task.config.anchor_name),
                ));
            }
            let mut removed = Vec::new();
            for id in ids {
                let Some(task) = inner.tasks.get(id).cloned() else {
                    continue;
                };
                inner.store.delete_task(*id)?;
                inner.tasks.remove(id);
                inner.last_known_live.remove(id);
                removed.push(*id);
                events.push(TaskEvent::Activity(inner.store.insert_activity(
                    &NewActivity {
                        task_id: Some(*id),
                        anchor_name: task.config.anchor_name.clone(),
                        kind: ActivityKind::UserAction,
                        message: "删除任务".into(),
                        detail: Some(json!({ "url": task.config.url })),
                    },
                )?));
            }
            if !removed.is_empty() {
                events.insert(0, TaskEvent::Removed(removed.clone()));
            }
            removed
        };
        self.emit_all(events);
        Ok(removed)
    }

    /// 批量启用或暂停监控，只改监控意图。返回变化后的快照（含未变化的任务）。
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
        let mut events = Vec::new();
        let snapshots = {
            let mut inner = self.lock();
            let mut snapshots = Vec::new();
            for id in ids {
                let Some(task) = inner.tasks.get(id).cloned() else {
                    continue;
                };
                if task.monitoring != monitoring {
                    inner.store.set_monitoring(*id, monitoring)?;
                    let updated = inner.tasks.get_mut(id).expect("task exists");
                    updated.monitoring = monitoring;
                    // 暂停后旧的检测结果不再有效，重新启用时从未知开始。
                    if !updated.record.is_active() {
                        updated.live = LiveState::Unknown;
                    }
                    inner.last_known_live.remove(id);
                    let snapshot = inner.tasks[id].snapshot();
                    events.push(TaskEvent::Updated(Box::new(snapshot)));
                    events.push(TaskEvent::Activity(
                        inner.store.insert_activity(&NewActivity {
                            task_id: Some(*id),
                            anchor_name: task.config.anchor_name.clone(),
                            kind: ActivityKind::UserAction,
                            message: if enabled {
                                "开始监控"
                            } else {
                                "暂停监控"
                            }
                            .into(),
                            detail: None,
                        })?,
                    ));
                }
                snapshots.push(inner.tasks[id].snapshot());
            }
            snapshots
        };
        self.emit_all(events);
        Ok(snapshots)
    }

    // ---- 调度与录制引擎回写 ----

    /// 更新直播状态（只在内存中）。在开播、下播和首次检测失败时写动态。
    pub fn set_live_state(&self, id: TaskId, live: LiveState) -> Option<TaskSnapshot> {
        let mut events = Vec::new();
        let snapshot = {
            let mut inner = self.lock();
            let task = inner.tasks.get(&id)?.clone();
            let previous_live = inner.last_known_live.get(&id).copied();
            let activity = match &live {
                LiveState::Live { title, .. } if previous_live != Some(true) => {
                    inner.last_known_live.insert(id, true);
                    Some((
                        ActivityKind::LiveStart,
                        "检测到开播".to_string(),
                        title.as_ref().map(|title| json!({ "title": title })),
                    ))
                }
                LiveState::Live { .. } => None,
                LiveState::Offline { .. } => {
                    inner.last_known_live.insert(id, false);
                    (previous_live == Some(true))
                        .then(|| (ActivityKind::LiveEnd, "已下播".to_string(), None))
                }
                LiveState::CheckFailed { kind, message, .. }
                    if !matches!(task.live, LiveState::CheckFailed { .. }) =>
                {
                    Some((
                        ActivityKind::Failure,
                        format!("检测失败：{message}"),
                        Some(json!({ "kind": kind })),
                    ))
                }
                _ => None,
            };
            let updated = inner.tasks.get_mut(&id).expect("task exists");
            updated.live = live;
            let snapshot = updated.snapshot();
            events.push(TaskEvent::Updated(Box::new(snapshot.clone())));
            if let Some((kind, message, detail)) = activity {
                match inner.store.insert_activity(&NewActivity {
                    task_id: Some(id),
                    anchor_name: task.config.anchor_name.clone(),
                    kind,
                    message,
                    detail,
                }) {
                    Ok(entry) => events.push(TaskEvent::Activity(entry)),
                    Err(error) => eprintln!("写入动态失败：{error}"),
                }
            }
            snapshot
        };
        self.emit_all(events);
        Some(snapshot)
    }

    /// 更新录制状态并持久化。在开始录制、录制结束和录制失败时写动态。
    pub fn set_record_state(&self, id: TaskId, record: RecordState) -> CommandResult<TaskSnapshot> {
        let mut events = Vec::new();
        let snapshot = {
            let mut inner = self.lock();
            let task = inner.tasks.get(&id).cloned().ok_or_else(|| not_found(id))?;
            inner.store.set_record_state(id, &record)?;
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
                ) => Some((
                    ActivityKind::RecordEnd,
                    "录制结束".to_string(),
                    Some(json!({
                        "filePath": file_path,
                        "durationMs": now_ms() - started_at,
                    })),
                )),
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
            let updated = inner.tasks.get_mut(&id).expect("task exists");
            updated.record = record;
            let snapshot = updated.snapshot();
            events.push(TaskEvent::Updated(Box::new(snapshot.clone())));
            if let Some((kind, message, detail)) = activity {
                events.push(TaskEvent::Activity(inner.store.insert_activity(
                    &NewActivity {
                        task_id: Some(id),
                        anchor_name: task.config.anchor_name.clone(),
                        kind,
                        message,
                        detail,
                    },
                )?));
            }
            snapshot
        };
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
        let entry = self.lock().store.insert_activity(&entry)?;
        (self.listener)(TaskEvent::Activity(entry.clone()));
        Ok(entry)
    }

    pub fn list_activity(&self, query: &ActivityQuery) -> CommandResult<ActivityPage> {
        Ok(self.lock().store.list_activity(query)?)
    }

    // ---- 设置 ----

    /// 默认值与用户设置合并后的完整设置。
    pub fn settings(&self) -> Map<String, Value> {
        let stored = self.lock().store.load_settings().unwrap_or_default();
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
            settings::validate(&self.defaults, key, value)
                .map_err(|message| CommandError::new(ErrorKind::InvalidInput, message))?;
        }
        self.lock().store.save_settings(&values)?;
        Ok(self.settings())
    }

    pub fn stop_recording(&self, _ids: &[TaskId]) -> CommandResult<Vec<TaskSnapshot>> {
        Err(CommandError::new(
            ErrorKind::NotImplemented,
            "录制功能尚未接入",
        ))
    }
}

fn not_found(id: TaskId) -> CommandError {
    CommandError::new(ErrorKind::NotFound, format!("任务 {id} 不存在"))
}

/// 默认保存目录：系统「视频」目录下的 `录播宝`，取不到时用应用数据目录下的 `recordings`。
fn default_save_dir(app: &tauri::AppHandle, data_dir: &Path) -> PathBuf {
    app.path()
        .video_dir()
        .map(|dir| dir.join("录播宝"))
        .unwrap_or_else(|_| data_dir.join("recordings"))
}

/// 在 Tauri `setup` 中调用：打开数据库、恢复任务并登记 `Arc<TaskManager>`。
pub fn init(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = app.path().app_local_data_dir()?;
    let store = Store::open(&data_dir.join("lubobao.db"))?;
    let handle = app.clone();
    let listener: Listener = Box::new(move |event| {
        let _ = match event {
            TaskEvent::Updated(snapshot) => handle.emit(TASK_UPDATED_EVENT, *snapshot),
            TaskEvent::Removed(ids) => handle.emit(TASK_REMOVED_EVENT, json!({ "ids": ids })),
            TaskEvent::Activity(entry) => handle.emit(ACTIVITY_EVENT, entry),
        };
    });
    let manager = TaskManager::open(store, &default_save_dir(app, &data_dir), listener)?;
    app.manage(Arc::new(manager));
    Ok(())
}
