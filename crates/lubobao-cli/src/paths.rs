//! 数据目录、实例锁和控制通道的位置。
//!
//! 命令行程序不与桌面端共用数据目录（桌面端用 `com.xiaofany.lubobao`）：
//! - Linux：`$XDG_DATA_HOME/lubobao`，未设置时 `~/.local/share/lubobao`；
//! - macOS：`~/Library/Application Support/lubobao-cli`；
//! - Windows：`%LOCALAPPDATA%\lubobao-cli`。
//!
//! 可用 `--data-dir` 或 `LUBOBAO_DATA_DIR` 覆盖。
//!
//! 控制通道默认是数据目录里的 `control.sock`（Windows 上是按数据目录命名的命名管道）。
//! 数据目录路径太长（Unix socket 路径上限约 100 字节），或者所在文件系统不支持 socket
//! （CIFS、NFS 等网络盘）时，改用 `$XDG_RUNTIME_DIR/lubobao-<哈希>.sock`；也可以用
//! `--control` / `LUBOBAO_CONTROL` 直接指定。

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::GlobalOpts;

/// 实例锁文件：`run` 运行期间持有；`run` 没运行时，修改类命令写数据库期间也持有。
const LOCK_FILE: &str = "run.lock";
#[cfg(unix)]
const SOCKET_FILE: &str = "control.sock";
/// Unix socket 路径的安全长度（Linux 上限 108、macOS 104 字节，含结尾的 0）。
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

