//! FFmpeg 组件与 Tauri 之间的胶水：`setup` 中登记并后台检查、推送状态事件、命令。
//! 组件管理本身见上级模块，不依赖 Tauri。

use std::sync::Arc;

use tauri::{Emitter, Manager};

use super::{FfmpegManager, FfmpegStatus, ManagerConfig, STATUS_EVENT};

/// 在 Tauri `setup` 中调用：登记状态并在后台开始检查，不阻塞窗口。
pub fn init(app: &tauri::AppHandle) -> tauri::Result<()> {
    let root = app
        .path()
        .app_local_data_dir()?
        .join("components")
        .join("ffmpeg");
    // macOS：上次崩溃留下的 ffmpeg 在调度开始录制之前结束（单实例，此时没有别的录播宝在用
    // 这个目录）。Windows、Linux 由内核保证 ffmpeg 随录播宝结束，这里什么都不做。
    let reaped = crate::recorder::process_guard::reap_orphans(&root);
    if reaped > 0 {
        log::warn!("结束了 {reaped} 个上次异常退出时留下的 ffmpeg");
    }
    let handle = app.clone();
    let manager = Arc::new(FfmpegManager::new(
        ManagerConfig::for_current_platform(root),
        Box::new(move |status: &FfmpegStatus| {
            let _ = handle.emit(STATUS_EVENT, status);
        }),
    ));
    app.manage(Arc::clone(&manager));
    tauri::async_runtime::spawn(async move {
        manager.ensure().await;
    });
    Ok(())
}

#[tauri::command]
pub fn get_ffmpeg_status(manager: tauri::State<'_, Arc<FfmpegManager>>) -> FfmpegStatus {
    manager.status()
}

/// 重新检查并安装。已在运行时不会再启动一个任务。返回调用时的状态，后续变化通过事件推送。
#[tauri::command]
pub fn retry_ffmpeg_install(manager: tauri::State<'_, Arc<FfmpegManager>>) -> FfmpegStatus {
    let task = Arc::clone(manager.inner());
    tauri::async_runtime::spawn(async move {
        task.ensure().await;
    });
    manager.status()
}

/// 忽略校验缓存重新计算哈希，不通过时重新安装。返回调用时的状态，后续变化通过事件推送。
#[tauri::command]
pub fn reverify_ffmpeg(manager: tauri::State<'_, Arc<FfmpegManager>>) -> FfmpegStatus {
    let task = Arc::clone(manager.inner());
    tauri::async_runtime::spawn(async move {
        task.reverify().await;
    });
    manager.status()
}
