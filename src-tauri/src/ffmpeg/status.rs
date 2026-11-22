//! FFmpeg 组件状态，随 `ffmpeg-status` 事件发给前端状态栏。

use std::path::PathBuf;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FfmpegStatus {
    /// 正在检查本地已安装的版本。
    Checking,
    /// 本地没有可用版本，即将下载。
    NotInstalled,
    /// 另一个录播宝进程正在安装，等它完成后再检查。
    Waiting,
    /// 正在下载平台包。
    Downloading {
        downloaded: u64,
        total: u64,
        /// 当前下载源的主机名，例如 `registry.npmmirror.com`。
        source: String,
    },
    /// 正在校验和安装下载好的文件。
    Verifying,
    /// 可用。`outdated` 为 `true` 时用的是旧版本，`install_error` 是当前版本安装失败的原因，
    /// 此时仍可调用重试。
    Ready {
        path: PathBuf,
        ffmpeg_version: String,
        package_version: String,
        outdated: bool,
        install_error: Option<String>,
    },
    /// 失败。`reason` 是给用户看的中文说明。
    Failed { reason: String, retryable: bool },
}

impl FfmpegStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_state_tag_and_camel_case_fields() {
        let downloading = FfmpegStatus::Downloading {
            downloaded: 10,
            total: 20,
            source: "registry.npmmirror.com".into(),
        };
        assert_eq!(
            serde_json::to_value(&downloading).unwrap(),
            serde_json::json!({
                "state": "downloading",
                "downloaded": 10,
                "total": 20,
                "source": "registry.npmmirror.com"
            })
        );
        assert_eq!(
            serde_json::to_value(FfmpegStatus::NotInstalled).unwrap(),
            serde_json::json!({ "state": "notInstalled" })
        );
        let ready = FfmpegStatus::Ready {
            path: PathBuf::from("ffmpeg"),
            ffmpeg_version: "9.0.2".into(),
            package_version: "9.0.1".into(),
            outdated: false,
            install_error: None,
        };
        assert_eq!(
            serde_json::to_value(&ready).unwrap(),
            serde_json::json!({
                "state": "ready",
                "path": "ffmpeg",
                "ffmpegVersion": "9.0.2",
                "packageVersion": "9.0.1",
                "outdated": false,
                "installError": null
            })
        );
        let failed = FfmpegStatus::Failed {
            reason: "两个下载源都无法连接".into(),
            retryable: true,
        };
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            serde_json::json!({
                "state": "failed",
                "reason": "两个下载源都无法连接",
                "retryable": true
            })
        );
    }
}
