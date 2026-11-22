//! 任务核心与 Tauri 之间的胶水：取应用目录、把变化推送给前端、打不开数据库时提示。
//! 任务核心见上级模块（不依赖 Tauri）；Tauri 命令见 [`super::commands`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use super::{
    ACTIVITY_EVENT, Listener, TASK_REMOVED_EVENT, TASK_UPDATED_EVENT, TaskEvent, TaskManager,
    open_in,
};

/// 默认保存目录：系统「视频」目录下的 `录播宝`，取不到时用应用数据目录下的 `recordings`。
fn default_save_dir(app: &tauri::AppHandle, data_dir: &Path) -> PathBuf {
    app.path()
        .video_dir()
        .map(|dir| dir.join("录播宝"))
        .unwrap_or_else(|_| data_dir.join("recordings"))
}

/// 把任务变化转成 `task-updated`、`task-removed`、`activity` 事件。
fn event_listener(app: &tauri::AppHandle) -> Listener {
    let handle = app.clone();
    Box::new(move |event| {
        let _ = match event {
            TaskEvent::Updated(snapshot) => handle.emit(TASK_UPDATED_EVENT, *snapshot),
            TaskEvent::Removed(ids) => handle.emit(TASK_REMOVED_EVENT, json!({ "ids": ids })),
            TaskEvent::Activity(entry) => handle.emit(ACTIVITY_EVENT, entry),
        };
    })
}

fn open_manager(app: &tauri::AppHandle) -> Result<TaskManager, String> {
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|error| format!("找不到应用数据目录：{error}"))?;
    open_in(
        &data_dir,
        &default_save_dir(app, &data_dir),
        event_listener(app),
    )
}

/// 在 Tauri `setup` 中调用：打开数据库、恢复任务并登记 `Arc<TaskManager>`。
///
/// 打不开数据库（例如被更新版本的录播宝升级过）时不 panic：记日志，弹出错误对话框，
/// 用户确认后退出。
pub fn init(app: &tauri::AppHandle) -> tauri::Result<()> {
    match open_manager(app) {
        Ok(manager) => {
            app.manage(Arc::new(manager));
        }
        Err(message) => {
            eprintln!("录播宝无法打开任务数据：{message}");
            let handle = app.clone();
            app.dialog()
                .message(format!("无法打开任务数据，录播宝将退出。\n\n{message}"))
                .title("录播宝启动失败")
                .kind(MessageDialogKind::Error)
                .buttons(MessageDialogButtons::Ok)
                .show(move |_| handle.exit(1));
        }
    }
    Ok(())
}
