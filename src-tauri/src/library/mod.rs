//! 录像库：按目录浏览录像根目录（设置项 `live_save_path`）。
//!
//! 前端只传相对于录像根目录的子路径（`/` 或 `\` 分隔），这里负责：
//! - 拒绝 `..`、绝对路径、盘符和带 `:` 的片段；
//! - 规范化后确认目标仍在根目录内，跳出根目录的符号链接或 junction 一律拒绝；
//! - 列出子目录和录像文件，统计占用和磁盘剩余空间。
//!
//! 行为参考 StreamCap `app/ui/views/storage_view.py`：目录在前，空目录和目录不存在时给出说明。
//! 有意差异：StreamCap 列出全部文件并按名称排序；录播宝只列出录像文件
//! （扩展名取自 StreamCap `app/utils/utils.py` 的 `is_valid_video_file`），文件按修改时间倒序，
//! 同一次录制的分段文件按序号排在一起。
//!
//! 这里的函数都是同步阻塞 IO，命令层用 `spawn_blocking` 调用。
//!
//! 分层约定：本文件只依赖标准库、serde 和 fs4，不出现 `tauri::`，以便以后不依赖 Tauri 的
//! 命令行程序直接复用（根目录由调用方传入）。Tauri 命令包装、读取设置，以及用系统程序打开、
//! 在文件夹中显示这类桌面功能，都放在胶水层 [`commands`]。

pub mod commands;
pub mod usage;

#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

/// 录像文件扩展名（小写，不含点），与 StreamCap 一致。
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "mkv", "nut", "ts", "flv", "mp3", "m4a", "wav", "aac", "wma",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryErrorKind {
    /// 录像根目录不存在。
    RootMissing,
    /// 录像根目录存在，但不是目录。
    RootNotDirectory,
    /// 没有权限读取。
    PermissionDenied,
    /// 子路径不存在（可能已被删除）。
    NotFound,
    /// 子路径非法：含 `..`、绝对路径，或跳出了录像根目录。
    InvalidPath,
    /// 目标类型不对，例如要打开的是目录或不是录像文件。
    InvalidTarget,
    /// 系统播放器或文件管理器调用失败。
    OpenFailed,
    Io,
}

/// 命令失败时返回给前端的结构化错误：`{ kind, message, root }`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LibraryError {
    pub kind: LibraryErrorKind,
    pub message: String,
    /// 录像根目录的绝对路径，便于前端提示用户去设置中修改。
    pub root: String,
}

impl LibraryError {
    fn new(kind: LibraryErrorKind, message: impl Into<String>, root: &Path) -> Self {
        LibraryError {
            kind,
            message: message.into(),
            root: display_path(root),
        }
    }
}

impl std::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for LibraryError {}

pub type LibraryResult<T> = Result<T, LibraryError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryKind {
    Dir,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    pub name: String,
    /// 相对于录像根目录的路径，`/` 分隔；传回给其他命令使用。
    pub path: String,
    pub kind: EntryKind,
    /// 文件大小（字节）；目录为 None。
    pub size: Option<u64>,
    /// 修改时间，Unix 毫秒。
    pub modified_at: Option<i64>,
    /// 目录下的子目录和录像文件数；读不了时为 None。文件为 None。
    pub child_count: Option<u64>,
    /// 分段序号（文件名末尾 `_000` 这类数字）；不是分段文件时为 None。
    pub segment: Option<u32>,
    /// 同一次录制的分组键：去掉分段序号后的文件名。
    pub group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryListing {
    /// 录像根目录的绝对路径。
    pub root: String,
    /// 当前目录相对于根目录的路径，`/` 分隔；根目录为空字符串。
    pub path: String,
    /// 目录在前（按名称），文件按修改时间倒序，分段文件按序号。
    pub entries: Vec<LibraryEntry>,
}

/// 根目录所在磁盘的空间。只调用一次系统接口，很快，给状态栏定时刷新用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSpace {
    pub root: String,
    pub root_exists: bool,
    /// 剩余空间；根目录不存在时取最近的已存在上级目录。查询失败时为 None。
    pub available_bytes: Option<u64>,
    /// 磁盘总容量，同上。
    pub total_bytes: Option<u64>,
}

/// 根目录的占用。需要递归遍历，可能很慢，桌面端由 [`usage::UsageCache`] 缓存，同一时间只遍历一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryUsage {
    /// 根目录下全部文件的总大小（不跟随符号链接）。
    pub used_bytes: u64,
    /// 录像文件个数。
    pub file_count: u64,
}