#[derive(Debug, Clone)]
pub struct Dirs {
    pub data_dir: PathBuf,
    /// 设置 `live_save_path` 的缺省值。
    pub save_dir: PathBuf,
    /// `--control` / `LUBOBAO_CONTROL` 指定的控制通道。
    pub control: Option<PathBuf>,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// 平台默认的数据目录。`env` 读取环境变量，测试中替换。
fn default_data_dir_from(env: impl Fn(&str) -> Option<PathBuf>) -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        env("LOCALAPPDATA")
            .map(|dir| dir.join("lubobao-cli"))
            .ok_or_else(|| "找不到 %LOCALAPPDATA%，请用 --data-dir 指定数据目录".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        env("HOME")
            .map(|home| home.join("Library/Application Support/lubobao-cli"))
            .ok_or_else(|| "找不到 $HOME，请用 --data-dir 指定数据目录".to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // XDG 规范：相对路径的 XDG_DATA_HOME 无效，忽略。
        env("XDG_DATA_HOME")
            .filter(|dir| dir.is_absolute())
            .or_else(|| env("HOME").map(|home| home.join(".local/share")))
            .map(|dir| dir.join("lubobao"))
            .ok_or_else(|| {
                "找不到 $XDG_DATA_HOME 或 $HOME，请用 --data-dir 指定数据目录".to_string()
            })
    }
}

/// 建目录。Unix 上新建的数据目录只有本用户可访问。
fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// 数据目录的短哈希，用于命名管道和备用 socket 的名字。Windows 上不区分大小写。
fn dir_hash(dir: &Path) -> String {
    let mut text = dir.to_string_lossy().into_owned();
    if cfg!(windows) {
        text = text.to_lowercase();
    }
    let text = text.trim_end_matches(['\\', '/']);
    let digest = Sha256::digest(text.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Dirs {
    pub fn resolve(global: &GlobalOpts) -> Result<Dirs, String> {
        let data_dir = match &global.data_dir {
            Some(dir) => dir.clone(),
            None => default_data_dir_from(env_path)?,
        };
        let data_dir = std::path::absolute(&data_dir)
            .map_err(|error| format!("数据目录无效 {}：{error}", data_dir.display()))?;
        create_private_dir(&data_dir)
            .map_err(|error| format!("无法创建数据目录 {}：{error}", data_dir.display()))?;
        let save_dir = match &global.save_dir {
            Some(dir) => std::path::absolute(dir)
                .map_err(|error| format!("录像目录无效 {}：{error}", dir.display()))?,
            None => data_dir.join("recordings"),
        };
        let control = match &global.control {
            Some(path) => Some(
                std::path::absolute(path)
                    .map_err(|error| format!("控制通道路径无效 {}：{error}", path.display()))?,
            ),
            None => None,
        };
        Ok(Dirs {
            data_dir,
            save_dir,
            control,
        })
    }

    pub fn ffmpeg_root(&self) -> PathBuf {
        self.data_dir.join("components").join("ffmpeg")
    }

    /// 收紧数据目录的权限为 0700（里面有控制通道和 Cookie）。返回是否改动过。
    #[cfg(unix)]
    pub fn secure_data_dir(&self) -> io::Result<bool> {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&self.data_dir)?.permissions().mode();
        if mode & 0o077 == 0 {
            return Ok(false);
        }
        std::fs::set_permissions(&self.data_dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(true)
    }

    /// 控制通道 socket 的候选路径，按顺序尝试：指定的；否则数据目录里的（路径不太长时）、
    /// `$XDG_RUNTIME_DIR` 或临时目录里的备用路径。
    #[cfg(unix)]
    pub fn socket_candidates(&self) -> Vec<PathBuf> {
        if let Some(path) = &self.control {
            return vec![path.clone()];
        }
        let mut candidates = Vec::new();
        let primary = self.data_dir.join(SOCKET_FILE);
        if primary.as_os_str().len() <= MAX_SOCKET_PATH {
            candidates.push(primary);
        }
        let name = format!("lubobao-{}.sock", dir_hash(&self.data_dir));
        let runtime = env_path("XDG_RUNTIME_DIR").filter(|dir| dir.is_absolute());
        let fallback = match runtime {
            Some(dir) => dir.join(name),
            // 没有 XDG_RUNTIME_DIR（例如 Docker、cron）：放在临时目录，文件名带上 uid。
            // SAFETY: getuid 总是成功。
            None => std::env::temp_dir().join(format!(
                "lubobao-{}-{}.sock",
                unsafe { libc::getuid() },
                dir_hash(&self.data_dir)
            )),
        };
        candidates.push(fallback);
        candidates
    }

    /// 按数据目录区分的命名管道名，同一台机器上可以同时运行多个数据目录。
    #[cfg(windows)]
    pub fn pipe_name(&self) -> String {
        match &self.control {
            Some(path) => {
                let text = path.to_string_lossy();
                match text.find(r"\pipe\") {
                    Some(_) => text.into_owned(),
                    None => format!(
                        r"\\.\pipe\{}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                }
            }
            None => format!(r"\\.\pipe\lubobao-cli-{}", dir_hash(&self.data_dir)),
        }
    }

    /// 控制通道的地址，用于提示。
    pub fn endpoint(&self) -> String {
        #[cfg(unix)]
        {
            self.socket_candidates()
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" 或 ")
        }
        #[cfg(windows)]
        {
            self.pipe_name()
        }
    }

    /// 尝试取得实例锁，不等待。被别的进程持有时返回 `Ok(None)`。
    pub fn try_lock(&self) -> Result<Option<File>, String> {
        let path = self.data_dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| format!("无法打开实例锁 {}：{error}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => {
                Err(format!("无法锁定数据目录 {}：{error}", path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs_in(dir: &Path) -> Dirs {
        Dirs {
            data_dir: dir.to_path_buf(),
            save_dir: dir.join("recordings"),
            control: None,
        }
    }

    #[test]
    fn lock_is_exclusive_until_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(temp.path());
        let first = dirs.try_lock().unwrap();
        assert!(first.is_some());
        assert!(dirs.try_lock().unwrap().is_none(), "second lock must fail");
        drop(first);
        assert!(dirs.try_lock().unwrap().is_some());
    }

    #[test]
    fn directory_hash_is_stable_and_distinct() {
        let a = dir_hash(Path::new("/srv/lubobao"));
        assert_eq!(a.len(), 16);
        assert_eq!(a, dir_hash(Path::new("/srv/lubobao/")));
        assert_ne!(a, dir_hash(Path::new("/srv/other")));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_default_follows_xdg_data_home() {
        let env = |xdg: Option<&'static str>| {
            move |name: &str| match name {
                "XDG_DATA_HOME" => xdg.map(PathBuf::from),
                "HOME" => Some(PathBuf::from("/home/u")),
                _ => None,
            }
        };
        assert_eq!(
            default_data_dir_from(env(Some("/data/xdg"))).unwrap(),
            PathBuf::from("/data/xdg/lubobao")
        );
        assert_eq!(
            default_data_dir_from(env(None)).unwrap(),
            PathBuf::from("/home/u/.local/share/lubobao")
        );
        // 相对路径的 XDG_DATA_HOME 按规范忽略。
        assert_eq!(
            default_data_dir_from(env(Some("relative"))).unwrap(),
            PathBuf::from("/home/u/.local/share/lubobao")
        );
        assert!(default_data_dir_from(|_: &str| None).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_is_under_local_app_data() {
        let env = |name: &str| {
            (name == "LOCALAPPDATA").then(|| PathBuf::from(r"C:\Users\u\AppData\Local"))
        };
        assert_eq!(
            default_data_dir_from(env).unwrap(),
            PathBuf::from(r"C:\Users\u\AppData\Local\lubobao-cli")
        );
        let dirs = dirs_in(Path::new(r"C:\Data\LuBoBao"));
        assert_eq!(
            dirs.pipe_name(),
            dirs_in(Path::new(r"c:\data\lubobao\")).pipe_name()
        );
    }

    #[cfg(unix)]
    #[test]
    fn long_data_dir_uses_fallback_socket_only() {
        let long = PathBuf::from(format!("/{}", "x".repeat(120)));
        let candidates = dirs_in(&long).socket_candidates();
        assert_eq!(candidates.len(), 1);
        assert!(
            candidates[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("lubobao-")
        );
        let short = dirs_in(Path::new("/srv/lubobao")).socket_candidates();
        assert_eq!(short[0], PathBuf::from("/srv/lubobao/control.sock"));
        assert_eq!(short.len(), 2);
    }
}
