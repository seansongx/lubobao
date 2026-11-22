use tauri::Manager;

pub mod ffmpeg;
pub mod library;
pub mod platforms;
pub mod recorder;
pub mod scheduler;
pub mod tasks;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 只允许一个实例：第二次启动时把已有窗口显示到前台，新进程随即退出。
        // 必须最先注册。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            ffmpeg::init(app.handle())?;
            tasks::tauri_glue::init(app.handle())?;
            scheduler::tauri_glue::init(app.handle())?;
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
            tasks::commands::list_activity,
            scheduler::tauri_glue::get_scheduler_status,
            library::commands::list_library,
            library::commands::get_disk_space,
            library::commands::get_library_usage,
            library::commands::open_library_file,
            library::commands::open_library_folder,
            library::commands::reveal_library_item
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // 退出前停止调度和所有录制，等文件收尾。
            if let tauri::RunEvent::ExitRequested { code, api, .. } = &event {
                scheduler::tauri_glue::on_exit_requested(app, *code, api);
            }
        });
}