/// 根目录的绝对路径（相对路径按当前目录解析，与录制引擎一致）。
pub fn absolute_root(root: &Path) -> PathBuf {
    std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf())
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// 把前端传来的子路径拆成片段。空串、`.` 和重复分隔符忽略；拒绝 `..`、绝对路径和含 `:` 的片段。
pub fn split_relative(relative: &str) -> Result<Vec<String>, String> {
    if relative.starts_with('/') || relative.starts_with('\\') {
        return Err("路径必须相对于录像目录".into());
    }
    let path = Path::new(relative);
    if path.is_absolute() || path.has_root() {
        return Err("路径必须相对于录像目录".into());
    }
    let mut parts = Vec::new();
    for part in relative.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err("路径不能包含 ..".into()),
            _ if part.contains(':') || part.contains('\0') => {
                return Err(format!("路径片段无效：{part}"));
            }
            _ => {
                // 再用标准库确认它只是一个普通片段（排除前缀等平台特有的写法）。
                let mut components = Path::new(part).components();
                match (components.next(), components.next()) {
                    (Some(Component::Normal(_)), None) => parts.push(part.to_string()),
                    _ => return Err(format!("路径片段无效：{part}")),
                }
            }
        }
    }
    Ok(parts)
}

/// 解析出的目标。
#[derive(Debug, Clone)]
pub struct Resolved {
    /// 规范化后的真实路径，能去掉时去掉 `\\?\` 前缀；用于读取、显示和交给系统程序。
    /// 与越界检查是同一个目标，不会检查一个文件却打开另一个。
    pub path: PathBuf,
    /// 规范化后的真实路径，用于越界判断。
    pub canonical: PathBuf,
    /// 规范化的相对路径，`/` 分隔。
    pub relative: String,
}

struct Root {
    path: PathBuf,
    canonical: PathBuf,
}

fn open_root(root: &Path) -> LibraryResult<Root> {
    let path = absolute_root(root);
    let metadata = fs::metadata(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => LibraryError::new(
            LibraryErrorKind::RootMissing,
            format!("录像目录不存在：{}", path.display()),
            &path,
        ),
        io::ErrorKind::PermissionDenied => LibraryError::new(
            LibraryErrorKind::PermissionDenied,
            format!("没有权限访问录像目录：{}", path.display()),
            &path,
        ),
        _ => LibraryError::new(
            LibraryErrorKind::Io,
            format!("无法访问录像目录 {}：{error}", path.display()),
            &path,
        ),
    })?;
    if !metadata.is_dir() {
        return Err(LibraryError::new(
            LibraryErrorKind::RootNotDirectory,
            format!("录像目录不是文件夹：{}", path.display()),
            &path,
        ));
    }
    let canonical = fs::canonicalize(&path).map_err(|error| io_error(&error, &path, &path))?;
    Ok(Root { path, canonical })
}

fn io_error(error: &io::Error, target: &Path, root: &Path) -> LibraryError {
    match error.kind() {
        io::ErrorKind::NotFound => LibraryError::new(
            LibraryErrorKind::NotFound,
            format!("找不到 {}，可能已被移动或删除", target.display()),
            root,
        ),
        io::ErrorKind::PermissionDenied => LibraryError::new(
            LibraryErrorKind::PermissionDenied,
            format!("没有权限访问 {}", target.display()),
            root,
        ),
        _ => LibraryError::new(
            LibraryErrorKind::Io,
            format!("无法访问 {}：{error}", target.display()),
            root,
        ),
    }
}

fn resolve_in(root: &Root, relative: &str) -> LibraryResult<Resolved> {
    let parts = split_relative(relative)
        .map_err(|message| LibraryError::new(LibraryErrorKind::InvalidPath, message, &root.path))?;
    let mut path = root.path.clone();
    for part in &parts {
        path.push(part);
    }
    let canonical = fs::canonicalize(&path).map_err(|error| io_error(&error, &path, &root.path))?;
    if !canonical.starts_with(&root.canonical) {
        return Err(LibraryError::new(
            LibraryErrorKind::InvalidPath,
            format!("{} 指向录像目录之外", path.display()),
            &root.path,
        ));
    }
    Ok(Resolved {
        path: dunce::simplified(&canonical).to_path_buf(),
        canonical,
        relative: parts.join("/"),
    })
}

/// 解析录像根目录下的子路径，确认它存在且没有跳出根目录。
pub fn resolve(root: &Path, relative: &str) -> LibraryResult<Resolved> {
    resolve_in(&open_root(root)?, relative)
}

/// 能列出、之后还能再定位的名称：不是隐藏项，也不含 `\` 和 `:`。
/// Linux、macOS 的文件名可以含这两个字符，但 `split_relative` 会把它们当分隔符或拒绝。
pub fn is_listable_name(name: &str) -> bool {
    !name.starts_with('.') && !name.contains(['\\', ':', '\0'])
}

