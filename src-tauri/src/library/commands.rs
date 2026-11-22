//! 录像库的 Tauri 命令。参数名在前端为 camelCase，例如 `{ path }`。
//!
//! 录像根目录每次从设置 `live_save_path` 读取，所以修改保存目录后立即生效。
//! 文件系统和系统程序调用都放在阻塞线程里执行。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tauri::State;

use super::{LibraryError, LibraryErrorKind, LibraryListing, LibraryResult, LibraryStats};
use crate::tasks::TaskManager;

type Manager<'a> = State<'a, Arc<TaskManager>>;

fn library_root(manager: &TaskManager) -> PathBuf {
    match manager.settings().get("live_save_path") {
        Some(Value::String(path)) if !path.trim().is_empty() => PathBuf::from(path.trim()),
        _ => manager
            .defaults()
            .get("live_save_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_default(),
    }
}

async fn blocking<T, F>(root: PathBuf, job: F) -> LibraryResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Path) -> LibraryResult<T> + Send + 'static,
{
    let fallback = root.clone();
    tauri::async_runtime::spawn_blocking(move || job(&root))
        .await
        .unwrap_or_else(|error| {
            Err(LibraryError::new(
                LibraryErrorKind::Io,
                format!("录像库操作中断：{error}"),
                &fallback,
            ))
        })
}

/// 用系统默认程序打开文件或文件夹（Windows 上为 `ShellExecuteEx`）。
fn shell_open(path: &Path) -> Result<(), String> {
    tauri_plugin_opener::open_path(path, None::<&str>).map_err(|error| error.to_string())
}

/// 在文件管理器中显示并选中。
fn shell_reveal(path: &Path) -> Result<(), String> {
    tauri_plugin_opener::reveal_item_in_dir(path).map_err(|error| error.to_string())
}

fn open_failed(error: impl std::fmt::Display, root: &Path) -> LibraryError {
    LibraryError::new(LibraryErrorKind::OpenFailed, error.to_string(), root)
}

/// 列出录像根目录下 `path` 目录的内容；`path` 缺省为根目录。
#[tauri::command]
pub async fn list_library(
    manager: Manager<'_>,
    path: Option<String>,
) -> LibraryResult<LibraryListing> {
    let relative = path.unwrap_or_default();
    blocking(library_root(&manager), move |root| {
        super::list_dir(root, &relative)
    })
    .await
}

/// 根目录占用与磁盘空间。
#[tauri::command]
pub async fn get_library_stats(manager: Manager<'_>) -> LibraryResult<LibraryStats> {
    blocking(library_root(&manager), |root| Ok(super::stats(root))).await
}

/// 用系统默认播放器打开录像文件。
#[tauri::command]
pub async fn open_library_file(manager: Manager<'_>, path: String) -> LibraryResult<()> {
    blocking(library_root(&manager), move |root| {
        let target = super::resolve_file(root, &path, true)?;
        shell_open(&target.path).map_err(|error| {
            open_failed(format!("无法打开 {}：{error}", target.path.display()), root)
        })
    })
    .await
}

/// 在系统文件管理器中打开文件夹本身；`path` 为空时打开根目录。
#[tauri::command]
pub async fn open_library_folder(manager: Manager<'_>, path: String) -> LibraryResult<()> {
    blocking(library_root(&manager), move |root| {
        let target = super::resolve(root, &path)?;
        if !target.path.is_dir() {
            return Err(LibraryError::new(
                LibraryErrorKind::InvalidTarget,
                format!("{} 不是文件夹", target.path.display()),
                root,
            ));
        }
        shell_open(&target.path).map_err(|error| {
            open_failed(format!("无法打开 {}：{error}", target.path.display()), root)
        })
    })
    .await
}

/// 在系统文件管理器中显示并选中文件或文件夹；`path` 为空时打开根目录。
#[tauri::command]
pub async fn reveal_library_item(manager: Manager<'_>, path: String) -> LibraryResult<()> {
    blocking(library_root(&manager), move |root| {
        let target = super::resolve_file(root, &path, false)?;
        let result = if target.relative.is_empty() {
            shell_open(&target.path)
        } else {
            shell_reveal(&target.path)
        };
        result.map_err(|error| {
            open_failed(
                format!("无法在文件夹中显示 {}：{error}", target.path.display()),
                root,
            )
        })
    })
    .await
}
