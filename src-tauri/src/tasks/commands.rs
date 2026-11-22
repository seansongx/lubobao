//! 任务相关的 Tauri 命令。参数名在前端为 camelCase，例如 `{ input }`、`{ ids, enabled }`。

use std::sync::Arc;

use serde_json::{Map, Value};
use tauri::State;

use super::{
    ActivityPage, ActivityQuery, AddTaskInput, CommandResult, TaskId, TaskManager, TaskMutation,
    TaskSnapshot, UpdateTaskInput,
};

type Manager<'a> = State<'a, Arc<TaskManager>>;

#[tauri::command]
pub fn list_tasks(manager: Manager<'_>) -> Vec<TaskSnapshot> {
    manager.list()
}

#[tauri::command]
pub async fn add_task(manager: Manager<'_>, input: AddTaskInput) -> CommandResult<TaskMutation> {
    manager.add_task(input).await
}

#[tauri::command]
pub async fn update_task(
    manager: Manager<'_>,
    input: UpdateTaskInput,
) -> CommandResult<TaskMutation> {
    manager.update_task(input).await
}

#[tauri::command]
pub fn delete_tasks(manager: Manager<'_>, ids: Vec<TaskId>) -> CommandResult<Vec<TaskId>> {
    manager.delete_tasks(&ids)
}

#[tauri::command]
pub fn set_monitoring(
    manager: Manager<'_>,
    ids: Vec<TaskId>,
    enabled: bool,
) -> CommandResult<Vec<TaskSnapshot>> {
    manager.set_monitoring(&ids, enabled)
}

#[tauri::command]
pub fn stop_recording(manager: Manager<'_>, ids: Vec<TaskId>) -> CommandResult<Vec<TaskSnapshot>> {
    manager.stop_recording(&ids)
}

#[tauri::command]
pub fn get_settings(manager: Manager<'_>) -> Map<String, Value> {
    manager.settings()
}

/// 设置的默认值，与 `settings::defaults` 一致（保存目录是本机的默认目录）。
/// 前端“恢复默认值”用它，不在前端另写一份。
#[tauri::command]
pub fn get_setting_defaults(manager: Manager<'_>) -> Map<String, Value> {
    manager.defaults.clone()
}

#[tauri::command]
pub fn update_settings(
    manager: Manager<'_>,
    values: Map<String, Value>,
) -> CommandResult<Map<String, Value>> {
    manager.update_settings(values)
}

#[tauri::command]
pub fn list_activity(
    manager: Manager<'_>,
    query: Option<ActivityQuery>,
) -> CommandResult<ActivityPage> {
    manager.list_activity(&query.unwrap_or_default())
}
