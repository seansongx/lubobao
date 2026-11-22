//! FFmpeg 组件管理：启动时检查本地版本，缺失时从 `@seankit/ffmpeg` 下载、校验并原子安装。
//!
//! 安装位置：`<app_local_data_dir>/components/ffmpeg/<ffmpegVersion>-<packageVersion>/ffmpeg(.exe)`，
//! 下载和解压的临时文件放在同一目录下的 `.tmp/`，保证最后一步改名在同一块磁盘上。
//! 安装期间持有 `.tmp/lock` 的系统文件锁，同时打开多个录播宝时只有一个进程在下载和写入。
//!
//! 本模块不依赖 Tauri，命令行程序直接使用 [`FfmpegManager`]；Tauri 的初始化和命令见 `src-tauri/src/ffmpeg/tauri_glue.rs`。

pub mod download;
mod error;
mod install;
pub mod manifest;
mod status;
#[cfg(test)]
mod tests;

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use download::DownloadOptions;
pub use error::InstallError;
pub use install::{InstalledVersion, run_version};
pub use manifest::{Component, Manifest, PlatformPackage};
pub use status::FfmpegStatus;

/// 状态变化时发给前端的事件名。
pub const STATUS_EVENT: &str = "ffmpeg-status";

const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const VERIFIED_RECORD: &str = "verified.json";
const LOCK_FILE: &str = "lock";

/// 试运行安装前的可执行文件，返回 `-version` 的第一行。测试中可替换。
pub type Probe = Arc<dyn Fn(&Path) -> Result<String, String> + Send + Sync>;
/// 状态变化的回调。
pub type Listener = Box<dyn Fn(&FfmpegStatus) + Send + Sync>;

pub struct ManagerConfig {
    /// 组件根目录，例如 `<app_local_data_dir>/components/ffmpeg`。
    pub root: PathBuf,
    pub component: Result<Component, String>,
    pub download: DownloadOptions,
    pub probe: Probe,
}

impl ManagerConfig {
    /// 使用嵌入的固定清单和当前编译目标。
    pub fn for_current_platform(root: PathBuf) -> Self {
        Self {
            root,
            component: Manifest::pinned().select_current(),
            download: DownloadOptions::default(),
            probe: Arc::new(run_version),
        }
    }
}

pub struct FfmpegManager {
    root: PathBuf,
    component: Result<Component, String>,
    download: DownloadOptions,
    probe: Probe,
    client: reqwest::Client,
    status: Mutex<FfmpegStatus>,
    /// 当前可以使用的版本。下载新版本期间仍指向旧版本。
    usable: Mutex<Option<InstalledVersion>>,
    install_lock: tokio::sync::Mutex<()>,
    listener: Listener,
}

impl FfmpegManager {
    pub fn new(config: ManagerConfig, listener: Listener) -> Self {
        Self {
            client: download::build_client(&config.download),
            root: config.root,
            component: config.component,
            download: config.download,
            probe: config.probe,
            status: Mutex::new(FfmpegStatus::Checking),
            usable: Mutex::new(None),
            install_lock: tokio::sync::Mutex::new(()),
            listener,
        }
    }

