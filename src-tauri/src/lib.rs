pub mod ffmpeg;
pub mod platforms;
pub mod recorder;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            ffmpeg::init(app.handle())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ffmpeg::get_ffmpeg_status,
            ffmpeg::retry_ffmpeg_install
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
