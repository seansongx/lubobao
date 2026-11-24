//! 托盘图标：显示窗口、全部暂停或恢复监控、退出；提示文字显示录制数。
//!
//! 「全部暂停监控」在有录制时先确认。「全部恢复监控」只恢复本次运行中由「全部暂停」暂停的
//! 任务，用户单独暂停的保持暂停；没有可恢复的任务时这一项不可用。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};

use super::quit::{self, QuitPhase, QuitSnapshot};
use crate::tasks::{Monitoring, TaskId, TaskManager};

const TRAY_ID: &str = "main";
const MENU_SHOW: &str = "show";
const MENU_PAUSE_ALL: &str = "pause-all";
const MENU_RESUME_ALL: &str = "resume-all";
const MENU_QUIT: &str = "quit";

/// 有录制时点「全部暂停监控」，请前端确认。载荷为 [`PausePrompt`]。
pub const PAUSE_ALL_PROMPT_EVENT: &str = "pause-all-prompt";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PausePrompt {
    pub recording: usize,
}

/// 托盘菜单和单击图标对应的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    ShowWindow,
    PauseAll,
    ResumeAll,
    Quit,
}

impl TrayAction {
    fn from_menu_id(id: &str) -> Option<TrayAction> {
        match id {
            MENU_SHOW => Some(TrayAction::ShowWindow),
            MENU_PAUSE_ALL => Some(TrayAction::PauseAll),
            MENU_RESUME_ALL => Some(TrayAction::ResumeAll),
            MENU_QUIT => Some(TrayAction::Quit),
            _ => None,
        }
    }

    pub fn parse(name: &str) -> Option<TrayAction> {
        match name {
            "show" | "click" => Some(TrayAction::ShowWindow),
            "pause-all" => Some(TrayAction::PauseAll),
            "resume-all" => Some(TrayAction::ResumeAll),
            "quit" => Some(TrayAction::Quit),
            _ => None,
        }
    }
}

/// 托盘相关的运行时状态。
#[derive(Default)]
pub struct TrayState {
    /// 由「全部暂停」暂停、等「全部恢复」恢复的任务。
    paused_by_tray: Mutex<Vec<TaskId>>,
    resume_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    /// 最近一次设置的提示文字。托盘对象不提供读取接口，记在这里供调试命令查看。
    tooltip: Mutex<String>,
    /// 托盘创建成功。失败时（例如 Linux 缺少 AppIndicator）关窗直接按退出处理。
    available: AtomicBool,
    /// 「全部暂停」「全部恢复」在阻塞线程里执行，用它保证两者不交错。
    serial: Mutex<()>,
}

/// 托盘是否可用。
pub fn available(app: &tauri::AppHandle) -> bool {
    app.try_state::<TrayState>()
        .is_some_and(|state| state.available.load(Ordering::SeqCst))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 执行托盘操作（主线程）。菜单、单击图标和调试命令都走这里。
pub fn handle(app: &tauri::AppHandle, action: TrayAction) {
    match action {
        TrayAction::ShowWindow => super::show_main_window(app),
        TrayAction::PauseAll => request_pause_all(app),
        TrayAction::ResumeAll => in_background(app, resume_all),
        TrayAction::Quit => quit::request_quit(app),
    }
}

/// 暂停、恢复要拿任务锁并写 SQLite，放到阻塞线程里，不卡住托盘菜单所在的主线程。
fn in_background(app: &tauri::AppHandle, job: fn(&tauri::AppHandle)) {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || job(&handle));
}

/// 托盘提示文字。
pub fn tooltip_text(snapshot: QuitSnapshot) -> String {
    match snapshot.phase {
        QuitPhase::Finishing => "录播宝 · 正在收尾…".into(),
        _ => format!("录播宝 · 录制中 {}", snapshot.recording),
    }
}

/// Linux 托盘标题（libappindicator 不支持提示文字，标题常驻显示在图标旁）：有录制或收尾时才
/// 显示，平时只显示「录播宝」。
pub fn title_text(snapshot: QuitSnapshot) -> String {
    match snapshot.phase {
        QuitPhase::Finishing => tooltip_text(snapshot),
        _ if snapshot.recording > 0 => tooltip_text(snapshot),
        _ => "录播宝".into(),
    }
}

/// Linux：托盘依赖运行时加载的 AppIndicator 库，缺失时 libappindicator-sys 会直接 panic，
/// 所以先自己探测。
#[cfg(target_os = "linux")]
fn appindicator_available() -> bool {
    ["libayatana-appindicator3.so.1", "libappindicator3.so.1"]
        .iter()
        .any(|name| {
            let name = std::ffi::CString::new(*name).expect("library name");
            // SAFETY: 只加载后立即卸载，不调用库中的符号。
            unsafe {
                let handle = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
                if handle.is_null() {
                    false
                } else {
                    libc::dlclose(handle);
                    true
                }
            }
        })
}

