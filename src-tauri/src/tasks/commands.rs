//! 任务相关的 Tauri 命令。参数名在前端为 camelCase，例如 `{ input }`、`{ ids, enabled }`。
//!
//! 不带 `async` 的命令在主线程上执行；任务命令要拿任务锁、读写 SQLite，调度线程也在频繁使用
//! 这把锁，所以一律放到阻塞线程里执行，主线程（窗口重绘、托盘菜单）不跟着等。

use std::sync::Arc;

use serde_json::{Map, Value};
use tauri::State;

use super::{
    ActivityPage, ActivityQuery, AddTaskInput, CommandError, CommandResult, ErrorKind, TaskId,
    TaskManager, TaskMutation, TaskSnapshot, UpdateTaskInput,
};

type Manager<'a> = State<'a, Arc<TaskManager>>;

/// 在阻塞线程里执行 `job`。
async fn blocking<T, F>(manager: &Arc<TaskManager>, job: F) -> CommandResult<T>
where
    T: Send + 'static,
    F: FnOnce(&TaskManager) -> CommandResult<T> + Send + 'static,
{
    let manager = Arc::clone(manager);
    tauri::async_runtime::spawn_blocking(move || job(&manager))
        .await
        .unwrap_or_else(|error| {
            Err(CommandError::new(
                ErrorKind::Storage,
                format!("任务操作中断：{error}"),
            ))
        })
}

#[tauri::command]
pub async fn list_tasks(manager: Manager<'_>) -> CommandResult<Vec<TaskSnapshot>> {
    blocking(&manager, |tasks| Ok(tasks.list())).await
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
pub async fn delete_tasks(manager: Manager<'_>, ids: Vec<TaskId>) -> CommandResult<Vec<TaskId>> {
    blocking(&manager, move |tasks| tasks.delete_tasks(&ids)).await
}

#[tauri::command]
pub async fn set_monitoring(
    manager: Manager<'_>,
    ids: Vec<TaskId>,
    enabled: bool,
) -> CommandResult<Vec<TaskSnapshot>> {
    blocking(&manager, move |tasks| tasks.set_monitoring(&ids, enabled)).await
}

#[tauri::command]
pub async fn stop_recording(
    manager: Manager<'_>,
    ids: Vec<TaskId>,
) -> CommandResult<Vec<TaskSnapshot>> {
    blocking(&manager, move |tasks| tasks.stop_recording(&ids)).await
}

#[tauri::command]
pub async fn start_recording(
    manager: Manager<'_>,
    ids: Vec<TaskId>,
) -> CommandResult<Vec<TaskSnapshot>> {
    blocking(&manager, move |tasks| tasks.start_recording(&ids)).await
}

#[tauri::command]
pub async fn get_settings(manager: Manager<'_>) -> CommandResult<Map<String, Value>> {
    blocking(&manager, |tasks| Ok(tasks.settings())).await
}

/// 设置的默认值，与 `settings::defaults` 一致（保存目录是本机的默认目录）。
/// 前端“恢复默认值”用它，不在前端另写一份。只读内存里的默认值，不拿任务锁。
#[tauri::command]
pub fn get_setting_defaults(manager: Manager<'_>) -> Map<String, Value> {
    manager.defaults().clone()
}

#[tauri::command]
pub async fn update_settings(
    manager: Manager<'_>,
    values: Map<String, Value>,
) -> CommandResult<Map<String, Value>> {
    blocking(&manager, move |tasks| tasks.update_settings(values)).await
}

#[tauri::command]
pub async fn list_activity(
    manager: Manager<'_>,
    query: Option<ActivityQuery>,
) -> CommandResult<ActivityPage> {
    let query = query.unwrap_or_default();
    blocking(&manager, move |tasks| tasks.list_activity(&query)).await
}
