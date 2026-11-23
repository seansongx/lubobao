//! 主窗口：按平台创建窗口、应用窗口材质，并在正式构建里关闭 WebView2 的浏览器快捷键。
//! 设计说明见 docs/design/native-ui.md。
//!
//! - Windows：保留系统标题栏（贴靠布局等系统行为不受影响），窗口透明后应用 Mica。Mica 只在
//!   Windows 11 上可用；Windows 10 上应用失败，前端得到 `none`，画不透明底色，看起来与普通窗口相同。
//! - macOS：Overlay 标题栏、隐藏标题，红绿灯落在侧栏顶部；整窗 `sidebar` vibrancy，内容区由前端画
//!   不透明底色，只有侧栏透出材质。
//! - Linux：不支持窗口材质，按配置创建不透明窗口和系统标题栏。
//!
//! 窗口先隐藏创建，窗口状态插件恢复大小和位置后再显示，避免先闪出默认大小。

use tauri::{Manager, WebviewWindow, WebviewWindowBuilder};

use super::MAIN_WINDOW;

/// 窗口材质实际是否生效，前端据此决定窗口底是否透明。
pub struct WindowMaterial(&'static str);

/// 前端启动时查询窗口材质：`mica`、`vibrancy` 或 `none`。
#[tauri::command]
pub fn get_window_material(state: tauri::State<'_, WindowMaterial>) -> &'static str {
    state.0
}

/// 在 `setup` 中最先调用：按 tauri.conf.json 里 `create: false` 的主窗口配置创建窗口。
pub fn create_main_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == MAIN_WINDOW)
        .cloned()
        .ok_or_else(|| tauri::Error::WindowNotFound)?;
    let builder = WebviewWindowBuilder::from_config(app, &config)?.visible(false);
    let window = platform_builder(builder).build()?;
    let material = apply_material(&window);
    app.manage(WindowMaterial(material));
    if !cfg!(debug_assertions) {
        disable_browser_accelerators(&window);
    }
    window.show()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_builder<'a, R: tauri::Runtime, M: Manager<R>>(
    builder: WebviewWindowBuilder<'a, R, M>,
) -> WebviewWindowBuilder<'a, R, M> {
    builder.transparent(true)
}

#[cfg(target_os = "macos")]
fn platform_builder<'a, R: tauri::Runtime, M: Manager<R>>(
    builder: WebviewWindowBuilder<'a, R, M>,
) -> WebviewWindowBuilder<'a, R, M> {
    use tauri::utils::config::WindowEffectsConfig;
    use tauri::window::{Effect, EffectState};
    builder
        .transparent(true)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        // 与前端 52px 的统一工具栏垂直居中
        .traffic_light_position(tauri::LogicalPosition::new(18.0, 20.0))
        .effects(WindowEffectsConfig {
            effects: vec![Effect::Sidebar],
            state: Some(EffectState::FollowsWindowActiveState),
            ..Default::default()
        })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_builder<'a, R: tauri::Runtime, M: Manager<R>>(
    builder: WebviewWindowBuilder<'a, R, M>,
) -> WebviewWindowBuilder<'a, R, M> {
    builder
}

#[cfg(target_os = "windows")]
fn apply_material(window: &WebviewWindow) -> &'static str {
    // None：不改深浅色，标题栏和 Mica 跟随系统主题。
    match window_vibrancy::apply_mica(window, None) {
        Ok(()) => "mica",
        Err(error) => {
            eprintln!("录播宝无法应用 Mica，使用不透明窗口：{error}");
            "none"
        }
    }
}

#[cfg(target_os = "macos")]
fn apply_material(_window: &WebviewWindow) -> &'static str {
    // vibrancy 在创建窗口时由 effects 应用，macOS 10.14 起都支持（最低支持 10.15）。
    "vibrancy"
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn apply_material(_window: &WebviewWindow) -> &'static str {
    "none"
}

/// 关闭 WebView2 的浏览器快捷键（Ctrl+F 查找、Ctrl+P 打印、F5 刷新、缩放、开发者工具等）。
/// 按键仍会传给页面，Ctrl+F 由前端用作聚焦搜索框；剪切、复制、粘贴、全选等编辑键不受影响。
/// 前端另有一层拦截（src/lib/native-behavior.ts），这里是第一道。
#[cfg(target_os = "windows")]
fn disable_browser_accelerators(window: &WebviewWindow) {
    let result = window.with_webview(|webview| {
        use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3;
        use windows_core::Interface;
        // SAFETY: 回调在 WebView2 所在的主线程上执行，controller 在 webview 存活期间有效。
        unsafe {
            let Ok(core) = webview.controller().CoreWebView2() else {
                return;
            };
            let Ok(settings) = core.Settings() else {
                return;
            };
            if let Ok(settings) = settings.cast::<ICoreWebView2Settings3>() {
                let _ = settings.SetAreBrowserAcceleratorKeysEnabled(false);
            }
        }
    });
    if let Err(error) = result {
        eprintln!("录播宝无法关闭浏览器快捷键：{error}");
    }
}

#[cfg(not(target_os = "windows"))]
fn disable_browser_accelerators(_window: &WebviewWindow) {}