    pub fn status(&self) -> FfmpegStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 可用版本的绝对路径。重新检查或下载新版本期间仍返回旧版本。
    pub fn ffmpeg_path(&self) -> Option<PathBuf> {
        self.usable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|version| version.path.clone())
    }

    pub fn install_dir(&self, component: &Component) -> PathBuf {
        self.root.join(component.version_dir())
    }

    pub fn binary_path(&self, component: &Component) -> PathBuf {
        self.install_dir(component)
            .join(component.package.binary_file_name())
    }

    fn temp_dir(&self) -> PathBuf {
        self.root.join(".tmp")
    }

    fn part_path(&self, component: &Component) -> PathBuf {
        self.temp_dir()
            .join(format!("{}.part", component.package.file))
    }

    fn set(&self, status: FfmpegStatus) {
        {
            let mut current = self.status.lock().unwrap_or_else(|e| e.into_inner());
            if *current == status {
                return;
            }
            *current = status.clone();
        }
        (self.listener)(&status);
    }

    /// 已可用时重新检查不切换到“检查中”，检查期间保持 `Ready`。
    fn set_progress(&self, status: FfmpegStatus) {
        if status == FfmpegStatus::Checking && self.status().is_ready() {
            return;
        }
        self.set(status);
    }

    fn set_usable(&self, version: Option<InstalledVersion>) {
        *self.usable.lock().unwrap_or_else(|e| e.into_inner()) = version;
    }

    /// 检查本地版本，必要时下载安装。同一进程内同一时间只运行一个；已在运行时直接返回当前状态。
    /// 失败后再次调用即为重试，已下载的部分会续传。
    pub async fn ensure(&self) -> FfmpegStatus {
        self.run_exclusive(false).await
    }

    /// 忽略校验缓存，重新计算当前版本的哈希。期间状态为“校验中”；通过后为 `Ready`，
    /// 不通过时与 [`Self::ensure`] 一样重新下载安装，安装失败时退回仍可用的旧版本。
    /// 已有检查或安装在运行时直接返回当前状态。
    pub async fn reverify(&self) -> FfmpegStatus {
        self.run_exclusive(true).await
    }

    async fn run_exclusive(&self, force: bool) -> FfmpegStatus {
        let Ok(_guard) = self.install_lock.try_lock() else {
            return self.status();
        };
        let status = self.run(force).await;
        self.set(status.clone());
        status
    }

    async fn run(&self, force: bool) -> FfmpegStatus {
        let component = match &self.component {
            Ok(component) => component.clone(),
            Err(reason) => {
                return FfmpegStatus::Failed {
                    reason: reason.clone(),
                    retryable: false,
                };
            }
        };
        if force {
            self.set(FfmpegStatus::Verifying);
        } else {
            self.set_progress(FfmpegStatus::Checking);
        }
        match self.check_current(&component, force).await {
            Ok(true) => return self.ready_current(&component),
            // 强制校验时读不出文件（被占用、权限等）不能说明文件坏了，保留当前版本可用，
            // 不因此掉到旧版本或重新下载。普通检查沿用原来的处理：当作不可用。
            Err(_) if force => return self.ready_current(&component),
            Ok(false) | Err(_) => {}
        }

        // 当前版本不可用：没有旧版本时如实显示未安装；有旧版本时先切到旧版本，下载期间也能用。
        let fallback = self.find_fallback(&component).await;
        self.set_usable(fallback.clone());
        match &fallback {
            None => self.set(FfmpegStatus::NotInstalled),
            Some(old) => self.set(FfmpegStatus::Ready {
                path: old.path.clone(),
                ffmpeg_version: old.ffmpeg_version.clone(),
                package_version: old.package_version.clone(),
                outdated: true,
                install_error: None,
            }),
        }

        let result = match self.lock_install_dir().await {
            Ok(_lock) => {
                // 等锁期间另一个进程可能已经装好了。
                if self.check_current(&component, false).await == Ok(true) {
                    return self.ready_current(&component);
                }
                self.install(&component).await
            }
            Err(err) => Err(err),
        };

        match (result, fallback) {
            (Ok(()), _) => self.ready_current(&component),
            (Err(err), Some(old)) => FfmpegStatus::Ready {
                path: old.path,
                ffmpeg_version: old.ffmpeg_version,
                package_version: old.package_version,
                outdated: true,
                install_error: Some(err.to_string()),
            },
            (Err(err), None) => FfmpegStatus::Failed {
                reason: err.to_string(),
                retryable: true,
            },
        }
    }

    fn ready_current(&self, component: &Component) -> FfmpegStatus {
        let version = InstalledVersion {
            path: self.binary_path(component),
            ffmpeg_version: component.ffmpeg_version.clone(),
            package_version: component.package_version.clone(),
        };
        self.set_usable(Some(version.clone()));
        FfmpegStatus::Ready {
            path: version.path,
            ffmpeg_version: version.ffmpeg_version,
            package_version: version.package_version,
            outdated: false,
            install_error: None,
        }
    }

    /// 校验当前版本。`force` 时忽略校验记录重新计算哈希；文件读不出来时返回 `Err`。
    async fn check_current(&self, component: &Component, force: bool) -> Result<bool, String> {
        let binary = self.binary_path(component);
        let record = self.install_dir(component).join(VERIFIED_RECORD);
        let component = component.clone();
        blocking(move || {
            install::check_installed(
                &binary,
                &record,
                &component.package,
                install::Versions {
                    ffmpeg: &component.ffmpeg_version,
                    package: &component.package_version,
                },
                force,
            )
        })
        .await
        .unwrap_or(Ok(false))
    }

    async fn find_fallback(&self, component: &Component) -> Option<InstalledVersion> {
        let root = self.root.clone();
        let skip = component.version_dir();
        let binary = component.package.binary_file_name();
        blocking(move || install::find_fallback(&root, &skip, &binary, VERIFIED_RECORD))
            .await
            .ok()
            .flatten()
    }

    /// 取得 `.tmp/lock` 的系统文件锁。另一个进程持有时显示“等待”，直到它释放。
    async fn lock_install_dir(&self) -> Result<File, InstallError> {
        let temp = self.temp_dir();
        tokio::fs::create_dir_all(&temp)
            .await
            .map_err(|err| InstallError::io("创建临时目录", err))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(temp.join(LOCK_FILE))
            .map_err(|err| InstallError::io("打开安装锁", err))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => {
                self.set_progress(FfmpegStatus::Waiting);
                blocking(move || file.lock().map(|()| file))
                    .await?
                    .map_err(|err| InstallError::io("等待安装锁", err))
            }
            Err(TryLockError::Error(err)) => Err(InstallError::io("锁定安装目录", err)),
        }
    }

    /// 在持有文件锁时调用。
    async fn install(&self, component: &Component) -> Result<(), InstallError> {
        let temp = self.temp_dir();
        remove_stale_stages(&temp).await;
        let part = self.part_path(component);
        download::remove_foreign_parts(&temp, &part).await;

        let package = &component.package;
        let mut last_emit: Option<Instant> = None;
        let mut last_source = String::new();
        let mut on_progress = |downloaded: u64, total: u64, source: &str| {
            let now = Instant::now();
            let due = last_emit.is_none_or(|at| now.duration_since(at) >= PROGRESS_INTERVAL);
            if due || downloaded == total || source != last_source {
                last_emit = Some(now);
                last_source = source.to_string();
                self.set_progress(FfmpegStatus::Downloading {
                    downloaded,
                    total,
                    source: source.to_string(),
                });
            }
        };

        let mut order = package.urls.clone();
        let mut switched = false;
        loop {
            let used = download::download(
                &self.client,
                &self.download,
                &order,
                &part,
                package.size,
                &mut on_progress,
            )
            .await?;

            self.set_progress(FfmpegStatus::Verifying);
            let stage = temp.join(format!("stage-{}", unique_suffix()));
            let result = self.verify_and_install(component, &part, &stage).await;
            let _ = tokio::fs::remove_dir_all(&stage).await;
            // 本地读写失败时保留已下载的文件，其余情况（包括成功）删除。
            if !matches!(result, Err(InstallError::Io { .. })) {
                download::discard_part(&part).await;
            }

            // 内容不一致时换到下一个地址从头再下载一次。
            if matches!(result, Err(InstallError::TarballMismatch)) && !switched && order.len() > 1
            {
                switched = true;
                let index = order.iter().position(|url| *url == used).unwrap_or(0);
                order.rotate_left(index + 1);
                continue;
            }
            return result;
        }
    }

    async fn verify_and_install(
        &self,
        component: &Component,
        part: &Path,
        stage: &Path,
    ) -> Result<(), InstallError> {
        let package = component.package.clone();
        let tarball = part.to_path_buf();
        blocking(move || install::verify_tarball(&tarball, &package)).await??;

        tokio::fs::create_dir_all(stage)
            .await
            .map_err(|err| InstallError::io("创建临时目录", err))?;
        let staged = stage.join(component.package.binary_file_name());
        let package = component.package.clone();
        let tarball = part.to_path_buf();
        let dest = staged.clone();
        blocking(move || install::extract_binary(&tarball, &package, &dest)).await??;

        let probe = Arc::clone(&self.probe);
        let candidate = staged.clone();
        blocking(move || probe(&candidate))
            .await?
            .map_err(InstallError::ProbeFailed)?;

        let dir = self.install_dir(component);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|err| InstallError::io("创建安装目录", err))?;
        let target = self.binary_path(component);
        tokio::fs::rename(&staged, &target)
            .await
            .map_err(|err| InstallError::io("安装 FFmpeg", err))?;

        let record = dir.join(VERIFIED_RECORD);
        let component = component.clone();
        blocking(move || {
            install::write_record(
                &target,
                &record,
                &component.package.binary_sha256,
                install::Versions {
                    ffmpeg: &component.ffmpeg_version,
                    package: &component.package_version,
                },
            )
        })
        .await?;
        Ok(())
    }
}

async fn blocking<T, F>(f: F) -> Result<T, InstallError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| InstallError::io("后台任务", err))
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// 清理上次异常退出时留下的解压目录。只在持有文件锁时调用，不会删到别的进程正在用的目录。
async fn remove_stale_stages(temp: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(temp).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with("stage-") {
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
}
