//! 桌面外壳：托盘、关窗、退出确认、应用菜单和系统通知。只在 Tauri 桌面端使用，核心模块不依赖它。
//!
//! 数据来源：任务事件由 `tasks::tauri_glue` 的监听器转给 [`on_task_event`]，录制数由
//! `scheduler::tauri_glue` 的状态回调转给 [`on_scheduler_status`]。这两处在调度或任务的线程上
//! 被调用，这里只更新自己的小状态，其余（读设置、发通知、改退出状态、托盘提示、发事件）
//! 一律经 `run_on_main_thread` 串行到主线程，不阻塞调用方。
//!
//! 退出一律经过 [`crate::scheduler::tauri_glue::on_exit_requested`]，由它停止录制、等文件收尾；
//! 不经过它的系统退出（macOS Dock「退出」、Windows 注销关机）只有 `RunEvent::Exit`，由
//! [`crate::scheduler::tauri_glue::on_exit`] 限时兜底。Linux 注销关机时进程收到 SIGTERM，这里
//! 监听后改走 `app.exit(0)`，与普通退出一样收尾。

pub mod close;
mod hint;
pub mod notify;
pub mod quit;
pub mod tray;
pub mod update;

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tauri::Manager;
#[cfg(target_os = "macos")]
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};

use crate::tasks::{Monitoring, TaskEvent, TaskManager};

pub const MAIN_WINDOW: &str = "main";

#[cfg(target_os = "macos")]
const MENU_APP_QUIT: &str = "app-quit";

/// 通知用的运行时状态。
#[derive(Default)]
struct Desktop {
    tracker: Mutex<notify::Tracker>,
    pending: Mutex<PendingLive>,
}

#[derive(Default)]
struct PendingLive {
    items: Vec<notify::LiveItem>,
    scheduled: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 在 `setup` 中、任务和调度初始化之后调用。托盘创建失败只记日志。
pub fn init(app: &tauri::AppHandle) -> tauri::Result<()> {
    app.manage(quit::QuitState::default());
    update::init(app);
    app.manage(tray::TrayState::default());
    let desktop = Desktop::default();
    if let Some(tasks) = app.try_state::<Arc<TaskManager>>() {
        // 记下已有任务；启动前就处于需处理的不再通知。
        let mut tracker = lock(&desktop.tracker);
        for task in tasks.list() {
            let _ = tracker.on_updated(&task);
        }
    }
    app.manage(desktop);

    if let Err(error) = tray::create(app) {
        eprintln!("录播宝无法创建托盘图标，关闭窗口将直接退出：{error}");
    }
    // 只有 macOS 有应用菜单：替换默认菜单里直接结束进程的「退出」。
    #[cfg(target_os = "macos")]
    {
        app.set_menu(app_menu(app)?)?;
        app.on_menu_event(|app, event| {
            if event.id().as_ref() == MENU_APP_QUIT {
                quit::request_quit(app);
            }
        });
    }
    #[cfg(target_os = "linux")]
    exit_on_sigterm(app);
    tray::set_tooltip(app, quit::refresh(app));
    Ok(())
}

/// Linux 注销、关机或 `kill` 时进程收到 SIGTERM，不会有 `RunEvent::Exit`。收到后按普通退出
/// 处理（不再确认），由调度停止录制、等文件收尾。
#[cfg(target_os = "linux")]
fn exit_on_sigterm(app: &tauri::AppHandle) {
    use tokio::signal::unix::{SignalKind, signal};
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                if terminate.recv().await.is_some() {
                    handle.exit(0);
                }
            }
            Err(error) => eprintln!("录播宝无法监听 SIGTERM：{error}"),
        }
    });
}

