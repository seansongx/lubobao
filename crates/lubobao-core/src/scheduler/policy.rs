//! 调度用到的纯函数：设置取值、退避时长、录制请求的组装。不发请求，便于单独测试。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::platforms::{FetchOptions, LiveInfo, Platform, PlatformError};
use crate::recorder::postprocess::ConvertOptions;
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

/// 这个平台要用的代理地址，规则见 [`crate::tasks::settings::proxy_for`]。
pub fn proxy(settings: &Map<String, Value>, platform: Platform) -> Option<String> {
    crate::tasks::settings::proxy_for(settings, platform.key())
}

/// 录制用的代理，以及不能用时给用户的提示。FFmpeg 只支持 HTTP 代理；SOCKS、HTTPS 等
/// 协议的代理只用于检测，录制改为直连，否则每次录制都会失败。
pub fn recording_proxy(
    settings: &Map<String, Value>,
    platform: Platform,
) -> (Option<String>, Option<String>) {
    let Some(address) = proxy(settings, platform) else {
        return (None, None);
    };
    match crate::recorder::command::ffmpeg_proxy(&address) {
        Ok(proxy) => (proxy, None),
        Err(_) => {
            let scheme = address.split_once("://").map_or("", |(scheme, _)| scheme);
            (
                None,
                Some(format!(
                    "录制不支持 {} 代理，已直连录制（检测仍使用代理）",
                    scheme.to_ascii_uppercase()
                )),
            )
        }
    }
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

/// 检测请求的选项：任务的画质，加上这个平台的代理和 Cookie。
pub fn fetch_options(task: &TaskSnapshot, settings: &Map<String, Value>) -> FetchOptions {
    crate::tasks::settings::fetch_options(settings, task.config.platform, task.config.quality)
}

/// StreamCap `is_flv_preferred_platform`：这些平台默认录 FLV。
pub fn flv_preferred(platform: Platform) -> bool {
    matches!(platform, Platform::Douyin | Platform::Tiktok)
}

/// 录制选项。格式目前只有 TS；其余格式要等转码功能。代理由 [`recording_proxy`] 另行决定。
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
        proxy: None,
        folder_by_platform: flag(
            settings,
            "folder_name_platform",
            defaults.folder_by_platform,
        ),
        folder_by_anchor: flag(settings, "folder_name_author", defaults.folder_by_anchor),
        ..defaults
    }
}

/// 录后转 MP4 的选项（设置 `convert_to_mp4`、`delete_original`，缺省值同 StreamCap：
/// 转换、保留原件）。关闭转换时返回 `None`。
pub fn convert_options(
    settings: &Map<String, Value>,
    ffmpeg_path: PathBuf,
) -> Option<ConvertOptions> {
    wants_conversion(settings).then(|| ConvertOptions {
        ffmpeg_path,
        delete_original: flag(settings, "delete_original", false),
        // 转换后仍要留出录制的磁盘阈值，与 `record_options` 用同一个值。
        reserve_bytes: record_options(settings).disk_threshold_bytes,
    })
}

/// 设置 `convert_to_mp4`，缺省为 true（同 StreamCap）。
pub fn wants_conversion(settings: &Map<String, Value>) -> bool {
    flag(settings, "convert_to_mp4", true)
}

/// 一次录制里要转 MP4 的文件：磁盘上存在的 TS，保持原顺序。引擎目前只录 TS
/// （`video_format` 暂时忽略），所以按扩展名判断，与 StreamCap `save_format == "ts"` 等价。
pub fn convertible_files(files: &[PathBuf]) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
                && path.is_file()
        })
        .cloned()
        .collect()
}

/// 组装好的录制请求，以及要提示用户的问题（例如代理协议不支持、已直连）。
pub struct RecordingPlan {
    pub request: RecordingRequest,
    pub warning: Option<String>,
}

