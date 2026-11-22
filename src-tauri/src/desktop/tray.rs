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
        TrayAction::ResumeAll => resume_all(app),
        TrayAction::Quit => quit::request_quit(app),
    }
}

/// 托盘提示文字。
pub fn tooltip_text(snapshot: QuitSnapshot) -> String {
    match snapshot.phase {
        QuitPhase::Finishing => "录播宝 · 正在收尾…".into(),
        _ => format!("录播宝 · 录制中 {}", snapshot.recording),
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

/// 创建托盘。失败时返回错误，由调用方记日志并改为关窗即退出。
pub fn create(app: &tauri::AppHandle) -> tauri::Result<()> {
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
    // libappindicator 不支持提示文字，改在标题里显示。
    #[cfg(target_os = "linux")]
    let _ = tray.set_title(Some(&tooltip));
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
            eprintln!("录播宝更新托盘提示失败：{error}");
        }
        #[cfg(target_os = "linux")]
        let _ = tray.set_title(Some(&text));
    }
    if let Some(state) = app.try_state::<TrayState>() {
        *lock(&state.tooltip) = text;
    }
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
        pause_all(app);
    }
}

fn pause_all(app: &tauri::AppHandle) {
    let (Some(tasks), Some(state)) = (
        app.try_state::<Arc<TaskManager>>(),
        app.try_state::<TrayState>(),
    ) else {
        return;
    };
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
        Err(error) => eprintln!("录播宝托盘暂停监控失败：{}", error.message),
    }
}

fn resume_all(app: &tauri::AppHandle) {
    let (Some(tasks), Some(state)) = (
        app.try_state::<Arc<TaskManager>>(),
        app.try_state::<TrayState>(),
    ) else {
        return;
    };
    let ids = resume_targets(&lock(&state.paused_by_tray), &task_monitoring(&tasks));
    match tasks.set_monitoring(&ids, true) {
        Ok(_) => {
            lock(&state.paused_by_tray).clear();
            refresh_resume_item(&state);
        }
        Err(error) => eprintln!("录播宝托盘恢复监控失败：{}", error.message),
    }
}

fn refresh_resume_item(state: &TrayState) {
    let enabled = !lock(&state.paused_by_tray).is_empty();
    if let Some(item) = lock(&state.resume_item).as_ref() {
        let _ = item.set_enabled(enabled);
    }
}

/// 确认框里点「全部暂停」。
#[tauri::command]
pub fn confirm_pause_all(app: tauri::AppHandle) {
    pause_all(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_shows_the_recording_count() {
        let snapshot = QuitSnapshot {
            phase: QuitPhase::Idle,
            recording: 2,
        };
        assert_eq!(tooltip_text(snapshot), "录播宝 · 录制中 2");
        let confirming = QuitSnapshot {
            phase: QuitPhase::Confirming,
            ..snapshot
        };
        assert_eq!(tooltip_text(confirming), "录播宝 · 录制中 2");
    }

    #[test]
    fn tooltip_says_finishing_while_quitting() {
        let snapshot = QuitSnapshot {
            phase: QuitPhase::Finishing,
            recording: 1,
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
