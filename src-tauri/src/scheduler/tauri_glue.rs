//! 调度与 Tauri 之间的胶水：`setup` 中启动、退出时收尾、推送事件、命令。
//! 调度本身见上级模块，不依赖 Tauri。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{Emitter, Manager};

use super::{STATUS_EVENT, Scheduler, SchedulerStatus};
use crate::ffmpeg::FfmpegManager;
use crate::tasks::TaskManager;

/// 在 Tauri `setup` 中、`tasks::tauri_glue::init` 和 `ffmpeg::tauri_glue::init` 之后调用：创建调度器并在
/// 后台开始轮询。任务数据没打开时（已提示退出）不启动。
pub fn init(app: &tauri::AppHandle) -> tauri::Result<()> {
    let Some(tasks) = app.try_state::<Arc<TaskManager>>() else {
        return Ok(());
    };
    let ffmpeg = app.state::<Arc<FfmpegManager>>();
    let handle = app.clone();
    let scheduler = Scheduler::with_defaults(
        Arc::clone(tasks.inner()),
        Arc::clone(ffmpeg.inner()),
        Box::new(move |status: &SchedulerStatus| {
            crate::desktop::on_scheduler_status(&handle, status.recording);
            let _ = handle.emit(STATUS_EVENT, status);
        }),
    );
    tauri::async_runtime::spawn(scheduler.run());
    app.manage(scheduler);
    Ok(())
}

static EXIT_STARTED: AtomicBool = AtomicBool::new(false);
static EXIT_READY: AtomicBool = AtomicBool::new(false);
/// 安装更新前收尾期间被拦下的退出请求（退出码）。收尾由 [`finish_before_update`] 进行、
/// 它不会退出，所以要记下来，收尾后由调用方补发。
static PENDING_EXIT: Mutex<Option<i32>> = Mutex::new(None);

/// 处理 `RunEvent::ExitRequested`：第一次先阻止退出，调用 [`Scheduler::shutdown`] 等录制
/// 收尾后再退出，退出码沿用这次请求的 `code`（例如数据库打不开时的 1）；收尾完成后的
/// 退出请求直接放行。收尾进行中又来的请求记在 [`PENDING_EXIT`]：退出的收尾结束后本来就会
/// 退出；更新的收尾结束后由 [`finish_before_update`] 交给调用方补发。
pub fn on_exit_requested(app: &tauri::AppHandle, code: Option<i32>, api: &tauri::ExitRequestApi) {
    if EXIT_READY.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_exit();
    if EXIT_STARTED.swap(true, Ordering::SeqCst) {
        let mut pending = PENDING_EXIT
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        pending.get_or_insert(code.unwrap_or(0));
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match app.try_state::<Arc<Scheduler>>() {
            Some(scheduler) => scheduler.shutdown().await,
            None => {
                crate::recorder::stop_all().await;
            }
        }
        EXIT_READY.store(true, Ordering::SeqCst);
        app.exit(code.unwrap_or(0));
    });
}

/// 安装更新前收尾：与退出相同地停止调度和全部录制、等文件收尾（[`Scheduler::shutdown`]），
/// 但不退出。收尾已经开始时只等它完成。完成后的退出、重启请求直接放行，不会重复收尾。
///
/// 返回收尾期间被拦下的退出请求的退出码：调用方应放弃安装，改为 `app.exit(code)`。
pub async fn finish_before_update(app: &tauri::AppHandle) -> Option<i32> {
    if EXIT_STARTED.swap(true, Ordering::SeqCst) {
        while !EXIT_READY.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    } else {
        match app.try_state::<Arc<Scheduler>>() {
            Some(scheduler) => scheduler.shutdown().await,
            None => {
                crate::recorder::stop_all().await;
            }
        }
        EXIT_READY.store(true, Ordering::SeqCst);
    }
    PENDING_EXIT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
}

/// 录制是否已经收尾完毕（调度已停止）。安装更新失败后只在这之后才允许重启。
pub fn finished() -> bool {
    EXIT_READY.load(Ordering::SeqCst)
}

/// 系统直接结束进程时的收尾时限，短于注销、关机的宽限时间（Windows 约 5 秒起）。
const EXIT_FALLBACK_LIMIT: Duration = Duration::from_secs(4);

/// 系统直接结束进程时该怎么收尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitFallback {
    /// 收尾还没开始：限时同步收尾。
    Shutdown,
    /// 已经开始、还没完成：限时等它完成。
    WaitForShutdown,
    /// 已经完成：什么都不做。
    Done,
}

/// `started`：这次之前收尾是否已经开始（`EXIT_STARTED` 置位前的值）；`ready`：是否已完成。
fn exit_fallback(started: bool, ready: bool) -> ExitFallback {
    match (started, ready) {
        (_, true) => ExitFallback::Done,
        (true, false) => ExitFallback::WaitForShutdown,
        (false, false) => ExitFallback::Shutdown,
    }
}

/// 处理 `RunEvent::Exit`：没经过 [`on_exit_requested`] 的退出（macOS Dock 菜单的「退出」、
/// Windows 注销或关机）在这里同步收尾，最多 [`EXIT_FALLBACK_LIMIT`]。收尾已经开始时只等它
/// 完成，已经完成时直接返回，不会重复执行。Linux 的 SIGTERM 由桌面层转成 `app.exit(0)`。
pub fn on_exit(app: &tauri::AppHandle) {
    let deadline = Instant::now() + EXIT_FALLBACK_LIMIT;
    let started = EXIT_STARTED.swap(true, Ordering::SeqCst);
    match exit_fallback(started, EXIT_READY.load(Ordering::SeqCst)) {
        ExitFallback::Done => return,
        ExitFallback::WaitForShutdown => {
            while !EXIT_READY.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            return;
        }
        ExitFallback::Shutdown => {}
    }
    let scheduler = app
        .try_state::<Arc<Scheduler>>()
        .map(|state| Arc::clone(state.inner()));
    let finished = tauri::async_runtime::block_on(async move {
        tokio::time::timeout(EXIT_FALLBACK_LIMIT, async move {
            match scheduler {
                Some(scheduler) => scheduler.shutdown().await,
                None => {
                    crate::recorder::stop_all().await;
                }
            }
        })
        .await
        .is_ok()
    });
    if !finished {
        eprintln!("录播宝退出时收尾超时，部分录制可能没有正常结束");
    }
    EXIT_READY.store(true, Ordering::SeqCst);
}

/// 调度概况，供状态栏显示「下次检测」。变化时另有 `scheduler-status` 事件。
#[tauri::command]
pub fn get_scheduler_status(scheduler: tauri::State<'_, Arc<Scheduler>>) -> SchedulerStatus {
    scheduler.status()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_fallback_shuts_down_only_once() {
        assert_eq!(exit_fallback(false, false), ExitFallback::Shutdown);
        assert_eq!(exit_fallback(true, false), ExitFallback::WaitForShutdown);
        assert_eq!(exit_fallback(true, true), ExitFallback::Done);
        // 已完成时不管是否记为开始，都不再执行。
        assert_eq!(exit_fallback(false, true), ExitFallback::Done);
    }
}