/// 任务变为监控中（任意线程）：不再算作「全部暂停」暂停的任务。全部移除后把「全部恢复」
/// 置灰（菜单在主线程上更新）。
pub fn on_task_enabled(app: &tauri::AppHandle, id: TaskId) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    let removed = {
        let mut paused = lock(&state.paused_by_tray);
        let before = paused.len();
        paused.retain(|task| *task != id);
        paused.len() != before
    };
    if removed {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(state) = handle.try_state::<TrayState>() {
                refresh_resume_item(&state);
            }
        });
    }
}

/// 「全部暂停」要暂停的任务：当前在监控的。
pub fn pause_targets(tasks: &[(TaskId, Monitoring)]) -> Vec<TaskId> {
    tasks
        .iter()
        .filter(|(_, monitoring)| *monitoring == Monitoring::Enabled)
        .map(|(id, _)| *id)
        .collect()
}

/// 「全部恢复」要恢复的任务：由「全部暂停」暂停、仍存在且仍暂停的。
pub fn resume_targets(paused_by_tray: &[TaskId], tasks: &[(TaskId, Monitoring)]) -> Vec<TaskId> {
    paused_by_tray
        .iter()
        .copied()
        .filter(|id| {
            tasks
                .iter()
                .any(|(task, monitoring)| task == id && *monitoring == Monitoring::Paused)
        })
        .collect()
}

/// 创建托盘。失败时返回原因，由调用方记日志并改为关窗即退出。
pub fn create(app: &tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if !appindicator_available() {
        return Err("找不到 libayatana-appindicator3 或 libappindicator3".into());
    }
    build(app).map_err(|error| error.to_string())
}

fn build(app: &tauri::AppHandle) -> tauri::Result<()> {
    let resume = MenuItem::with_id(app, MENU_RESUME_ALL, "全部恢复监控", false, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, MENU_SHOW, "显示窗口", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, MENU_PAUSE_ALL, "全部暂停监控", true, None::<&str>)?,
            &resume,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?,
        ],
    )?;
    let tooltip = tooltip_text(QuitSnapshot::default());
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(&tooltip)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            if let Some(action) = TrayAction::from_menu_id(event.id().as_ref()) {
                handle(app, action);
            }
        })
        .on_tray_icon_event(|tray, event| {
            // Linux 的托盘不发出点击事件，从菜单「显示窗口」恢复。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                handle(tray.app_handle(), TrayAction::ShowWindow);
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;
    #[cfg(target_os = "linux")]
    let _ = tray.set_title(Some(title_text(QuitSnapshot::default())));
    #[cfg(not(target_os = "linux"))]
    let _ = tray;
    if let Some(state) = app.try_state::<TrayState>() {
        *lock(&state.resume_item) = Some(resume);
        *lock(&state.tooltip) = tooltip;
        state.available.store(true, Ordering::SeqCst);
    }
    Ok(())
}

pub fn set_tooltip<R: tauri::Runtime>(app: &tauri::AppHandle<R>, snapshot: QuitSnapshot) {
    let text = tooltip_text(snapshot);
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Err(error) = tray.set_tooltip(Some(&text)) {
            log::warn!("录播宝更新托盘提示失败：{error}");
        }
        #[cfg(target_os = "linux")]
        let _ = tray.set_title(Some(title_text(snapshot)));
    }
    if let Some(state) = app.try_state::<TrayState>() {
        *lock(&state.tooltip) = text;
    }
}

/// 调试命令用：「全部暂停」记下的任务和「全部恢复」是否可用。
pub fn debug_state(app: &tauri::AppHandle) -> String {
    let Some(state) = app.try_state::<TrayState>() else {
        return String::new();
    };
    let paused = lock(&state.paused_by_tray).clone();
    let resume_enabled = lock(&state.resume_item)
        .as_ref()
        .and_then(|item| item.is_enabled().ok());
    format!("pausedByTray={paused:?} resumeEnabled={resume_enabled:?}")
}

pub fn current_tooltip(app: &tauri::AppHandle) -> String {
    app.try_state::<TrayState>()
        .map(|state| lock(&state.tooltip).clone())
        .unwrap_or_default()
}

fn task_monitoring(tasks: &TaskManager) -> Vec<(TaskId, Monitoring)> {
    tasks
        .list()
        .into_iter()
        .map(|task| (task.id, task.monitoring))
        .collect()
}

/// 「全部暂停监控」：有录制时显示窗口、请前端确认；否则直接暂停。
fn request_pause_all(app: &tauri::AppHandle) {
    let recording = quit::refresh(app).recording;
    if recording > 0 {
        super::show_main_window(app);
        let _ = app.emit(PAUSE_ALL_PROMPT_EVENT, PausePrompt { recording });
    } else {
        in_background(app, pause_all);
    }
}

