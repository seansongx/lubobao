//! 关闭主窗口：按设置 `close_window_action` 隐藏到托盘、退出，或请前端三选一
//! （「最小化到托盘 / 退出录播宝 / 取消」，可记住选择）。与 StreamCap 的关闭对话框一致。
//! 托盘不可用时没有别的退出入口，关窗一律按退出处理。

use std::sync::Arc;

use serde_json::{Map, Value};
use tauri::{Emitter, Manager};

use super::{MAIN_WINDOW, hint, quit, tray};
use crate::tasks::TaskManager;

pub const CLOSE_WINDOW_SETTING: &str = "close_window_action";

/// 请前端弹出关窗三选一。没有载荷。
pub const CLOSE_PROMPT_EVENT: &str = "close-prompt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    Ask,
    Tray,
    Quit,
}

/// 关窗时怎么做。设置缺失或无法识别时按 `ask`。
pub fn close_action(setting: Option<&str>, tray_available: bool) -> CloseAction {
    if !tray_available {
        return CloseAction::Quit;
    }
    match setting {
        Some("tray") => CloseAction::Tray,
        Some("quit") => CloseAction::Quit,
        _ => CloseAction::Ask,
    }
}

fn setting(app: &tauri::AppHandle) -> Option<String> {
    let tasks = app.try_state::<Arc<TaskManager>>()?;
    tasks
        .settings()
        .get(CLOSE_WINDOW_SETTING)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 主窗口的 `CloseRequested`（已阻止关闭，主线程）。
pub fn on_close_requested(app: &tauri::AppHandle) {
    let action = close_action(setting(app).as_deref(), tray::available(app));
    perform(app, action);
}

fn perform(app: &tauri::AppHandle, action: CloseAction) {
    match action {
        CloseAction::Ask => {
            let _ = app.emit(CLOSE_PROMPT_EVENT, ());
        }
        CloseAction::Tray => {
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                let _ = window.hide();
            }
            hint::show_once(app);
        }
        CloseAction::Quit => quit::request_quit(app),
    }
}

/// 关窗三选一的结果。`choice` 为 `tray` 或 `quit`（取消不调用）；`remember` 时写入设置，
/// 以后不再询问，可在设置页改回。
#[tauri::command]
pub fn resolve_close(app: tauri::AppHandle, choice: String, remember: bool) -> Result<(), String> {
    let action = match choice.as_str() {
        "tray" => CloseAction::Tray,
        "quit" => CloseAction::Quit,
        other => return Err(format!("未知的选择：{other}")),
    };
    if remember && let Some(tasks) = app.try_state::<Arc<TaskManager>>() {
        let mut values = Map::new();
        values.insert(CLOSE_WINDOW_SETTING.into(), Value::String(choice));
        tasks
            .update_settings(values)
            .map_err(|error| error.message)?;
    }
    // 托盘在选择期间不会消失，但仍按当前是否可用决定。
    let action = match action {
        CloseAction::Tray if !tray::available(&app) => CloseAction::Quit,
        action => action,
    };
    perform(&app, action);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_remembered_choice() {
        assert_eq!(close_action(Some("tray"), true), CloseAction::Tray);
        assert_eq!(close_action(Some("quit"), true), CloseAction::Quit);
        assert_eq!(close_action(Some("ask"), true), CloseAction::Ask);
    }

    #[test]
    fn asks_when_the_setting_is_missing_or_unknown() {
        assert_eq!(close_action(None, true), CloseAction::Ask);
        assert_eq!(close_action(Some("minimize"), true), CloseAction::Ask);
    }

    #[test]
    fn quits_when_there_is_no_tray() {
        for setting in [None, Some("ask"), Some("tray"), Some("quit")] {
            assert_eq!(close_action(setting, false), CloseAction::Quit);
        }
    }

    #[test]
    fn the_setting_is_a_validated_enum_defaulting_to_ask() {
        let defaults = crate::tasks::settings::defaults(std::path::Path::new("videos"));
        assert_eq!(
            defaults.get(CLOSE_WINDOW_SETTING),
            Some(&Value::from("ask"))
        );
        for value in ["ask", "tray", "quit"] {
            assert!(
                crate::tasks::settings::validate(
                    &defaults,
                    CLOSE_WINDOW_SETTING,
                    &Value::from(value)
                )
                .is_ok()
            );
        }
        assert!(
            crate::tasks::settings::validate(
                &defaults,
                CLOSE_WINDOW_SETTING,
                &Value::from("minimize")
            )
            .is_err()
        );
    }
}
