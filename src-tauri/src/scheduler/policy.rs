//! 调度用到的纯函数：设置取值、退避时长、录制请求的组装。不发请求，便于单独测试。

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::platforms::{FetchOptions, LiveInfo, Platform, PlatformError};
use crate::recorder::{OutputFormat, RecordOptions, RecordingRequest};
use crate::tasks::{PlatformErrorKind, TaskSnapshot};

/// 检测间隔缺省值（StreamCap `loop_time_seconds`）。
pub const DEFAULT_INTERVAL_SECS: u64 = 180;
/// 每平台并发请求数缺省值（StreamCap `platform_max_concurrent_requests`）。
pub const DEFAULT_MAX_CONCURRENT: usize = 3;
/// 可重试失败的第一次退避时长，之后每次翻倍，不超过检测间隔。
pub const RETRY_BASE: Duration = Duration::from_secs(10);
/// 链接无效、需要登录、配置错误这类要用户处理的失败，至少隔这么久才再试一次。
pub const ATTENTION_RETRY: Duration = Duration::from_secs(10 * 60);
/// 录制不到这么久就因断流结束，视为「秒断」，重新检测前要退避，避免空转。
pub const MIN_VALID_RECORDING: Duration = Duration::from_secs(60);
/// 秒断后第一次重新检测前的等待，之后每次翻倍，不超过检测间隔。
pub const SHORT_RUN_BASE: Duration = Duration::from_secs(5);
/// FFmpeg 还没装好时，隔这么久再检测一次（不超过检测间隔）。
pub const FFMPEG_WAIT: Duration = Duration::from_secs(15);

fn positive_u64(settings: &Map<String, Value>, key: &str) -> Option<u64> {
    settings
        .get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
}

fn flag(settings: &Map<String, Value>, key: &str, default: bool) -> bool {
    settings
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn text<'a>(settings: &'a Map<String, Value>, key: &str) -> &'a str {
    settings
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// 检测间隔（设置 `loop_time_seconds`）。
pub fn interval(settings: &Map<String, Value>) -> Duration {
    Duration::from_secs(
        positive_u64(settings, "loop_time_seconds").unwrap_or(DEFAULT_INTERVAL_SECS),
    )
}

/// 每平台同时进行的检测数上限（设置 `platform_max_concurrent_requests`）。
pub fn max_concurrent(settings: &Map<String, Value>) -> usize {
    positive_u64(settings, "platform_max_concurrent_requests")
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_MAX_CONCURRENT)
}

/// 用户启用代理且填了地址时的代理地址（设置 `enable_proxy`、`proxy_address`）。
pub fn proxy(settings: &Map<String, Value>) -> Option<String> {
    let address = text(settings, "proxy_address").trim();
    (flag(settings, "enable_proxy", true) && !address.is_empty()).then(|| address.to_string())
}

/// `base × 2^(n-1)`，不超过 `cap`。
fn doubling(base: Duration, n: u32, cap: Duration) -> Duration {
    let factor = 1u32 << n.saturating_sub(1).min(16);
    base.saturating_mul(factor).min(cap)
}

/// 第 `attempt` 次连续检测失败后，等多久再检测。
///
/// 网络、限流、解析失败按 10 秒起翻倍退避，不超过检测间隔；其余类别要用户处理，
/// 按检测间隔和 10 分钟中较长的一个重试，修改设置后会立即重试。
pub fn retry_delay(kind: PlatformErrorKind, attempt: u32, interval: Duration) -> Duration {
    if kind.is_retryable() {
        doubling(RETRY_BASE, attempt, interval)
    } else {
        interval.max(ATTENTION_RETRY)
    }
}

/// 录制因断流或网络错误结束后，等多久重新检测。录满 [`MIN_VALID_RECORDING`] 的立即检测；
/// 连续秒断的第 `short_runs` 次按 5 秒起翻倍退避，不超过检测间隔。
pub fn recheck_delay(short_runs: u32, interval: Duration) -> Duration {
    if short_runs == 0 {
        Duration::ZERO
    } else {
        doubling(SHORT_RUN_BASE, short_runs, interval)
    }
}

/// 检测请求的选项：任务的画质，加上设置中的代理。
pub fn fetch_options(task: &TaskSnapshot, settings: &Map<String, Value>) -> FetchOptions {
    FetchOptions {
        cookies: None,
        proxy: proxy(settings),
        quality: task.config.quality,
    }
}

/// StreamCap `is_flv_preferred_platform`：这些平台默认录 FLV。
pub fn flv_preferred(platform: Platform) -> bool {
    matches!(platform, Platform::Douyin)
}

/// 录制选项。格式目前只有 TS；其余格式要等转码功能。
pub fn record_options(settings: &Map<String, Value>) -> RecordOptions {
    let threshold_gb = settings
        .get("recording_space_threshold")
        .and_then(Value::as_f64)
        .filter(|value| *value >= 0.0)
        .unwrap_or(2.0);
    let defaults = RecordOptions::default();
    RecordOptions {
        format: OutputFormat::Ts,
        segment_enabled: flag(
            settings,
            "segmented_recording_enabled",
            defaults.segment_enabled,
        ),
        segment_secs: positive_u64(settings, "video_segment_time").unwrap_or(defaults.segment_secs),
        disk_threshold_bytes: (threshold_gb * 1024.0 * 1024.0 * 1024.0) as u64,
        proxy: proxy(settings),
        folder_by_platform: flag(
            settings,
            "folder_name_platform",
            defaults.folder_by_platform,
        ),
        folder_by_anchor: flag(settings, "folder_name_author", defaults.folder_by_anchor),
        ..defaults
    }
}

