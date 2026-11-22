//! 调度与 Tauri 之间的胶水：`setup` 中启动、退出时收尾、推送事件、命令。
//! 调度本身见上级模块，不依赖 Tauri。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// 处理 `RunEvent::ExitRequested`：第一次先阻止退出，调用 [`Scheduler::shutdown`] 等录制
/// 收尾后再退出，退出码沿用这次请求的 `code`（例如数据库打不开时的 1）；收尾完成后的
/// 退出请求直接放行。
pub fn on_exit_requested(app: &tauri::AppHandle, code: Option<i32>, api: &tauri::ExitRequestApi) {
    if EXIT_READY.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_exit();
    if EXIT_STARTED.swap(true, Ordering::SeqCst) {
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

/// 系统直接结束进程时的收尾时限，短于注销、关机的宽限时间（Windows 约 5 秒起）。
const EXIT_FALLBACK_LIMIT: Duration = Duration::from_secs(4);

/// 处理 `RunEvent::Exit`：没经过 [`on_exit_requested`] 的退出（macOS Dock 菜单的「退出」、
/// 注销或关机）在这里同步收尾，最多 [`EXIT_FALLBACK_LIMIT`]。收尾已经开始时只等它完成，
/// 已经完成时直接返回，不会重复执行。
pub fn on_exit(app: &tauri::AppHandle) {
    let deadline = Instant::now() + EXIT_FALLBACK_LIMIT;
    if EXIT_STARTED.swap(true, Ordering::SeqCst) {
        while !EXIT_READY.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        return;
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
