pub mod ffmpeg;
pub mod platforms;
pub mod tasks;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            ffmpeg::init(app.handle())?;
            tasks::init(app.handle())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ffmpeg::get_ffmpeg_status,
            ffmpeg::retry_ffmpeg_install,
            tasks::commands::list_tasks,
            tasks::commands::add_task,
            tasks::commands::update_task,
            tasks::commands::delete_tasks,
            tasks::commands::set_monitoring,
            tasks::commands::stop_recording,
            tasks::commands::get_settings,
            tasks::commands::update_settings,
            tasks::commands::list_activity
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