/// 由检测结果和设置组装录制请求。已开播但没有可用的流地址时返回解析错误。
///
/// 取流地址按 StreamCap `_get_record_url`：FLV 优先的平台在未选 HLS 且不是 H.265 时录
/// FLV，否则录 `record_url`；开启 `force_https_recording` 时把 `http://` 换成 `https://`。
pub fn recording_request(
    task: &TaskSnapshot,
    info: &LiveInfo,
    settings: &Map<String, Value>,
    ffmpeg_path: PathBuf,
) -> Result<RecordingRequest, PlatformError> {
    let selected = info
        .selected
        .as_ref()
        .ok_or_else(|| PlatformError::Parse("已开播，但没有取到直播流地址".into()))?;
    let prefer_hls = text(settings, "default_live_source") == "HLS";
    let (source_format, url) = selected.source_url(flv_preferred(info.platform), prefer_hls);
    let mut source_url = url.to_string();
    if flag(settings, "force_https_recording", true)
        && let Some(rest) = source_url.strip_prefix("http://")
    {
        source_url = format!("https://{rest}");
    }
    let anchor_name = if info.anchor_name.trim().is_empty() {
        task.config.anchor_name.clone()
    } else {
        info.anchor_name.clone()
    };
    Ok(RecordingRequest {
        ffmpeg_path,
        source_url,
        source_format,
        headers: info.stream_headers.clone(),
        output_dir: PathBuf::from(text(settings, "live_save_path")),
        anchor_name,
        platform: info.platform.display_name().to_string(),
        options: record_options(settings),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(value: Value) -> Map<String, Value> {
        let mut map = crate::tasks::settings::defaults(std::path::Path::new("D:/录像"));
        if let Value::Object(overrides) = value {
            map.extend(overrides);
        }
        map
    }

    #[test]
    fn retryable_failures_back_off_exponentially_up_to_the_interval() {
        let interval = Duration::from_secs(180);
        let delays: Vec<u64> = (1..=6)
            .map(|n| retry_delay(PlatformErrorKind::Network, n, interval).as_secs())
            .collect();
        assert_eq!(delays, vec![10, 20, 40, 80, 160, 180]);
        assert_eq!(
            retry_delay(PlatformErrorKind::RateLimited, 50, interval),
            interval
        );
        // 间隔比基数还短时以间隔为上限。
        assert_eq!(
            retry_delay(PlatformErrorKind::Parse, 1, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn failures_needing_the_user_are_not_retried_quickly() {
        for kind in [
            PlatformErrorKind::InvalidUrl,
            PlatformErrorKind::AuthRequired,
            PlatformErrorKind::Config,
        ] {
            assert_eq!(
                retry_delay(kind, 1, Duration::from_secs(180)),
                ATTENTION_RETRY
            );
            assert_eq!(
                retry_delay(kind, 1, Duration::from_secs(3600)),
                Duration::from_secs(3600)
            );
        }
    }

    #[test]
    fn short_recordings_back_off_before_rechecking() {
        let interval = Duration::from_secs(180);
        assert_eq!(recheck_delay(0, interval), Duration::ZERO);
        assert_eq!(recheck_delay(1, interval).as_secs(), 5);
        assert_eq!(recheck_delay(3, interval).as_secs(), 20);
        assert_eq!(recheck_delay(20, interval), interval);
    }

    #[test]
    fn reads_interval_concurrency_and_proxy_from_settings() {
        let map = settings(json!({}));
        assert_eq!(interval(&map), Duration::from_secs(180));
        assert_eq!(max_concurrent(&map), 3);
        assert_eq!(proxy(&map), None);
        let map = settings(json!({
            "loop_time_seconds": 30,
            "platform_max_concurrent_requests": 1,
            "proxy_address": " 127.0.0.1:7890 ",
        }));
        assert_eq!(interval(&map), Duration::from_secs(30));
        assert_eq!(max_concurrent(&map), 1);
        assert_eq!(proxy(&map).as_deref(), Some("127.0.0.1:7890"));
        let map = settings(json!({ "enable_proxy": false, "proxy_address": "127.0.0.1:7890" }));
        assert_eq!(proxy(&map), None);
    }

    #[test]
    fn maps_recording_settings_to_record_options() {
        let map = settings(json!({
            "segmented_recording_enabled": false,
            "video_segment_time": 600,
            "recording_space_threshold": 0.5,
            "folder_name_platform": false,
        }));
        let options = record_options(&map);
        assert!(!options.segment_enabled);
        assert_eq!(options.segment_secs, 600);
        assert_eq!(options.disk_threshold_bytes, 512 * 1024 * 1024);
        assert!(!options.folder_by_platform);
        assert!(options.folder_by_anchor);
        assert_eq!(
            record_options(&settings(json!({}))).disk_threshold_bytes,
            2 << 30
        );
    }
}
