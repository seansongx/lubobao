//! `lubobao-cli run`：前台常驻，监控直播间并自动录制。
//!
//! 启动顺序：注册退出信号 → 取得实例锁 → 打开任务数据库 → 开控制通道 → 启动调度和
//! FFmpeg 检查（自动下载在后台进行，失败后按 1、5、15 分钟、之后每 30 分钟重试）。
//! 收到 SIGINT、SIGTERM、SIGHUP（Windows 上为 Ctrl+C、Ctrl+Break、关闭控制台、关机）或
//! `lubobao-cli shutdown` 后走调度器的正常收尾：停止所有录制，等 FFmpeg 写完文件尾再退出。
//! 收尾期间再收到一次退出信号就立即退出（录制中的 ffmpeg 随进程结束）。

use std::sync::Arc;
use std::time::Duration;

use lubobao_core::ffmpeg::{FfmpegManager, FfmpegStatus};
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

/// 实例锁被占用时等待的时间：可能是另一个子命令正在写数据库（只有本地写入，很快）。
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// FFmpeg 自动安装失败后的重试间隔（分钟），之后每 [`RETRY_EVERY`] 分钟一次。
const RETRY_DELAYS: [u64; 3] = [1, 5, 15];
const RETRY_EVERY: u64 = 30;

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

/// 退出信号。启动时最先创建，之后收到的信号都不会走系统默认处理（直接结束进程）。
#[cfg(unix)]
struct ExitSignals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ExitSignals {
    fn new() -> Result<ExitSignals, String> {
        use tokio::signal::unix::{SignalKind, signal};
        let listen =
            |kind: SignalKind| signal(kind).map_err(|error| format!("无法监听退出信号：{error}"));
        Ok(ExitSignals {
            term: listen(SignalKind::terminate())?,
            int: listen(SignalKind::interrupt())?,
            hup: listen(SignalKind::hangup())?,
        })
    }

    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.term.recv() => "SIGTERM",
            _ = self.int.recv() => "SIGINT",
            _ = self.hup.recv() => "SIGHUP",
        }
    }
}

#[cfg(windows)]
struct ExitSignals {
    c: tokio::signal::windows::CtrlC,
    brk: tokio::signal::windows::CtrlBreak,
    close: tokio::signal::windows::CtrlClose,
    shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl ExitSignals {
    fn new() -> Result<ExitSignals, String> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
        let failed = |error: std::io::Error| format!("无法监听退出信号：{error}");
        Ok(ExitSignals {
            c: ctrl_c().map_err(failed)?,
            brk: ctrl_break().map_err(failed)?,
            close: ctrl_close().map_err(failed)?,
            shutdown: ctrl_shutdown().map_err(failed)?,
        })
    }

    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.c.recv() => "Ctrl+C",
            _ = self.brk.recv() => "Ctrl+Break",
            // 关闭控制台时系统只等约 5 秒，收尾不一定来得及。
            _ = self.close.recv() => "关闭控制台",
            _ = self.shutdown.recv() => "系统关机",
        }
    }
}

/// 自动安装 FFmpeg；可重试的失败按 [`RETRY_DELAYS`] 退避重试，直到可用或遇到不可重试的失败。
async fn keep_ffmpeg_installed(manager: Arc<FfmpegManager>) {
    let mut attempt = 0usize;
    loop {
        match manager.ensure().await {
            FfmpegStatus::Failed {
                retryable: true, ..
            } => {}
            _ => return,
        }
        let minutes = RETRY_DELAYS.get(attempt).copied().unwrap_or(RETRY_EVERY);
        attempt += 1;
        logging::warn(&format!(
            "FFmpeg 自动安装失败，{minutes} 分钟后第 {attempt} 次重试（也可以运行 lubobao-cli ffmpeg install）"
        ));
        tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
        logging::info(&format!("第 {attempt} 次重试安装 FFmpeg"));
    }
}

pub async fn run(dirs: Dirs, global: GlobalOpts) -> Result<(), String> {
    let mut signals = ExitSignals::new()?;
    let _lock = acquire_lock(&dirs).await?;
    logging::info(&format!(
        "录播宝命令行 {} 启动，数据目录 {}",
        env!("CARGO_PKG_VERSION"),
        dirs.data_dir.display()
    ));
    #[cfg(unix)]
    match dirs.secure_data_dir() {
        Ok(true) => logging::info("已把数据目录权限收紧为 0700"),
        Ok(false) => {}
        Err(error) => logging::warn(&format!("无法检查数据目录权限：{error}")),
    }
    // macOS：上次崩溃留下的 ffmpeg 在调度开始录制之前结束（已持有数据目录锁，没有别的实例在用
    // 这个目录）。其他系统由内核保证 ffmpeg 随进程结束，这里什么都不做。
    let reaped = lubobao_core::recorder::process_guard::reap_orphans(&dirs.ffmpeg_root());
    if reaped > 0 {
        logging::warn(&format!("结束了 {reaped} 个上次异常退出时留下的 ffmpeg"));
    }

    let tasks = Arc::new(tasks::open_in(
        &dirs.data_dir,
        &dirs.save_dir,
        logging::task_listener(),
    )?);
    if let Some(serde_json::Value::String(save)) = tasks.settings().get("live_save_path") {
        logging::info(&format!("录像目录 {save}"));
    }

    let ffmpeg = FfmpegSource::new(&dirs, global.ffmpeg.as_ref(), logging::ffmpeg_listener());
    if let FfmpegSource::External { path, version } = &ffmpeg {
        match version {
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

    let shutdown = Arc::new(Notify::new());
    let ctx = Arc::new(Ctx {
        tasks: Arc::clone(&tasks),
        scheduler: Some(Arc::clone(&scheduler)),
        ffmpeg: ffmpeg.clone(),
        dirs: dirs.clone(),
        shutdown: Some(Arc::clone(&shutdown)),
        started_at: Some(now_ms()),
    });
    let handler: Handler = Arc::new(move |request| {
        let ctx = Arc::clone(&ctx);
        Box::pin(async move { Response::from(ops::execute(&ctx, &request).await) })
    });
    // 先开控制通道，再开始检测和下载。
    let server = control::listen(&dirs, handler)
        .map_err(|error| format!("无法打开控制通道 {}：{error}", dirs.endpoint()))?;

    let join = scheduler.spawn();
    if let FfmpegSource::Managed(manager) = &ffmpeg {
        tokio::spawn(keep_ffmpeg_installed(Arc::clone(manager)));
    }

    let monitored = tasks.monitored().len();
    logging::info(&format!(
        "已启动：{} 个直播间，{monitored} 个监控中；控制通道 {}",
        tasks.list().len(),
        server.endpoint
    ));

    let reason = tokio::select! {
        signal = signals.recv() => signal,
        _ = shutdown.notified() => "lubobao-cli shutdown",
    };
    logging::info(&format!("收到 {reason}，正在停止录制并收尾…"));
    tokio::select! {
        _ = scheduler.shutdown() => {}
        signal = signals.recv() => {
            logging::warn(&format!("收尾期间再次收到 {signal}，立即退出"));
            std::process::exit(130);
        }
    }
    let _ = join.await;
    drop(server);
    logging::info("已退出");
    Ok(())
}