pub fn is_media_file(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            let ext = ext.to_ascii_lowercase();
            MEDIA_EXTENSIONS.contains(&ext.as_str())
        })
}

/// 拆出分段序号：`主播_2026-11-22_20-14-03_001.ts` → (`主播_2026-11-22_20-14-03.ts`, Some(1))。
/// 序号至少三位数字，与录制引擎的 `_%03d` 一致。
pub fn split_segment(name: &str) -> (String, Option<u32>) {
    let path = Path::new(name);
    let (Some(stem), ext) = (path.file_stem().and_then(|s| s.to_str()), path.extension()) else {
        return (name.to_string(), None);
    };
    let with_ext = |base: &str| match ext.and_then(|e| e.to_str()) {
        Some(ext) => format!("{base}.{ext}"),
        None => base.to_string(),
    };
    if let Some((base, digits)) = stem.rsplit_once('_')
        && !base.is_empty()
        && digits.len() >= 3
        && digits.bytes().all(|b| b.is_ascii_digit())
        && let Ok(index) = digits.parse::<u32>()
    {
        return (with_ext(base), Some(index));
    }
    (name.to_string(), None)
}

fn modified_ms(metadata: &fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

/// 目录项的探测结果。
enum Probe {
    Dir(fs::Metadata),
    Media(fs::Metadata),
}

/// 先用 `file_type()`（Windows 上来自目录枚举，不额外打开文件）和文件名过滤，只对目录、
/// 录像文件和链接取元数据。符号链接和 junction 只在指向根目录之内时保留，并按目标判断类型；
/// 链接到文件时，链接名和目标名都必须是录像扩展名。
fn probe_entry(root: &Root, entry: &fs::DirEntry, name: &str) -> Option<Probe> {
    let file_type = entry.file_type().ok()?;
    if file_type.is_symlink() {
        let target = fs::canonicalize(entry.path()).ok()?;
        if !target.starts_with(&root.canonical) {
            return None;
        }
        let metadata = fs::metadata(&target).ok()?;
        if metadata.is_dir() {
            return Some(Probe::Dir(metadata));
        }
        let target_name = target.file_name()?.to_str()?;
        return (metadata.is_file() && is_media_file(name) && is_media_file(target_name))
            .then_some(Probe::Media(metadata));
    }
    if file_type.is_dir() {
        return entry.metadata().ok().map(Probe::Dir);
    }
    if file_type.is_file() && is_media_file(name) {
        return entry.metadata().ok().map(Probe::Media);
    }
    None
}

/// 子目录和录像文件数。普通目录项只看 `file_type()` 和文件名，只有链接才解析目标。
fn count_children(root: &Root, dir: &Path) -> Option<u64> {
    let mut count = 0;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_listable_name(name) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let counted = if file_type.is_symlink() {
            probe_entry(root, &entry, name).is_some()
        } else {
            file_type.is_dir() || (file_type.is_file() && is_media_file(name))
        };
        if counted {
            count += 1;
        }
    }
    Some(count)
}

/// 列出录像根目录下 `relative` 目录的子目录和录像文件。
/// 隐藏项（以 `.` 开头）和含 `\`、`:` 的名称不列出，见 [`is_listable_name`]。
pub fn list_dir(root: &Path, relative: &str) -> LibraryResult<LibraryListing> {
    let root = open_root(root)?;
    let target = resolve_in(&root, relative)?;
    let metadata =
        fs::metadata(&target.path).map_err(|error| io_error(&error, &target.path, &root.path))?;
    if !metadata.is_dir() {
        return Err(LibraryError::new(
            LibraryErrorKind::InvalidTarget,
            format!("{} 不是文件夹", target.path.display()),
            &root.path,
        ));
    }
    let reader =
        fs::read_dir(&target.path).map_err(|error| io_error(&error, &target.path, &root.path))?;

    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in reader.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            // 无法表示为 UTF-8 的文件名无法传回给前端再定位，跳过。
            continue;
        };
        if !is_listable_name(&name) {
            continue;
        }
        let Some(probe) = probe_entry(&root, &entry, &name) else {
            continue;
        };
        let path = if target.relative.is_empty() {
            name.clone()
        } else {
            format!("{}/{name}", target.relative)
        };
        match probe {
            Probe::Dir(metadata) => dirs.push(LibraryEntry {
                child_count: count_children(&root, &entry.path()),
                name,
                path,
                kind: EntryKind::Dir,
                size: None,
                modified_at: modified_ms(&metadata),
                segment: None,
                group: None,
            }),
            Probe::Media(metadata) => {
                let (group, segment) = split_segment(&name);
                files.push(LibraryEntry {
                    name,
                    path,
                    kind: EntryKind::File,
                    size: Some(metadata.len()),
                    modified_at: modified_ms(&metadata),
                    child_count: None,
                    segment,
                    group: Some(group),
                });
            }
        }
    }

    dirs.sort_by(|a, b| compare_names(&a.name, &b.name));
    sort_files(&mut files);
    dirs.extend(files);
    Ok(LibraryListing {
        root: display_path(&root.path),
        path: target.relative,
        entries: dirs,
    })
}

