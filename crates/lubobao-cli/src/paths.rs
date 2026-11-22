//! 数据目录、实例锁和控制通道的位置。
//!
//! 命令行程序不与桌面端共用数据目录（桌面端用 `com.xiaofany.lubobao`）：
//! - Linux：`$XDG_DATA_HOME/lubobao`，未设置时 `~/.local/share/lubobao`；
//! - macOS：`~/Library/Application Support/lubobao-cli`；
//! - Windows：`%LOCALAPPDATA%\lubobao-cli`。
//!
//! 可用 `--data-dir` 或 `LUBOBAO_DATA_DIR` 覆盖。

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use crate::GlobalOpts;

/// 实例锁文件：`run` 运行期间持有；`run` 没运行时，修改类命令操作数据库期间也持有。
const LOCK_FILE: &str = "run.lock";
#[cfg(unix)]
const SOCKET_FILE: &str = "control.sock";

#[derive(Debug, Clone)]
pub struct Dirs {
    pub data_dir: PathBuf,
    /// 设置 `live_save_path` 的缺省值。
    pub save_dir: PathBuf,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn default_data_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        env_path("LOCALAPPDATA")
            .map(|dir| dir.join("lubobao-cli"))
            .ok_or_else(|| "找不到 %LOCALAPPDATA%，请用 --data-dir 指定数据目录".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        env_path("HOME")
            .map(|home| home.join("Library/Application Support/lubobao-cli"))
            .ok_or_else(|| "找不到 $HOME，请用 --data-dir 指定数据目录".to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        env_path("XDG_DATA_HOME")
            .filter(|dir| dir.is_absolute())
            .or_else(|| env_path("HOME").map(|home| home.join(".local/share")))
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

impl Dirs {
    pub fn resolve(global: &GlobalOpts) -> Result<Dirs, String> {
        let data_dir = match &global.data_dir {
            Some(dir) => dir.clone(),
            None => default_data_dir()?,
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
        Ok(Dirs { data_dir, save_dir })
    }

    pub fn ffmpeg_root(&self) -> PathBuf {
        self.data_dir.join("components").join("ffmpeg")
    }

    #[cfg(unix)]
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join(SOCKET_FILE)
    }

    /// 按数据目录区分的命名管道名，同一台机器上可以同时运行多个数据目录。
    #[cfg(windows)]
    pub fn pipe_name(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.data_dir
            .to_string_lossy()
            .to_lowercase()
            .trim_end_matches(['\\', '/'])
            .hash(&mut hasher);
        format!(r"\\.\pipe\lubobao-cli-{:016x}", hasher.finish())
    }

    /// 控制通道的地址，用于提示。
    pub fn endpoint(&self) -> String {
        #[cfg(unix)]
        {
            self.socket_path().display().to_string()
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
