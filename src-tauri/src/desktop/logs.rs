//! 设置 →「关于」的「打开日志」：在文件管理器里选中 `lubobao.log`，方便反馈问题时附上。

use tauri::{AppHandle, Manager};

/// 日志文件名，与 `lib.rs` 里 `log_plugin` 的 `file_name` 一致。
const LOG_FILE: &str = "lubobao.log";

/// 在文件管理器中显示日志文件；文件还不存在时显示日志目录。
#[tauri::command]
pub async fn reveal_log_file(app: AppHandle) -> Result<(), String> {
    let dir = app
        .path()
        .app_log_dir()
        .map_err(|error| format!("找不到日志目录：{error}"))?;
    tauri::async_runtime::spawn_blocking(move || {
        let file = dir.join(LOG_FILE);
        let target = if file.is_file() { file } else { dir };
        tauri_plugin_opener::reveal_item_in_dir(&target)
            .map_err(|error| format!("无法打开日志位置：{error}"))
    })
    .await
    .map_err(|error| format!("打开日志位置中断：{error}"))?
}
