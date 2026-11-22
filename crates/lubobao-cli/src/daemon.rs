//! `lubobao-cli run`：前台常驻，监控直播间并自动录制。
//!
//! 启动顺序：取得实例锁 → 打开任务数据库 → 准备 FFmpeg（自动下载在后台进行）→ 启动调度
//! → 开控制通道。收到 SIGINT / SIGTERM（Windows 上为 Ctrl+C、Ctrl+Break、关闭控制台）或
//! `lubobao-cli shutdown` 后走调度器的正常收尾：停止所有录制，等 FFmpeg 写完文件尾，再退出。
//! 收尾期间再收到一次退出信号就立即退出（录制中的 ffmpeg 随进程结束）。

use std::sync::Arc;
use std::time::Duration;

use lubobao_core::scheduler::{PlatformSource, Scheduler};
use lubobao_core::tasks::{self, now_ms};
use tokio::sync::Notify;

use crate::GlobalOpts;
use crate::control::{self, Handler};
use crate::engine::{CliEngine, FfmpegSource};
use crate::logging;
use crate::ops::{self, Ctx};
use crate::paths::Dirs;
use crate::protocol::Response;

/// 实例锁被占用时等待的时间：可能是另一个子命令正在直接操作数据库。
const LOCK_WAIT: Duration = Duration::from_secs(5);

async fn acquire_lock(dirs: &Dirs) -> Result<std::fs::File, String> {
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        if let Some(lock) = dirs.try_lock()? {
            return Ok(lock);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "数据目录 {} 已有 lubobao-cli run 在运行（同一个数据目录只能运行一个）",
                dirs.data_dir.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 等一个退出信号，返回信号名。
#[cfg(unix)]
async fn exit_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        logging::warn("无法监听退出信号");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    }
}

#[cfg(windows)]
async fn exit_signal() -> &'static str {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
    let (Ok(mut c), Ok(mut brk), Ok(mut close), Ok(mut shutdown)) =
        (ctrl_c(), ctrl_break(), ctrl_close(), ctrl_shutdown())
    else {
        logging::warn("无法监听退出信号");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = c.recv() => "Ctrl+C",
        _ = brk.recv() => "Ctrl+Break",
        _ = close.recv() => "关闭控制台",
        _ = shutdown.recv() => "系统关机",
    }
}

pub async fn run(dirs: Dirs, global: GlobalOpts) -> Result<(), String> {
    let _lock = acquire_lock(&dirs).await?;
    logging::info(&format!(
        "录播宝命令行 {} 启动，数据目录 {}",
        env!("CARGO_PKG_VERSION"),
        dirs.data_dir.display()
    ));

    let tasks = Arc::new(tasks::open_in(
        &dirs.data_dir,
        &dirs.save_dir,
        logging::task_listener(),
    )?);
    if let Some(serde_json::Value::String(save)) = tasks.settings().get("live_save_path") {
        logging::info(&format!("录像目录 {save}"));
    }

    let ffmpeg = FfmpegSource::new(&dirs, global.ffmpeg.as_ref(), logging::ffmpeg_listener());
    match &ffmpeg {
        FfmpegSource::External { path, version } => match version {
            Ok(version) => logging::info(&format!(
                "使用指定的 FFmpeg：{version}（{}）",
                path.display()
            )),
            Err(reason) => {
                return Err(format!(
                    "指定的 FFmpeg 无法运行 {}：{reason}",
                    path.display()
                ));
            }
        },
        FfmpegSource::Managed(manager) => {
            let manager = Arc::clone(manager);
            tokio::spawn(async move {
                manager.ensure().await;
            });
        }
    }

    let scheduler = Scheduler::new(
        Arc::clone(&tasks),
        Arc::new(PlatformSource),
        Arc::new(CliEngine {
            ffmpeg: ffmpeg.clone(),
        }),
        Box::new(|_status| {}),
    );
    let join = scheduler.spawn();

    let shutdown = Arc::new(Notify::new());
    let ctx = Arc::new(Ctx {
        tasks: Arc::clone(&tasks),
        scheduler: Some(Arc::clone(&scheduler)),
        ffmpeg,
        dirs: dirs.clone(),
        shutdown: Some(Arc::clone(&shutdown)),
        started_at: Some(now_ms()),
    });
    let handler: Handler = Arc::new(move |request| {
        let ctx = Arc::clone(&ctx);
        Box::pin(async move { Response::from(ops::execute(&ctx, &request).await) })
    });
    let server = match control::listen(&dirs, handler) {
        Ok(server) => server,
        Err(error) => {
            scheduler.shutdown().await;
            return Err(format!("无法打开控制通道 {}：{error}", dirs.endpoint()));
        }
    };

    let monitored = tasks.monitored().len();
    logging::info(&format!(
        "已启动：{} 个直播间，{monitored} 个监控中；控制通道 {}",
        tasks.list().len(),
        dirs.endpoint()
    ));

    let reason = tokio::select! {
        signal = exit_signal() => signal,
        _ = shutdown.notified() => "lubobao-cli shutdown",
    };
    logging::info(&format!("收到 {reason}，正在停止录制并收尾…"));
    tokio::select! {
        _ = scheduler.shutdown() => {}
        signal = exit_signal() => {
            logging::warn(&format!("收尾期间再次收到 {signal}，立即退出"));
            std::process::exit(130);
        }
    }
    let _ = join.await;
    drop(server);
    logging::info("已退出");
    Ok(())
}