/// 由检测结果和设置组装录制请求。已开播但没有可用的流地址时返回解析错误。
/// `live_save_path` 为空时用 `default_save_dir`。
///
/// 取流地址按 StreamCap `_get_record_url`：FLV 优先的平台在未选 HLS 且不是 H.265 时录
/// FLV，否则录 `record_url`；开启 `force_https_recording` 时把 `http://` 换成 `https://`。
pub fn recording_request(
    task: &TaskSnapshot,
    info: &LiveInfo,
    settings: &Map<String, Value>,
    ffmpeg_path: PathBuf,
    default_save_dir: &Path,
) -> Result<RecordingPlan, PlatformError> {
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
    let save_dir = text(settings, "live_save_path").trim();
    let output_dir = if save_dir.is_empty() {
        default_save_dir.to_path_buf()
    } else {
        PathBuf::from(save_dir)
    };
    let (proxy, warning) = recording_proxy(settings, info.platform);
    Ok(RecordingPlan {
        request: RecordingRequest {
            ffmpeg_path,
            source_url,
            source_format,
            headers: info.stream_headers.clone(),
            output_dir,
            anchor_name,
            platform: info.platform.display_name().to_string(),
            options: RecordOptions {
                proxy,
                ..record_options(settings)
            },
        },
        warning,
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
    fn douyin_and_tiktok_record_flv_by_default_like_streamcap() {
        assert!(flv_preferred(Platform::Douyin));
        assert!(flv_preferred(Platform::Tiktok));
        assert!(!flv_preferred(Platform::Huya));
    }

    #[test]
    fn reads_interval_concurrency_and_proxy_from_settings() {
        let map = settings(json!({}));
        assert_eq!(interval(&map), Duration::from_secs(180));
        assert_eq!(max_concurrent(&map), 3);
        assert_eq!(proxy(&map, Platform::Douyin), None);
        let map = settings(json!({
            "loop_time_seconds": 30,
            "platform_max_concurrent_requests": 1,
            "proxy_address": " 127.0.0.1:7890 ",
            "default_platform_with_proxy": "tiktok，douyin, twitch",
        }));
        assert_eq!(interval(&map), Duration::from_secs(30));
        assert_eq!(max_concurrent(&map), 1);
        assert_eq!(
            proxy(&map, Platform::Douyin).as_deref(),
            Some("127.0.0.1:7890")
        );
        let map = settings(json!({
            "enable_proxy": false,
            "proxy_address": "127.0.0.1:7890",
            "default_platform_with_proxy": "douyin",
        }));
        assert_eq!(proxy(&map, Platform::Douyin), None);
    }

    #[test]
    fn uses_the_proxy_only_for_listed_platforms() {
        // StreamCap 默认列表只有海外平台，抖音直连。
        let map = settings(json!({ "proxy_address": "127.0.0.1:7890" }));
        assert_eq!(proxy(&map, Platform::Douyin), None);
        assert_eq!(recording_proxy(&map, Platform::Douyin), (None, None));
    }

    #[test]
    fn records_directly_when_the_proxy_is_not_http() {
        let listed = |address: &str| {
            settings(json!({ "proxy_address": address, "default_platform_with_proxy": "douyin" }))
        };
        let (proxy, warning) =
            recording_proxy(&listed("socks5://127.0.0.1:1080"), Platform::Douyin);
        assert_eq!(proxy, None);
        assert_eq!(
            warning.as_deref(),
            Some("录制不支持 SOCKS5 代理，已直连录制（检测仍使用代理）")
        );
        let (proxy, warning) = recording_proxy(&listed("https://proxy:443"), Platform::Douyin);
        assert_eq!((proxy, warning.is_some()), (None, true));
        assert_eq!(
            recording_proxy(&listed("127.0.0.1:7890"), Platform::Douyin),
            (Some("http://127.0.0.1:7890".to_string()), None)
        );
        assert_eq!(
            recording_proxy(&listed("HTTP://127.0.0.1:7890"), Platform::Douyin),
            (Some("HTTP://127.0.0.1:7890".to_string()), None)
        );
    }

    #[test]
    fn falls_back_to_the_default_save_dir_when_the_setting_is_empty() {
        let info = crate::scheduler::tests::live_info("https://live.douyin.com/1", true);
        let task = crate::scheduler::tests::snapshot_for(&info);
        let default_dir = Path::new("C:/Videos/录播宝");
        for value in ["", "   "] {
            let map = settings(json!({ "live_save_path": value }));
            let plan = recording_request(&task, &info, &map, PathBuf::from("ffmpeg"), default_dir)
                .unwrap();
            assert_eq!(plan.request.output_dir, default_dir);
        }
        let map = settings(json!({ "live_save_path": "E:/rec" }));
        let plan =
            recording_request(&task, &info, &map, PathBuf::from("ffmpeg"), default_dir).unwrap();
        assert_eq!(plan.request.output_dir, PathBuf::from("E:/rec"));
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