/// macOS 应用菜单：与 Tauri 默认菜单相同，只是「退出录播宝」（Cmd+Q）改为先确认、再收尾。
/// 保留「编辑」（输入框里的撤销、剪切、复制、粘贴、全选快捷键靠它）、「显示」（进入全屏）
/// 和「窗口」。
#[cfg(target_os = "macos")]
fn app_menu(app: &tauri::AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let name = app.package_info().name.clone();
    let quit = MenuItem::with_id(app, MENU_APP_QUIT, "退出录播宝", true, Some("CmdOrCtrl+Q"))?;
    let app_menu = Submenu::with_items(
        app,
        name,
        true,
        &[
            &PredefinedMenuItem::about(app, None, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::show_all(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let edit = Submenu::with_items(
        app,
        "编辑",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;
    let window = Submenu::with_items(
        app,
        "窗口",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;
    let view = Submenu::with_items(
        app,
        "显示",
        true,
        &[&PredefinedMenuItem::fullscreen(app, None)?],
    )?;
    Menu::with_items(app, &[&app_menu, &edit, &view, &window])
}

/// 显示主窗口并置于前台：托盘、单实例、退出确认、macOS 点 Dock 图标时调用。
pub fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// 正在退出收尾：丢弃待发的通知。
fn quitting(app: &tauri::AppHandle) -> bool {
    quit::phase(app) == quit::QuitPhase::Finishing
}

/// 主窗口隐藏或最小化。
fn main_window_in_background(app: &tauri::AppHandle) -> bool {
    app.get_webview_window(MAIN_WINDOW).is_none_or(|window| {
        !window.is_visible().unwrap_or(false) || window.is_minimized().unwrap_or(false)
    })
}

/// 任务事件（任意线程）：更新通知跟踪，需要发通知时转到主线程。
pub fn on_task_event(app: &tauri::AppHandle, event: &TaskEvent) {
    let Some(desktop) = app.try_state::<Desktop>() else {
        return;
    };
    match event {
        TaskEvent::Updated(task) => {
            if task.monitoring == Monitoring::Enabled {
                tray::on_task_enabled(app, task.id);
            }
            let notice = lock(&desktop.tracker).on_updated(task);
            if let Some(notice) = notice {
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || {
                    // 退出收尾时停录制会引起状态变化，不再弹通知。
                    let allowed = !quitting(&handle)
                        && handle
                            .try_state::<Arc<TaskManager>>()
                            .map(|tasks| notify::NotifySettings::from_settings(&tasks.settings()))
                            .is_some_and(|settings| notify::allow_needs_action(&settings));
                    if allowed {
                        notify::show(&handle, &notice);
                    }
                });
            }
        }
        TaskEvent::Removed(ids) => lock(&desktop.tracker).on_removed(ids),
        TaskEvent::Activity(entry) => {
            let item = lock(&desktop.tracker).on_activity(entry);
            if let Some(item) = item {
                queue_live(app, &desktop, item);
            }
        }
    }
}

/// 攒下开播、下播，合并窗口结束后在主线程一起发。
fn queue_live(app: &tauri::AppHandle, desktop: &Desktop, item: notify::LiveItem) {
    let schedule = {
        let mut pending = lock(&desktop.pending);
        pending.items.push(item);
        !std::mem::replace(&mut pending.scheduled, true)
    };
    if !schedule {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(notify::MERGE_WINDOW_MS)).await;
        let app = handle.clone();
        let _ = handle.run_on_main_thread(move || flush_live(&app));
    });
}

fn flush_live(app: &tauri::AppHandle) {
    let (Some(desktop), Some(tasks)) = (
        app.try_state::<Desktop>(),
        app.try_state::<Arc<TaskManager>>(),
    ) else {
        return;
    };
    let items = {
        let mut pending = lock(&desktop.pending);
        pending.scheduled = false;
        std::mem::take(&mut pending.items)
    };
    if quitting(app) {
        return;
    }
    let settings = notify::NotifySettings::from_settings(&tasks.settings());
    let background = main_window_in_background(app);
    for change in [notify::LiveChange::Start, notify::LiveChange::End] {
        if let Some(notice) = notify::live_notice(change, &items, &settings, background) {
            notify::show(app, &notice);
        }
    }
}

/// 调度状态变化（调度线程）：录制数转到主线程更新退出状态和托盘提示。
pub fn on_scheduler_status(app: &tauri::AppHandle, recording: usize) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || quit::set_recording(&handle, recording));
}

/// 窗口事件：关闭主窗口时按设置隐藏到托盘、退出或询问。
pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != MAIN_WINDOW {
        return;
    }
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        close::on_close_requested(window.app_handle());
    }
}

/// 应用事件：退出请求交给调度收尾；系统直接结束（`Exit`）时同步兜底收尾；macOS 点 Dock
/// 图标时重新显示窗口。
pub fn on_run_event(app: &tauri::AppHandle, event: &tauri::RunEvent) {
    match event {
        tauri::RunEvent::ExitRequested { code, api, .. } => {
            quit::on_exit_requested(app, *code, api);
        }
        tauri::RunEvent::Exit => crate::scheduler::tauri_glue::on_exit(app),
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => show_main_window(app),
        _ => {}
    }
}

/// 只在调试构建可用：不经过系统托盘和鼠标，直接触发托盘操作或关窗，返回当前托盘提示。
/// `action`：`show`、`click`、`pause-all`、`resume-all`、`quit`、`close-window`（走关窗流程），
/// `tooltip`（只读取提示），或 `tray-state`（「全部暂停」记下的任务和恢复项是否可用）。
/// 发布构建一律拒绝。
#[tauri::command]
pub fn debug_desktop_action(app: tauri::AppHandle, action: String) -> Result<String, String> {
    if !cfg!(debug_assertions) {
        return Err("仅调试构建可用".into());
    }
    match action.as_str() {
        "tooltip" => {}
        "tray-state" => return Ok(tray::debug_state(&app)),
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
    Ok(tray::current_tooltip(&app))
}
