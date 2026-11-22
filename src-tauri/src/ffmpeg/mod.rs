//! FFmpeg 组件管理：启动时检查本地版本，缺失时从 `@seankit/ffmpeg` 下载、校验并原子安装。
//!
//! 安装位置：`<app_local_data_dir>/components/ffmpeg/<ffmpegVersion>-<packageVersion>/ffmpeg(.exe)`，
//! 下载和解压的临时文件放在同一目录下的 `.tmp/`，保证最后一步改名在同一块磁盘上。

pub mod download;
mod error;
mod install;
pub mod manifest;
mod status;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::{Emitter, Manager};

pub use download::DownloadOptions;
pub use error::InstallError;
pub use install::run_version;
pub use manifest::{Component, Manifest, PlatformPackage};
pub use status::FfmpegStatus;

/// 状态变化时发给前端的事件名。
pub const STATUS_EVENT: &str = "ffmpeg-status";

const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const VERIFIED_RECORD: &str = "verified.json";

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

    /// 可用时返回 FFmpeg 的绝对路径。
    pub fn ffmpeg_path(&self) -> Option<PathBuf> {
        match self.status() {
            FfmpegStatus::Ready { path, .. } => Some(path),
            _ => None,
        }
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

    /// 检查本地版本，必要时下载安装。同一时间只运行一个；已在运行时直接返回当前状态。
    /// 失败后再次调用即为重试，已下载的部分会续传。
    pub async fn ensure(&self) -> FfmpegStatus {
        let Ok(_guard) = self.install_lock.try_lock() else {
            return self.status();
        };
        let status = self.run().await;
        self.set(status.clone());
        status
    }

    async fn run(&self) -> FfmpegStatus {
        self.set(FfmpegStatus::Checking);
        let component = match &self.component {
            Ok(component) => component.clone(),
            Err(reason) => {
                return FfmpegStatus::Failed {
                    reason: reason.clone(),
                    retryable: false,
                };
            }
        };
        let ready = FfmpegStatus::Ready {
            path: self.binary_path(&component),
            ffmpeg_version: component.ffmpeg_version.clone(),
            package_version: component.package_version.clone(),
        };

        let binary = self.binary_path(&component);
        let record = self.install_dir(&component).join(VERIFIED_RECORD);
        let package = component.package.clone();
        let installed = blocking(move || install::check_installed(&binary, &record, &package))
            .await
            .unwrap_or(false);
        if installed {
            return ready;
        }

        self.set(FfmpegStatus::NotInstalled);
        match self.install(&component).await {
            Ok(()) => ready,
            Err(err) => FfmpegStatus::Failed {
                reason: err.to_string(),
                retryable: true,
            },
        }
    }

    async fn install(&self, component: &Component) -> Result<(), InstallError> {
        let temp = self.temp_dir();
        tokio::fs::create_dir_all(&temp)
            .await
            .map_err(|err| InstallError::io("创建临时目录", err))?;
        remove_stale_stages(&temp).await;

        let part = self.part_path(component);
        let package = &component.package;
        let mut last_emit: Option<Instant> = None;
        let mut last_source = String::new();
        let mut on_progress = |downloaded: u64, total: u64, source: &str| {
            let now = Instant::now();
            let due = last_emit.is_none_or(|at| now.duration_since(at) >= PROGRESS_INTERVAL);
            if due || downloaded == total || source != last_source {
                last_emit = Some(now);
                last_source = source.to_string();
                self.set(FfmpegStatus::Downloading {
                    downloaded,
                    total,
                    source: source.to_string(),
                });
            }
        };
        download::download(
            &self.client,
            &self.download,
            &package.urls,
            &part,
            package.size,
            &mut on_progress,
        )
        .await?;

        self.set(FfmpegStatus::Verifying);
        let stage = temp.join(format!("stage-{}", unique_suffix()));
        let result = self.verify_and_install(component, &part, &stage).await;
        let _ = tokio::fs::remove_dir_all(&stage).await;
        // 本地读写失败时保留已下载的文件，其余情况（包括成功）删除。
        if !matches!(result, Err(InstallError::Io { .. })) {
            let _ = tokio::fs::remove_file(&part).await;
        }
        result
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
        let sha256 = component.package.binary_sha256.clone();
        blocking(move || install::write_record(&target, &record, &sha256)).await?;
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

/// 清理上次异常退出时留下的解压目录。下载的 `.part` 文件保留以便续传。
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

/// 在 Tauri `setup` 中调用：登记状态并在后台开始检查，不阻塞窗口。
pub fn init(app: &tauri::AppHandle) -> tauri::Result<()> {
    let root = app
        .path()
        .app_local_data_dir()?
        .join("components")
        .join("ffmpeg");
    let handle = app.clone();
    let manager = Arc::new(FfmpegManager::new(
        ManagerConfig::for_current_platform(root),
        Box::new(move |status: &FfmpegStatus| {
            let _ = handle.emit(STATUS_EVENT, status);
        }),
    ));
    app.manage(Arc::clone(&manager));
    tauri::async_runtime::spawn(async move {
        manager.ensure().await;
    });
    Ok(())
}

#[tauri::command]
pub fn get_ffmpeg_status(manager: tauri::State<'_, Arc<FfmpegManager>>) -> FfmpegStatus {
    manager.status()
}

/// 重新检查并安装。已在运行时不会再启动一个任务。返回调用时的状态，后续变化通过事件推送。
#[tauri::command]
pub fn retry_ffmpeg_install(manager: tauri::State<'_, Arc<FfmpegManager>>) -> FfmpegStatus {
    let task = Arc::clone(manager.inner());
    tauri::async_runtime::spawn(async move {
        task.ensure().await;
    });
    manager.status()
}
