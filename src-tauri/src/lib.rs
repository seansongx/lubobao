pub mod desktop;
pub mod ffmpeg;
pub mod library;
pub use lubobao_core::platforms;
pub use lubobao_core::recorder;
pub mod scheduler;
pub mod tasks;

/// 日志写到系统日志目录下的 `lubobao.log`（Windows `%LOCALAPPDATA%\com.xiaofany.lubobao\logs`，
/// macOS `~/Library/Logs/com.xiaofany.lubobao`，Linux `~/.local/share/com.xiaofany.lubobao/logs`），
/// 同时写到 stderr（开发时在终端里看）。录播宝自己和 Tauri 记 info 及以上，其他库只记警告和错误。
fn log_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};
    tauri_plugin_log::Builder::new()
        .targets([
            Target::new(TargetKind::LogDir {
                file_name: Some("lubobao".into()),
            }),
            Target::new(TargetKind::Stderr),
        ])
        .level(log::LevelFilter::Info)
        .filter(|metadata| {
            metadata.level() <= log::Level::Warn
                || metadata.target().starts_with("lubobao")
                || metadata.target().starts_with("tauri")
        })
        .max_file_size(5_000_000)
        .rotation_strategy(RotationStrategy::KeepSome(3))
        .timezone_strategy(TimezoneStrategy::UseLocal)
        .build()
}

/// panic 时先写进日志文件，再交给默认处理（打印到 stderr）。发布构建按 unwind 处理 panic，
/// 出问题的任务结束、应用继续运行，没有这一步就无从得知发生过 panic。
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic：{info}");
        default(info);
    }));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    log_panics();
    tauri::Builder::default()
        // 只允许一个实例：第二次启动时把已有窗口显示到前台，新进程随即退出。
        // 必须最先注册。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            desktop::show_main_window(app);
        }))
        // 日志紧随其后，之后各插件和 setup 里的日志都能写进文件。
        .plugin(log_plugin())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // 应用内更新（检查、下载、安装和安装失败后的重启见 desktop::update）。
        .plugin(tauri_plugin_updater::Builder::new().build())
        // 只记住主窗口大小、位置和最大化状态：显示与否由托盘和关窗逻辑决定，也不恢复 macOS 的全屏。
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(desktop::window::SAVED_STATE)
                .build(),
        )
        // 关闭主窗口：按设置隐藏到托盘（录制继续）、退出或询问。
        .on_window_event(desktop::on_window_event)
        .setup(|app| {
            // 主窗口在 tauri.conf.json 里不自动创建，由这里按平台加上透明、标题栏和材质。
            desktop::window::create_main_window(app.handle())?;
            // 开发调试面板（src/dev/debug-panel.tsx）要切换窗口明暗；这个权限只在调试构建里放开。
            #[cfg(debug_assertions)]
            tauri::Manager::add_capability(
                app,
                tauri::ipc::CapabilityBuilder::new("dev-debug-panel")
                    .window(desktop::MAIN_WINDOW)
                    .permission("core:window:allow-set-theme"),
            )?;
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
            desktop::window::get_window_material,
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