fn compare_names(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// 文件按同次录制分组：组按组内最新修改时间倒序，组内按分段序号升序。
/// 排序键预先算好，比较时不再分配字符串。
pub fn sort_files(files: &mut Vec<LibraryEntry>) {
    fn group_of(file: &LibraryEntry) -> &str {
        file.group.as_deref().unwrap_or(&file.name)
    }
    let mut latest: HashMap<&str, i64> = HashMap::new();
    for file in files.iter() {
        let time = file.modified_at.unwrap_or(i64::MIN);
        latest
            .entry(group_of(file))
            .and_modify(|value| *value = (*value).max(time))
            .or_insert(time);
    }
    let keys: Vec<(i64, String, u32, String)> = files
        .iter()
        .map(|file| {
            let group = group_of(file);
            (
                latest[group],
                group.to_lowercase(),
                file.segment.unwrap_or(0),
                file.name.to_lowercase(),
            )
        })
        .collect();
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|&a, &b| {
        let (ka, kb) = (&keys[a], &keys[b]);
        kb.0.cmp(&ka.0)
            .then_with(|| kb.1.cmp(&ka.1))
            .then_with(|| ka.2.cmp(&kb.2))
            .then_with(|| ka.3.cmp(&kb.3))
            .then_with(|| files[a].name.cmp(&files[b].name))
    });
    let mut taken: Vec<Option<LibraryEntry>> =
        std::mem::take(files).into_iter().map(Some).collect();
    files.extend(order.into_iter().filter_map(|index| taken[index].take()));
}

/// 要打开或定位的文件：必须在根目录内且存在。`media_only` 时只接受录像文件。
pub fn resolve_file(root: &Path, relative: &str, media_only: bool) -> LibraryResult<Resolved> {
    let root = open_root(root)?;
    let target = resolve_in(&root, relative)?;
    if media_only {
        // 对解析后的真实目标做检查：根目录内名为 a.ts、实际指向 payload.exe 的链接会被拒绝。
        let metadata = fs::metadata(&target.canonical)
            .map_err(|error| io_error(&error, &target.path, &root.path))?;
        let requested = relative.rsplit(['/', '\\']).next().unwrap_or_default();
        let real = target
            .canonical
            .file_name()
            .map(|name| name.to_string_lossy());
        if !metadata.is_file()
            || !is_media_file(requested)
            || !real.is_some_and(|name| is_media_file(&name))
        {
            return Err(LibraryError::new(
                LibraryErrorKind::InvalidTarget,
                format!("{} 不是录像文件", target.path.display()),
                &root.path,
            ));
        }
    }
    Ok(target)
}

/// 查询根目录所在磁盘的剩余和总空间。根目录不存在时查询最近的已存在上级目录所在磁盘。
pub fn disk_space(root: &Path) -> DiskSpace {
    let path = absolute_root(root);
    let probe = path.ancestors().find(|dir| dir.is_dir());
    DiskSpace {
        root: display_path(&path),
        root_exists: path.is_dir(),
        available_bytes: probe.and_then(|dir| fs4::available_space(dir).ok()),
        total_bytes: probe.and_then(|dir| fs4::total_space(dir).ok()),
    }
}

/// 递归统计根目录占用；根目录不存在时为 None。可能很慢，桌面端经 [`usage::UsageCache`] 调用。
pub fn usage(root: &Path) -> Option<LibraryUsage> {
    let path = absolute_root(root);
    path.is_dir().then(|| walk_usage(&path))
}

/// 递归累计文件大小，不跟随符号链接和 junction，读不了的目录跳过。
pub fn walk_usage(root: &Path) -> LibraryUsage {
    let mut used = 0u64;
    let mut count = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(reader) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file()
                && let Ok(metadata) = entry.metadata()
            {
                used = used.saturating_add(metadata.len());
                if is_media_file(&entry.file_name().to_string_lossy()) {
                    count += 1;
                }
            }
        }
    }
    LibraryUsage {
        used_bytes: used,
        file_count: count,
    }
}
