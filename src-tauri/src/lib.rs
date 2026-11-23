pub mod desktop;
pub mod ffmpeg;
pub mod library;
pub use lubobao_core::platforms;
pub use lubobao_core::recorder;
pub mod scheduler;
pub mod tasks;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 只允许一个实例：第二次启动时把已有窗口显示到前台，新进程随即退出。
        // 必须最先注册。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            desktop::show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // 应用内更新（检查、下载、安装和安装失败后的重启见 desktop::update）。
        .plugin(tauri_plugin_updater::Builder::new().build())
        // 关闭主窗口：按设置隐藏到托盘（录制继续）、退出或询问。
        .on_window_event(desktop::on_window_event)
        .setup(|app| {
            ffmpeg::tauri_glue::init(app.handle())?;
            tasks::tauri_glue::init(app.handle())?;
            scheduler::tauri_glue::init(app.handle())?;
            desktop::init(app.handle())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ffmpeg::tauri_glue::get_ffmpeg_status,
            ffmpeg::tauri_glue::retry_ffmpeg_install,
            ffmpeg::tauri_glue::reverify_ffmpeg,
            tasks::commands::list_tasks,
            tasks::commands::add_task,
            tasks::commands::update_task,
            tasks::commands::delete_tasks,
            tasks::commands::set_monitoring,
            tasks::commands::stop_recording,
            tasks::commands::start_recording,
            tasks::commands::get_settings,
            tasks::commands::get_setting_defaults,
            tasks::commands::update_settings,
            tasks::commands::list_activity,
            scheduler::tauri_glue::get_scheduler_status,
            desktop::quit::get_quit_state,
            desktop::quit::confirm_quit,
            desktop::quit::cancel_quit,
            desktop::debug_desktop_action,
            desktop::close::resolve_close,
            desktop::tray::confirm_pause_all,
            desktop::update::check_update,
            desktop::update::download_update,
            desktop::update::install_update,
            desktop::update::restart_after_failed_update,
            library::commands::list_library,
            library::commands::get_disk_space,
            library::commands::get_library_usage,
            library::commands::open_library_file,
            library::commands::open_library_folder,
            library::commands::reveal_library_item
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        // 退出前停止调度和所有录制，等文件收尾（确认在退出入口处理）；系统直接结束时同步兜底；
        // macOS 点 Dock 图标时重新显示窗口。
        .run(|app, event| desktop::on_run_event(app, &event));
}
