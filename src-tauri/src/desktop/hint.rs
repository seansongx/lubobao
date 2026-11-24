//! 第一次关窗隐藏到托盘时的提示。是否提示过记在应用数据目录的 `desktop.json`。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tauri::Manager;

use super::notify::{self, Notice, NotifySettings};
use crate::tasks::TaskManager;

const STATE_FILE: &str = "desktop.json";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DesktopFlags {
    tray_hint_shown: bool,
}

/// 本次运行已经处理过（提示过或读到已提示），避免每次关窗都读文件。
static HANDLED: AtomicBool = AtomicBool::new(false);

fn state_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|dir| dir.join(STATE_FILE))
}

fn read_flags(path: &PathBuf) -> DesktopFlags {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 提示「录播宝仍在后台运行」，整个安装只提示一次。只在隐藏到托盘时调用，受系统通知总开关
/// 控制；总开关关闭时不算提示过，打开后第一次隐藏再提示。
pub fn show_once(app: &tauri::AppHandle) {
    let enabled = app
        .try_state::<std::sync::Arc<TaskManager>>()
        .is_some_and(|tasks| NotifySettings::from_settings(&tasks.settings()).system);
    if !enabled || HANDLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(path) = state_path(app) else {
        return;
    };
    let mut flags = read_flags(&path);
    if flags.tray_hint_shown {
        return;
    }
    notify::show(
        app,
        &Notice {
            title: "录播宝".into(),
            body: "录播宝仍在后台运行，可从托盘退出".into(),
        },
    );
    flags.tray_hint_shown = true;
    let written = serde_json::to_vec_pretty(&flags)
        .map_err(|error| error.to_string())
        .and_then(|bytes| fs::write(&path, bytes).map_err(|error| error.to_string()));
    if let Err(error) = written {
        log::warn!("录播宝无法保存 {}：{error}", path.display());
    }
}