/// 阻塞线程上调用。
fn pause_all(app: &tauri::AppHandle) {
    let (Some(tasks), Some(state)) = (
        app.try_state::<Arc<TaskManager>>(),
        app.try_state::<TrayState>(),
    ) else {
        return;
    };
    let _serial = lock(&state.serial);
    let ids = pause_targets(&task_monitoring(&tasks));
    if ids.is_empty() {
        return;
    }
    match tasks.set_monitoring(&ids, false) {
        Ok(_) => {
            let mut paused = lock(&state.paused_by_tray);
            for id in ids {
                if !paused.contains(&id) {
                    paused.push(id);
                }
            }
            drop(paused);
            refresh_resume_item(&state);
        }
        Err(error) => log::error!("录播宝托盘暂停监控失败：{}", error.message),
    }
}

/// 阻塞线程上调用。
fn resume_all(app: &tauri::AppHandle) {
    let (Some(tasks), Some(state)) = (
        app.try_state::<Arc<TaskManager>>(),
        app.try_state::<TrayState>(),
    ) else {
        return;
    };
    let _serial = lock(&state.serial);
    let ids = resume_targets(&lock(&state.paused_by_tray), &task_monitoring(&tasks));
    match tasks.set_monitoring(&ids, true) {
        Ok(_) => {
            lock(&state.paused_by_tray).clear();
            refresh_resume_item(&state);
        }
        Err(error) => log::error!("录播宝托盘恢复监控失败：{}", error.message),
    }
}

fn refresh_resume_item(state: &TrayState) {
    let enabled = !lock(&state.paused_by_tray).is_empty();
    if let Some(item) = lock(&state.resume_item).as_ref() {
        let _ = item.set_enabled(enabled);
    }
}

/// 确认框里点「全部暂停」。在阻塞线程里执行，完成后才返回。
#[tauri::command]
pub async fn confirm_pause_all(app: tauri::AppHandle) {
    let _ = tauri::async_runtime::spawn_blocking(move || pause_all(&app)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_shows_the_recording_count() {
        let snapshot = QuitSnapshot {
            phase: QuitPhase::Idle,
            recording: 2,
            ..QuitSnapshot::default()
        };
        assert_eq!(tooltip_text(snapshot), "录播宝 · 录制中 2");
        let confirming = QuitSnapshot {
            phase: QuitPhase::Confirming,
            ..snapshot
        };
        assert_eq!(tooltip_text(confirming), "录播宝 · 录制中 2");
    }

    #[test]
    fn linux_title_shows_the_count_only_while_recording() {
        let idle = QuitSnapshot::default();
        assert_eq!(title_text(idle), "录播宝");
        let recording = QuitSnapshot {
            phase: QuitPhase::Idle,
            recording: 2,
            ..QuitSnapshot::default()
        };
        assert_eq!(title_text(recording), "录播宝 · 录制中 2");
        let finishing = QuitSnapshot {
            phase: QuitPhase::Finishing,
            recording: 0,
            ..QuitSnapshot::default()
        };
        assert_eq!(title_text(finishing), "录播宝 · 正在收尾…");
    }

    #[test]
    fn tooltip_says_finishing_while_quitting() {
        let snapshot = QuitSnapshot {
            phase: QuitPhase::Finishing,
            recording: 1,
            ..QuitSnapshot::default()
        };
        assert_eq!(tooltip_text(snapshot), "录播宝 · 正在收尾…");
    }

    #[test]
    fn menu_ids_and_debug_names_map_to_actions() {
        assert_eq!(
            TrayAction::from_menu_id(MENU_SHOW),
            Some(TrayAction::ShowWindow)
        );
        assert_eq!(TrayAction::from_menu_id(MENU_QUIT), Some(TrayAction::Quit));
        assert_eq!(TrayAction::from_menu_id("other"), None);
        assert_eq!(TrayAction::parse("click"), Some(TrayAction::ShowWindow));
        assert_eq!(TrayAction::parse("pause-all"), Some(TrayAction::PauseAll));
        assert_eq!(TrayAction::parse("resume-all"), Some(TrayAction::ResumeAll));
    }

    #[test]
    fn pause_all_only_pauses_monitored_tasks() {
        let tasks = [
            (1, Monitoring::Enabled),
            (2, Monitoring::Paused),
            (3, Monitoring::Enabled),
        ];
        assert_eq!(pause_targets(&tasks), vec![1, 3]);
    }

    #[test]
    fn resume_all_leaves_individually_paused_tasks_alone() {
        // 2 是用户单独暂停的；3 已被删除；4 在全部暂停后被用户单独恢复了。
        let tasks = [
            (1, Monitoring::Paused),
            (2, Monitoring::Paused),
            (4, Monitoring::Enabled),
        ];
        assert_eq!(resume_targets(&[1, 3, 4], &tasks), vec![1]);
        assert!(resume_targets(&[], &tasks).is_empty());
    }
}
