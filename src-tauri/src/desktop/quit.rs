//! 退出确认：有录制时先让前端弹窗确认，确认后交给调度收尾（`on_exit_requested`）。
//!
//! 退出入口都经过 [`request_quit`]：托盘「退出」、macOS 应用菜单的「退出录播宝」（Cmd+Q，
//! 见 `desktop::app_menu`）、关窗时选择「退出」。它们在有录制时先确认，再 `app.exit(0)`。
//! `app.exit` 到达 `RunEvent::ExitRequested` 时直接收尾。
//!
//! 不经过 `ExitRequested` 的退出（macOS 的 Dock「退出」、注销或关机）只有 `RunEvent::Exit`，
//! 无法确认，由 `scheduler::tauri_glue::on_exit` 在有限时间内同步收尾。
//!
//! 状态只在主线程上修改（命令、菜单、窗口和 run 事件都在主线程；调度线程上的录制数变化经
//! `run_on_main_thread` 转过来），因此 `quit-state` 事件不会乱序。

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

    /// 修改并返回新值。`change` 里不要再访问 `QuitState`（同一把锁）。
    fn update(&self, change: impl FnOnce(&mut QuitSnapshot)) -> QuitSnapshot {
        let mut guard = self.0.lock().unwrap_or_else(|error| error.into_inner());
        change(&mut guard);
        *guard
    }
}

/// 有录制时的退出请求要先确认；已在收尾时不再确认。
pub fn needs_confirmation(state: QuitSnapshot) -> bool {
    state.phase != QuitPhase::Finishing && state.recording > 0
}

fn state<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Option<tauri::State<'_, QuitState>> {
    app.try_state::<QuitState>()
}

/// 当前状态：录制数以调度的实时数字为准，调度没启动（例如任务数据打不开）时用记下的数字。
pub fn snapshot_now(scheduler_recording: Option<usize>, stored: QuitSnapshot) -> QuitSnapshot {
    QuitSnapshot {
        phase: stored.phase,
        recording: scheduler_recording.unwrap_or(stored.recording),
    }
}

fn current_recording<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> usize {
    refresh(app).recording
}

/// 读取当前状态，不写入（写入和发布只在 [`set_recording`]、[`set_phase`]）。先取调度的数字，
/// 再单独读本状态，任何时候都不在持有本状态的锁时访问别的状态。
pub fn refresh<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> QuitSnapshot {
    let scheduler = app
        .try_state::<Arc<Scheduler>>()
        .map(|scheduler| scheduler.status().recording);
    let stored = state(app).map(|state| state.get()).unwrap_or_default();
    snapshot_now(scheduler, stored)
}

pub fn phase<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> QuitPhase {
    state(app)
        .map(|state| state.get().phase)
        .unwrap_or_default()
}

fn publish<R: tauri::Runtime>(app: &tauri::AppHandle<R>, snapshot: QuitSnapshot) {
    tray::set_tooltip(app, snapshot);
    let _ = app.emit(QUIT_STATE_EVENT, snapshot);
}

/// 录制数变化时调用（主线程）：更新托盘提示，确认框里的数字也随之更新。
pub fn set_recording<R: tauri::Runtime>(app: &tauri::AppHandle<R>, recording: usize) {
    let Some(state) = state(app) else {
        return;
    };
    let before = state.get();
    let after = state.update(|snapshot| snapshot.recording = recording);
    if before != after {
        publish(app, after);
    }
}

fn set_phase<R: tauri::Runtime>(app: &tauri::AppHandle<R>, phase: QuitPhase) {
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

/// 退出入口（主线程）：有录制时显示窗口并弹出确认；收尾中只把窗口显示出来，让用户看到
/// 「正在收尾…」；没有录制就直接退出。
pub fn request_quit(app: &tauri::AppHandle) {
    let snapshot = refresh(app);
    if snapshot.phase == QuitPhase::Finishing {
        super::show_main_window(app);
        return;
    }
    if needs_confirmation(snapshot) {
        super::show_main_window(app);
        set_phase(app, QuitPhase::Confirming);
    } else {
        app.exit(0);
    }
}

/// 处理 `RunEvent::ExitRequested`：进入收尾，交给
/// [`crate::scheduler::tauri_glue::on_exit_requested`]。
///
/// 主窗口的关闭被拦截、永不销毁，所以这里只会收到 `app.exit` 发起的请求（确认之后、没有
/// 录制、启动失败），不需要再确认。
pub fn on_exit_requested(app: &tauri::AppHandle, code: Option<i32>, api: &tauri::ExitRequestApi) {
    if refresh(app).phase != QuitPhase::Finishing {
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

/// 确认框里点「取消」。已在收尾时不能取消。
#[tauri::command]
pub fn cancel_quit(app: tauri::AppHandle) -> QuitSnapshot {
    if phase(&app) != QuitPhase::Finishing {
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
    fn exit_with_recordings_asks_first() {
        assert!(needs_confirmation(snapshot(QuitPhase::Idle, 2)));
        assert!(needs_confirmation(snapshot(QuitPhase::Confirming, 1)));
    }

    #[test]
    fn exits_without_asking_when_nothing_is_recording_or_already_finishing() {
        assert!(!needs_confirmation(snapshot(QuitPhase::Idle, 0)));
        assert!(!needs_confirmation(snapshot(QuitPhase::Finishing, 3)));
    }

    #[test]
    fn snapshot_prefers_the_live_scheduler_count() {
        let stored = snapshot(QuitPhase::Confirming, 2);
        assert_eq!(
            snapshot_now(Some(0), stored),
            snapshot(QuitPhase::Confirming, 0)
        );
    }

    /// 没有登记调度（任务数据打不开）时用记下的数字。曾在持有锁时再次加锁而卡死；现在
    /// `refresh` 先后两次各自取值，`snapshot_now` 只做组合。
    #[test]
    fn snapshot_without_a_scheduler_uses_the_stored_count() {
        let stored = snapshot(QuitPhase::Finishing, 3);
        assert_eq!(snapshot_now(None, stored), stored);
        assert_eq!(
            snapshot_now(None, QuitSnapshot::default()),
            QuitSnapshot::default()
        );
    }
}
