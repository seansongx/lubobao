//! 退出确认：有录制时先让前端弹窗确认，确认后交给调度收尾（`on_exit_requested`）。
//!
//! 退出入口：托盘「退出」调用 [`request_quit`]；macOS 的 Cmd+Q 和系统发起的退出到达
//! `RunEvent::ExitRequested` 时 `code` 为 `None`，在 [`on_exit_requested`] 中拦下确认。
//! `app.exit(code)` 发起的退出（确认之后、没有录制、启动失败）带 `code`，直接收尾。

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{Emitter, Manager};

use super::tray;
use crate::scheduler::Scheduler;

pub const QUIT_STATE_EVENT: &str = "quit-state";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum QuitPhase {
    #[default]
    Idle,
    /// 等用户在确认框里选择。
    Confirming,
    /// 已确认退出，正在停止录制、等文件收尾。
    Finishing,
}

/// `quit-state` 事件和 `get_quit_state` 的载荷。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuitSnapshot {
    pub phase: QuitPhase,
    /// 正在录制的任务数（含启动中、收尾中）。
    pub recording: usize,
}

#[derive(Default)]
pub struct QuitState(Mutex<QuitSnapshot>);

impl QuitState {
    fn get(&self) -> QuitSnapshot {
        *self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn update(&self, change: impl FnOnce(&mut QuitSnapshot)) -> QuitSnapshot {
        let mut guard = self.0.lock().unwrap_or_else(|error| error.into_inner());
        change(&mut guard);
        *guard
    }
}

/// 没有经过确认的退出请求是否要先确认。
pub fn needs_confirmation(code: Option<i32>, state: QuitSnapshot) -> bool {
    code.is_none() && state.phase != QuitPhase::Finishing && state.recording > 0
}

fn state(app: &tauri::AppHandle) -> Option<tauri::State<'_, QuitState>> {
    app.try_state::<QuitState>()
}

/// 以调度的实时数字为准，调度没启动时用最近一次事件里的数字。
fn current_recording(app: &tauri::AppHandle) -> usize {
    match app.try_state::<Arc<Scheduler>>() {
        Some(scheduler) => scheduler.status().recording,
        None => state(app).map(|state| state.get().recording).unwrap_or(0),
    }
}

fn publish(app: &tauri::AppHandle, snapshot: QuitSnapshot) {
    tray::set_tooltip(app, snapshot);
    let _ = app.emit(QUIT_STATE_EVENT, snapshot);
}

/// 录制数变化（`scheduler-status`）时调用：更新托盘提示，确认框里的数字也随之更新。
pub fn set_recording(app: &tauri::AppHandle, recording: usize) {
    let Some(state) = state(app) else {
        return;
    };
    let before = state.get();
    let after = state.update(|snapshot| snapshot.recording = recording);
    if before != after {
        publish(app, after);
    }
}

fn set_phase(app: &tauri::AppHandle, phase: QuitPhase) {
    let recording = current_recording(app);
    let Some(state) = state(app) else {
        return;
    };
    let snapshot = state.update(|snapshot| {
        snapshot.phase = phase;
        snapshot.recording = recording;
    });
    publish(app, snapshot);
}

fn ask(app: &tauri::AppHandle) {
    super::show_main_window(app);
    set_phase(app, QuitPhase::Confirming);
}

/// 托盘「退出」：有录制时弹出确认，没有就直接退出。
pub fn request_quit(app: &tauri::AppHandle) {
    let finishing = state(app).is_some_and(|state| state.get().phase == QuitPhase::Finishing);
    if finishing {
        return;
    }
    if current_recording(app) > 0 {
        ask(app);
    } else {
        app.exit(0);
    }
}

/// 处理 `RunEvent::ExitRequested`。不会绕过调度的收尾：除了先拦下确认，其余一律交给
/// [`crate::scheduler::tauri_glue::on_exit_requested`]。
pub fn on_exit_requested(app: &tauri::AppHandle, code: Option<i32>, api: &tauri::ExitRequestApi) {
    let snapshot = state(app)
        .map(|state| state.update(|snapshot| snapshot.recording = current_recording(app)))
        .unwrap_or_default();
    if needs_confirmation(code, snapshot) {
        api.prevent_exit();
        ask(app);
        return;
    }
    if snapshot.phase != QuitPhase::Finishing {
        set_phase(app, QuitPhase::Finishing);
    }
    crate::scheduler::tauri_glue::on_exit_requested(app, code, api);
}

#[tauri::command]
pub fn get_quit_state(app: tauri::AppHandle) -> QuitSnapshot {
    state(&app).map(|state| state.get()).unwrap_or_default()
}

/// 确认框里点「退出」：进入收尾并退出。
#[tauri::command]
pub fn confirm_quit(app: tauri::AppHandle) {
    set_phase(&app, QuitPhase::Finishing);
    app.exit(0);
}

/// 确认框里点「取消」。
#[tauri::command]
pub fn cancel_quit(app: tauri::AppHandle) -> QuitSnapshot {
    let finishing = state(&app).is_some_and(|state| state.get().phase == QuitPhase::Finishing);
    if !finishing {
        set_phase(&app, QuitPhase::Idle);
    }
    get_quit_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(phase: QuitPhase, recording: usize) -> QuitSnapshot {
        QuitSnapshot { phase, recording }
    }

    #[test]
    fn unconfirmed_exit_with_recordings_asks_first() {
        assert!(needs_confirmation(None, snapshot(QuitPhase::Idle, 2)));
        assert!(needs_confirmation(None, snapshot(QuitPhase::Confirming, 1)));
    }

    #[test]
    fn exits_without_asking_when_nothing_is_recording() {
        assert!(!needs_confirmation(None, snapshot(QuitPhase::Idle, 0)));
    }

    #[test]
    fn confirmed_or_programmatic_exits_go_straight_to_finishing() {
        assert!(!needs_confirmation(Some(0), snapshot(QuitPhase::Idle, 3)));
        assert!(!needs_confirmation(Some(1), snapshot(QuitPhase::Idle, 3)));
        assert!(!needs_confirmation(None, snapshot(QuitPhase::Finishing, 3)));
    }
}
