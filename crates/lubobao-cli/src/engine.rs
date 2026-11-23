//! FFmpeg 的来源和录制引擎：默认用 [`FfmpegManager`] 自动下载到数据目录，
//! 也可以用 `--ffmpeg` / `LUBOBAO_FFMPEG` 指定系统里的 ffmpeg。

use std::path::PathBuf;
use std::sync::Arc;

use lubobao_core::ffmpeg::{FfmpegManager, FfmpegStatus, ManagerConfig, run_version};
use lubobao_core::platforms::BoxFuture;
use lubobao_core::recorder::{self, EventListener, RecordingRequest};
use lubobao_core::scheduler::{RecordEngine, RecordingControl};
use serde_json::{Value, json};

use crate::paths::Dirs;

#[derive(Clone)]
pub enum FfmpegSource {
    Managed(Arc<FfmpegManager>),
    /// 用户指定的 ffmpeg：绝对路径和 `-version` 的第一行（运行失败时为原因）。
    External {
        path: PathBuf,
        version: Result<String, String>,
    },
}

impl FfmpegSource {
    /// `external` 为 `--ffmpeg` 的值。`listener` 只用于自动下载的组件。
    pub fn new(
        dirs: &Dirs,
        external: Option<&PathBuf>,
        listener: lubobao_core::ffmpeg::Listener,
    ) -> FfmpegSource {
        match external {
            Some(path) => {
                let path = std::path::absolute(path).unwrap_or_else(|_| path.clone());
                let version = run_version(&path);
                FfmpegSource::External { path, version }
            }
            None => FfmpegSource::Managed(Arc::new(FfmpegManager::new(
                ManagerConfig::for_current_platform(dirs.ffmpeg_root()),
                listener,
            ))),
        }
    }

    pub fn path(&self) -> Option<PathBuf> {
        match self {
            FfmpegSource::Managed(manager) => manager.ffmpeg_path(),
            FfmpegSource::External { path, version } => version.is_ok().then(|| path.clone()),
        }
    }

    /// 状态的 JSON：自动下载的与桌面端 `ffmpeg-status` 相同；指定的为 `state: "external"`。
    pub fn status_json(&self, status: Option<&FfmpegStatus>) -> Value {
        match self {
            FfmpegSource::Managed(manager) => {
                let status = status.cloned().unwrap_or_else(|| manager.status());
                serde_json::to_value(status).unwrap_or(Value::Null)
            }
            FfmpegSource::External { path, version } => match version {
                Ok(version) => {
                    json!({ "state": "external", "path": path, "ffmpegVersion": version })
                }
                Err(reason) => {
                    json!({ "state": "external", "path": path, "error": reason })
                }
            },
        }
    }
}

/// 与桌面端的 `FfmpegEngine` 相同，只是 ffmpeg 可以来自用户指定的路径。
pub struct CliEngine {
    pub ffmpeg: FfmpegSource,
}

impl RecordEngine for CliEngine {
    fn ffmpeg_path(&self) -> Option<PathBuf> {
        self.ffmpeg.path()
    }

    fn start(
        &self,
        request: RecordingRequest,
        listener: EventListener,
    ) -> BoxFuture<'static, Arc<dyn RecordingControl>> {
        Box::pin(async move {
            let handle = recorder::start(request, listener).await;
            Arc::new(handle) as Arc<dyn RecordingControl>
        })
    }

    fn stop_all(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {
            recorder::stop_all().await;
        })
    }
}
