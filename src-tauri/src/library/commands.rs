//! 录像库的 Tauri 命令（胶水层）。参数名在前端为 camelCase，例如 `{ path }`。
//!
//! 录像根目录每次从设置 `live_save_path` 读取，所以修改保存目录后立即生效。
//! 读取设置（SQLite）、文件系统和系统程序调用都放在阻塞线程里执行。

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde_json::Value;
use tauri::State;

use super::usage::{UsageCache, UsageSnapshot};
use super::{DiskSpace, LibraryError, LibraryErrorKind, LibraryListing, LibraryResult};
use crate::tasks::TaskManager;

type Manager<'a> = State<'a, Arc<TaskManager>>;

/// 占用统计的进程级缓存，保证同一时间只有一次遍历。
static USAGE: LazyLock<Arc<UsageCache>> = LazyLock::new(|| Arc::new(UsageCache::default()));

/// 一次占用请求最多等待遍历的时间；超过后返回上次结果并标明正在统计。
const USAGE_WAIT: Duration = Duration::from_secs(2);

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

/// 在阻塞线程里读取根目录并执行 `job`。
async fn blocking<T, F>(manager: &Arc<TaskManager>, job: F) -> LibraryResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Path) -> LibraryResult<T> + Send + 'static,
{
    let manager = Arc::clone(manager);
    tauri::async_runtime::spawn_blocking(move || job(&library_root(&manager)))
        .await
        .unwrap_or_else(|error| {
            Err(LibraryError::new(
                LibraryErrorKind::Io,
                format!("录像库操作中断：{error}"),
                Path::new(""),
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
    blocking(&manager, move |root| super::list_dir(root, &relative)).await
}

/// 根目录所在磁盘的剩余和总空间。很快，状态栏定时调用。
#[tauri::command]
pub async fn get_disk_space(manager: Manager<'_>) -> LibraryResult<DiskSpace> {
    blocking(&manager, |root| Ok(super::disk_space(root))).await
}

/// 根目录占用（缓存，同一时间只遍历一次）。`refresh` 为 true 时忽略缓存有效期。
#[tauri::command]
pub async fn get_library_usage(
    manager: Manager<'_>,
    refresh: Option<bool>,
) -> LibraryResult<UsageSnapshot> {
    let refresh = refresh.unwrap_or(false);
    blocking(&manager, move |root| {
        Ok(USAGE.request(root, refresh, USAGE_WAIT))
    })
    .await
}

/// 用系统默认播放器打开录像文件。打开的是越界检查时解析出的同一个真实路径。
#[tauri::command]
pub async fn open_library_file(manager: Manager<'_>, path: String) -> LibraryResult<()> {
    blocking(&manager, move |root| {
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
    blocking(&manager, move |root| {
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
    blocking(&manager, move |root| {
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
