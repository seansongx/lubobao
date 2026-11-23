//! 调度用的真实实现：平台适配器和 FFmpeg 录制引擎。

use std::path::PathBuf;
use std::sync::Arc;

use super::{LiveSource, RecordEngine, RecordingControl};
use crate::ffmpeg::FfmpegManager;
use crate::platforms::{BoxFuture, FetchOptions, LiveInfo, PlatformError, adapter_for};
use crate::recorder::{self, EventListener, RecordingHandle, RecordingOutcome, RecordingRequest};

/// 用 [`adapter_for`] 选出的平台适配器检测。
pub struct PlatformSource;

impl LiveSource for PlatformSource {
    fn fetch(
        &self,
        url: String,
        options: FetchOptions,
    ) -> BoxFuture<'static, Result<LiveInfo, PlatformError>> {
        Box::pin(async move {
            let adapter = adapter_for(&url).ok_or_else(|| {
                PlatformError::InvalidUrl(format!("暂不支持这个链接的平台：{url}"))
            })?;
            adapter.fetch(&url, &options).await
        })
    }
}

impl RecordingControl for RecordingHandle {
    fn stop(&self) {
        RecordingHandle::stop(self);
    }

    fn wait(&self) -> BoxFuture<'static, RecordingOutcome> {
        let handle = self.clone();
        Box::pin(async move { handle.wait().await })
    }
}

/// [`recorder::start`]，ffmpeg 取自 [`FfmpegManager`]。
pub struct FfmpegEngine {
    ffmpeg: Arc<FfmpegManager>,
}

impl FfmpegEngine {
    pub fn new(ffmpeg: Arc<FfmpegManager>) -> Self {
        FfmpegEngine { ffmpeg }
    }
}

impl RecordEngine for FfmpegEngine {
    fn ffmpeg_path(&self) -> Option<PathBuf> {
        self.ffmpeg.ffmpeg_path()
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
