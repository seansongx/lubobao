//! 桌面外壳：托盘、关窗隐藏、退出确认和系统通知。只在 Tauri 桌面端使用，核心模块不依赖它。
//!
//! 数据来源都是已有的接口：录制数来自 `scheduler-status` 事件，通知来自 `activity` 事件，
//! 暂停和恢复调用 [`TaskManager::set_monitoring`]。退出一律经过
//! [`crate::scheduler::tauri_glue::on_exit_requested`]，由它停止录制、等文件收尾。

mod hint;
pub mod notify;
pub mod quit;
mod tray;

use std::sync::Arc;

use serde::Deserialize;
use tauri::{Listener, Manager};

use crate::scheduler::{STATUS_EVENT, Scheduler};
use crate::tasks::{ACTIVITY_EVENT, TaskManager};

pub const MAIN_WINDOW: &str = "main";

/// `scheduler-status` 载荷中用到的字段（见 `scheduler::SchedulerStatus`）。
#[derive(Deserialize)]
struct StatusPayload {
    recording: usize,
}

/// 在 `setup` 中、任务和调度初始化之后调用。
pub fn init(app: &tauri::AppHandle) -> tauri::Result<()> {
    app.manage(quit::QuitState::default());
    tray::create(app)?;
    let recording = app
        .try_state::<Arc<Scheduler>>()
        .map(|scheduler| scheduler.status().recording)
        .unwrap_or(0);
    quit::set_recording(app, recording);

    let handle = app.clone();
    app.listen_any(STATUS_EVENT, move |event| {
        if let Ok(status) = serde_json::from_str::<StatusPayload>(event.payload()) {
            quit::set_recording(&handle, status.recording);
        }
    });

    let handle = app.clone();
    app.listen_any(ACTIVITY_EVENT, move |event| {
        let Ok(activity) = serde_json::from_str::<notify::ActivityNotice>(event.payload()) else {
            return;
        };
        let Some(tasks) = handle.try_state::<Arc<TaskManager>>() else {
            return;
        };
        let task = activity
            .task_id
            .and_then(|id| tasks.get(id))
            .map(|task| notify::TaskNotice {
                platform_name: task.platform_name.to_string(),
                display: task.display,
                display_reason: task.display_reason,
            });
        let settings = notify::NotifySettings::from_settings(&tasks.settings());
        if let Some(notice) = notify::decide(&activity, task.as_ref(), &settings) {
            notify::show(&handle, &notice);
        }
    });
    Ok(())
}

/// 显示主窗口并置于前台：托盘、单实例、macOS 点 Dock 图标时调用。
pub fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// 窗口事件：关闭主窗口只隐藏到托盘，录制照常进行。第一次隐藏时提示一次。
pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != MAIN_WINDOW {
        return;
    }
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        let _ = window.hide();
        hint::show_once(window.app_handle());
    }
}

/// 应用事件：退出请求先经过确认，再交给调度收尾；macOS 点 Dock 图标时重新显示窗口。
pub fn on_run_event(app: &tauri::AppHandle, event: &tauri::RunEvent) {
    match event {
        tauri::RunEvent::ExitRequested { code, api, .. } => {
            quit::on_exit_requested(app, *code, api);
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => show_main_window(app),
        _ => {}
    }
}

/// 只在调试构建可用：不经过系统托盘和鼠标，直接触发托盘操作或关窗，返回当前托盘提示。
/// `action`：`show`、`click`、`pause-all`、`resume-all`、`quit`、`close-window`，或 `tooltip`
/// （只读取提示）。发布构建一律拒绝。
#[tauri::command]
pub fn debug_desktop_action(app: tauri::AppHandle, action: String) -> Result<String, String> {
    if !cfg!(debug_assertions) {
        return Err("仅调试构建可用".into());
    }
    match action.as_str() {
        "tooltip" => {}
        "close-window" => {
            let window = app
                .get_webview_window(MAIN_WINDOW)
                .ok_or_else(|| "没有主窗口".to_string())?;
            window.close().map_err(|error| error.to_string())?;
        }
        name => {
            let action =
                tray::TrayAction::parse(name).ok_or_else(|| format!("未知操作：{name}"))?;
            tray::handle(&app, action);
        }
    }
    Ok(tray::current_tooltip())
}
