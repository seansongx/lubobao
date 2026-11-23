//! 主窗口：按平台创建窗口、应用窗口材质，并在正式构建里关闭 WebView2 的浏览器快捷键。
//! 设计说明见 docs/design/native-ui.md。
//!
//! - Windows：保留系统标题栏（贴靠布局等系统行为不受影响）。Mica 只在 Windows 11（build 22000 起）
//!   可用，所以建窗前先判断系统版本：Windows 11 才建透明窗口并应用 Mica；Windows 10 建普通的不透明
//!   窗口，前端画不透明底色。透明窗口一旦建好就无法改回，若在 Windows 10 上也建成透明，首帧之前、
//!   放大窗口新露出的区域和 WebView2 卡住时都会透出桌面。
//! - macOS：Overlay 标题栏、隐藏标题，红绿灯落在侧栏顶部；整窗 `sidebar` vibrancy，内容区由前端画
//!   不透明底色，只有侧栏透出材质。
//! - Linux：不支持窗口材质，按配置创建不透明窗口和系统标题栏。
//!
//! 平台和材质在首帧前由初始化脚本交给前端（`window.__LUBOBAO_SHELL__`，index.html 读取），
//! 启动时不会先画不透明底色再变透明；`get_window_material` 返回实际结果，应用 Mica 失败时前端据此改回。
//! 窗口先隐藏创建，窗口状态插件恢复大小和位置后再显示，避免先闪出默认大小。

use tauri::{Manager, WebviewWindow, WebviewWindowBuilder};

use super::MAIN_WINDOW;

/// 窗口状态插件保存和恢复的内容。
pub const SAVED_STATE: tauri_plugin_window_state::StateFlags =
    tauri_plugin_window_state::StateFlags::SIZE
        .union(tauri_plugin_window_state::StateFlags::POSITION)
        .union(tauri_plugin_window_state::StateFlags::MAXIMIZED);

/// 窗口材质实际是否生效，前端据此决定窗口底是否透明。
pub struct WindowMaterial(&'static str);

/// 前端启动时查询窗口材质：`mica`、`vibrancy` 或 `none`。
#[tauri::command]
pub fn get_window_material(state: tauri::State<'_, WindowMaterial>) -> &'static str {
    state.0
}

/// 首个支持 Mica 的 Windows 版本（Windows 11 21H2）。
#[cfg(any(target_os = "windows", test))]
const FIRST_MICA_BUILD: u32 = 22000;

/// 该 Windows build 是否支持 Mica。
#[cfg(any(target_os = "windows", test))]
fn supports_mica(build: u32) -> bool {
    build >= FIRST_MICA_BUILD
}

/// 当前系统的 Windows build 号（RtlGetVersion 不受兼容性清单影响）。读取失败时为 0。
#[cfg(target_os = "windows")]
fn windows_build() -> u32 {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform_id: u32,
        csd_version: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
    }
    let mut info = OsVersionInfoW {
        size: std::mem::size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform_id: 0,
        csd_version: [0; 128],
    };
    // SAFETY: info 是按 OSVERSIONINFOW 布局、已填好 size 的结构体，RtlGetVersion 只写入它。
    let status = unsafe { RtlGetVersion(&mut info) };
    if status == 0 { info.build } else { 0 }
}

/// 本次建窗打算使用的材质（建窗前决定，透明与否随之确定）。
fn planned_material() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        if supports_mica(windows_build()) {
            "mica"
        } else {
            "none"
        }
    }
    #[cfg(target_os = "macos")]
    {
        // vibrancy 在 macOS 10.14 起都支持（最低支持 10.15），创建窗口时由 effects 应用。
        "vibrancy"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "none"
    }
}

const PLATFORM: &str = if cfg!(target_os = "windows") {
    "windows"
} else if cfg!(target_os = "macos") {
    "macos"
} else {
    "linux"
};

/// 首帧前交给前端的平台和材质，index.html 的内联脚本据此写入 data-platform 和 data-material。
fn shell_script(material: &str) -> String {
    format!("window.__LUBOBAO_SHELL__ = {{ platform: \"{PLATFORM}\", material: \"{material}\" }};")
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
        .ok_or(tauri::Error::WindowNotFound)?;
    let planned = planned_material();
    let builder = WebviewWindowBuilder::from_config(app, &config)?
        .visible(false)
        .initialization_script(shell_script(planned));
    let window = platform_builder(builder, planned).build()?;
    let material = apply_material(&window, planned);
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
    planned: &str,
) -> WebviewWindowBuilder<'a, R, M> {
    // 只有要用 Mica 时才建透明窗口；Windows 10 保持普通的不透明窗口。
    builder.transparent(planned == "mica")
}

#[cfg(target_os = "macos")]
fn platform_builder<'a, R: tauri::Runtime, M: Manager<R>>(
    builder: WebviewWindowBuilder<'a, R, M>,
    _planned: &str,
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
    _planned: &str,
) -> WebviewWindowBuilder<'a, R, M> {
    builder
}

#[cfg(target_os = "windows")]
fn apply_material(window: &WebviewWindow, planned: &'static str) -> &'static str {
    if planned != "mica" {
        return "none";
    }
    // None：不改深浅色，标题栏和 Mica 跟随系统主题。
    match window_vibrancy::apply_mica(window, None) {
        Ok(()) => "mica",
        Err(error) => {
            eprintln!("录播宝无法应用 Mica，使用不透明底色：{error}");
            "none"
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn apply_material(_window: &WebviewWindow, planned: &'static str) -> &'static str {
    planned
}

/// 关闭 WebView2 的浏览器快捷键（Ctrl+F 查找、Ctrl+P 打印、F5 刷新、缩放、开发者工具等）。
/// 按键仍会传给页面，Ctrl+F 由前端用作聚焦搜索框；剪切、复制、粘贴、全选等编辑键不受影响。
/// 前端另有一层拦截（src/lib/native-behavior.ts），这里是第一道；失败时只记日志。
#[cfg(target_os = "windows")]
fn disable_browser_accelerators(window: &WebviewWindow) {
    let result = window.with_webview(|webview| {
        use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3;
        use windows_core::Interface;
        // SAFETY: 回调在 WebView2 所在的主线程上执行，controller 在 webview 存活期间有效。
        let outcome = unsafe {
            webview
                .controller()
                .CoreWebView2()
                .and_then(|core| core.Settings())
                .and_then(|settings| settings.cast::<ICoreWebView2Settings3>())
                .and_then(|settings| settings.SetAreBrowserAcceleratorKeysEnabled(false))
        };
        if let Err(error) = outcome {
            eprintln!("录播宝无法关闭 WebView2 浏览器快捷键：{error}");
        }
    });
    if let Err(error) = result {
        eprintln!("录播宝无法关闭 WebView2 浏览器快捷键：{error}");
    }
}

#[cfg(not(target_os = "windows"))]
fn disable_browser_accelerators(_window: &WebviewWindow) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mica_starts_with_windows_11() {
        assert!(!supports_mica(0));
        assert!(!supports_mica(19045)); // Windows 10 22H2
        assert!(supports_mica(22000)); // Windows 11 21H2
        assert!(supports_mica(26100));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reads_the_windows_build_number() {
        // 支持的系统至少是 Windows 10 1809（build 17763）。
        assert!(windows_build() >= 17763);
    }

    #[test]
    fn shell_script_hands_platform_and_material_to_the_page() {
        let script = shell_script("none");
        assert!(script.starts_with("window.__LUBOBAO_SHELL__ = {"));
        assert!(script.contains(&format!("platform: \"{PLATFORM}\"")));
        assert!(script.contains("material: \"none\""));
    }
}
