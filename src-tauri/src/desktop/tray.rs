//! 托盘图标：显示窗口、全部暂停或恢复监控、退出；提示文字显示录制数。

use std::sync::{Arc, Mutex};

use tauri::Manager;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use super::quit::{self, QuitPhase, QuitSnapshot};
use crate::tasks::TaskManager;

const TRAY_ID: &str = "main";
const MENU_SHOW: &str = "show";
const MENU_PAUSE_ALL: &str = "pause-all";
const MENU_RESUME_ALL: &str = "resume-all";
const MENU_QUIT: &str = "quit";

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

/// 执行托盘操作。菜单、单击图标和调试命令都走这里。
pub fn handle(app: &tauri::AppHandle, action: TrayAction) {
    match action {
        TrayAction::ShowWindow => super::show_main_window(app),
        TrayAction::PauseAll => set_all_monitoring(app, false),
        TrayAction::ResumeAll => set_all_monitoring(app, true),
        TrayAction::Quit => quit::request_quit(app),
    }
}

/// 最近一次设置的提示文字；托盘对象不提供读取接口，记在这里供调试命令查看。
static TOOLTIP: Mutex<String> = Mutex::new(String::new());

pub fn current_tooltip() -> String {
    TOOLTIP
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// 托盘提示文字。
pub fn tooltip_text(snapshot: QuitSnapshot) -> String {
    match snapshot.phase {
        QuitPhase::Finishing => "录播宝 · 正在收尾…".into(),
        _ => format!("录播宝 · 录制中 {}", snapshot.recording),
    }
}

pub fn create(app: &tauri::AppHandle) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, MENU_SHOW, "显示窗口", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, MENU_PAUSE_ALL, "全部暂停监控", true, None::<&str>)?,
            &MenuItem::with_id(app, MENU_RESUME_ALL, "全部恢复监控", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?,
        ],
    )?;
    let tooltip = tooltip_text(QuitSnapshot::default());
    *TOOLTIP.lock().unwrap_or_else(|error| error.into_inner()) = tooltip.clone();
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(tooltip)
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
    builder.build(app)?;
    Ok(())
}

pub fn set_tooltip(app: &tauri::AppHandle, snapshot: QuitSnapshot) {
    let text = tooltip_text(snapshot);
    if let Some(tray) = app.tray_by_id(TRAY_ID)
        && let Err(error) = tray.set_tooltip(Some(&text))
    {
        eprintln!("录播宝更新托盘提示失败：{error}");
        return;
    }
    *TOOLTIP.lock().unwrap_or_else(|error| error.into_inner()) = text;
}

/// 对全部任务启用或暂停监控。暂停时与界面上的暂停一样停止录制。
fn set_all_monitoring(app: &tauri::AppHandle, enabled: bool) {
    let Some(tasks) = app.try_state::<Arc<TaskManager>>() else {
        return;
    };
    let ids: Vec<_> = tasks.list().into_iter().map(|task| task.id).collect();
    if let Err(error) = tasks.set_monitoring(&ids, enabled) {
        eprintln!("录播宝托盘切换监控失败：{}", error.message);
    }
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
    fn tooltip_says_finishing_while_quitting() {
        let snapshot = QuitSnapshot {
            phase: QuitPhase::Finishing,
            recording: 1,
        };
        assert_eq!(tooltip_text(snapshot), "录播宝 · 正在收尾…");
    }
}
