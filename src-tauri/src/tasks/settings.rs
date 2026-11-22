//! 设置的默认值与校验。
//!
//! 键名沿用 StreamCap `config/default_settings.json`，值改为对应的 JSON 类型
//! （上游把数字存成字符串）。数据库只存用户改过的键，读取时与默认值合并。

use std::path::Path;

use serde_json::{Map, Value, json};

use super::{CommandError, ErrorKind};
use crate::platforms::{FetchOptions, Platform, Quality};

/// 录制相关的默认值。取自 StreamCap 默认配置（快照 `e1e8dbf`）。
///
/// `live_save_path` 上游默认为空，表示程序目录下的 `downloads`；录播宝改为
/// 系统「视频」目录下的 `录播宝`，由调用方传入。
pub fn defaults(save_dir: &Path) -> Map<String, Value> {
    let value = json!({
        "live_save_path": save_dir.to_string_lossy(),
        "loop_time_seconds": 180,
        "platform_max_concurrent_requests": 3,
        "video_format": "TS",
        "record_quality": "OD",
        "segmented_recording_enabled": true,
        "video_segment_time": 1800,
        "recording_space_threshold": 2.0,
        "default_live_source": "FLV",
        "force_https_recording": true,
        "flv_use_direct_download": false,
        "convert_to_mp4": true,
        "delete_original": false,
        "generate_time_subtitle_file": false,
        "only_notify_no_record": false,
        "filename_includes_title": false,
        "remove_emojis": false,
        "folder_name_platform": true,
        "folder_name_author": true,
        "folder_name_time": false,
        "folder_name_title": false,
        "enable_proxy": true,
        "proxy_address": "",
        "default_platform_with_proxy": "tiktok, sooplive, pandalive, winktv, flextv, popkontv, twitch, liveme, showroom, chzzk, shopee, shp, youtu, youtube, lang",
        "execute_custom_script": false,
        "custom_script_command": "",
        "platform_cookies": {},
        "system_notification_enabled": true,
        "stream_start_notification_enabled": true,
        "stream_end_notification_enabled": false,
        "close_window_action": "ask",
    });
    match value {
        Value::Object(map) => map,
        _ => unreachable!(),
    }
}

/// 取值有限的设置项及其合法值。格式取自 StreamCap `VideoFormat` 与 `AudioFormat`，
/// 直播源取自设置页的下拉选项。
pub const ENUM_SETTINGS: &[(&str, &[&str])] = &[
    ("record_quality", &["OD", "UHD", "HD", "SD", "LD"]),
    (
        "video_format",
        &[
            "TS", "MP4", "FLV", "MKV", "MOV", "NUT", "WAV", "MP3", "WMA", "M4A", "AAC",
        ],
    ),
    ("default_live_source", &["FLV", "HLS"]),
    // 关闭主窗口时：每次询问、隐藏到托盘、退出。
    ("close_window_action", &["ask", "tray", "quit"]),
];

/// 校验一项更新：键必须存在于默认值中，值的类型必须一致，数值项必须为正，
/// 枚举项必须是 [`ENUM_SETTINGS`] 中的值，`platform_cookies` 见 [`validate_cookies`]。
/// Cookie 无效时错误类别为 `config`，其余为 `invalid_input`。
pub fn validate(
    defaults: &Map<String, Value>,
    key: &str,
    value: &Value,
) -> Result<(), CommandError> {
    let invalid = |message: String| CommandError::new(ErrorKind::InvalidInput, message);
    let Some(default) = defaults.get(key) else {
        return Err(invalid(format!("未知的设置项：{key}")));
    };
    if key == "platform_cookies" {
        return validate_cookies(value)
            .map_err(|message| CommandError::new(ErrorKind::Config, message));
    }
    let same_type = match (default, value) {
        (Value::Bool(_), Value::Bool(_)) | (Value::String(_), Value::String(_)) => true,
        (Value::Number(_), Value::Number(number)) => number.as_f64().is_some_and(|n| n > 0.0),
        _ => false,
    };
    if !same_type {
        return Err(invalid(format!("设置项 {key} 的值无效：{value}")));
    }
    if default.is_u64() && !value.is_u64() {
        return Err(invalid(format!("设置项 {key} 需要正整数：{value}")));
    }
    if let Some((_, allowed)) = ENUM_SETTINGS.iter().find(|(name, _)| *name == key) {
        let text = value.as_str().unwrap_or_default();
        if !allowed.contains(&text) {
            return Err(invalid(format!(
                "设置项 {key} 只能是 {} 之一：{value}",
                allowed.join("、")
            )));
        }
    }
    Ok(())
}

/// `platform_cookies`：对象，键为平台键（`douyin`、`kuaishou`……），值为 Cookie 字符串
/// （对应 StreamCap `cookies.json`）。值必须能放进请求头：不能有换行等控制字符（制表符
/// 除外），与 reqwest `HeaderValue::from_str` 的规则一致。空字符串表示不用 Cookie。
pub fn validate_cookies(value: &Value) -> Result<(), String> {
    let Value::Object(map) = value else {
        return Err(format!("平台 Cookie 必须是以平台为键的对象：{value}"));
    };
    for (platform, cookie) in map {
        let Value::String(cookie) = cookie else {
            return Err(format!("{platform} 的 Cookie 必须是字符串"));
        };
        if reqwest::header::HeaderValue::from_str(cookie).is_err() {
            return Err(format!(
                "{platform} 的 Cookie 含有换行等无法放进请求头的字符"
            ));
        }
    }
    Ok(())
}

/// 这个平台的 Cookie（设置 `platform_cookies`），去掉首尾空白；没有或为空时为 `None`。
pub fn cookies_for(settings: &Map<String, Value>, platform_key: &str) -> Option<String> {
    settings
        .get("platform_cookies")
        .and_then(|cookies| cookies.get(platform_key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|cookie| !cookie.is_empty())
        .map(str::to_string)
}

/// 设置 `default_platform_with_proxy` 中的平台键。与 StreamCap `is_use_proxy` 相同：
/// 中文逗号换成英文逗号，去掉所有空格，按逗号分开，平台键须完全相同（区分大小写）。
pub fn proxy_platforms(settings: &Map<String, Value>) -> Vec<String> {
    settings
        .get("default_platform_with_proxy")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace('，', ",")
        .replace(' ', "")
        .split(',')
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .collect()
}

/// 这个平台要用的代理地址：`enable_proxy` 为 true、`proxy_address` 不为空，且平台键在
/// `default_platform_with_proxy` 中。检测和录制都按这个规则。
pub fn proxy_for(settings: &Map<String, Value>, platform_key: &str) -> Option<String> {
    let enabled = settings
        .get("enable_proxy")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let address = settings
        .get("proxy_address")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    (enabled
        && !address.is_empty()
        && proxy_platforms(settings)
            .iter()
            .any(|key| key == platform_key))
    .then(|| address.to_string())
}

/// 请求平台时的选项：画质，加上这个平台的代理和 Cookie。添加任务和定时检测共用。
pub fn fetch_options(
    settings: &Map<String, Value>,
    platform: Platform,
    quality: Quality,
) -> FetchOptions {
    FetchOptions {
        cookies: cookies_for(settings, platform.key()),
        proxy: proxy_for(settings, platform.key()),
        quality,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults_with(overrides: Value) -> Map<String, Value> {
        let mut map = defaults(Path::new("D:/录像"));
        if let Value::Object(overrides) = overrides {
            map.extend(overrides);
        }
        map
    }

    #[test]
    fn default_proxy_list_matches_streamcap_and_skips_domestic_platforms() {
        let map = defaults_with(json!({ "proxy_address": "127.0.0.1:7890" }));
        assert!(proxy_platforms(&map).contains(&"tiktok".to_string()));
        assert_eq!(proxy_for(&map, "douyin"), None);
        assert_eq!(proxy_for(&map, "kuaishou"), None);
        assert_eq!(proxy_for(&map, "tiktok").as_deref(), Some("127.0.0.1:7890"));
    }

    #[test]
    fn listed_platforms_use_the_proxy_with_streamcap_matching() {
        let map = defaults_with(json!({
            "proxy_address": "127.0.0.1:7890",
            "default_platform_with_proxy": "tiktok，dou yin , twitch",
        }));
        // StreamCap 去掉所有空格后比较，所以「dou yin」也算 douyin。
        assert_eq!(proxy_for(&map, "douyin").as_deref(), Some("127.0.0.1:7890"));
        assert_eq!(proxy_for(&map, "kuaishou"), None);
        let map = defaults_with(json!({
            "proxy_address": "127.0.0.1:7890",
            "default_platform_with_proxy": "Douyin",
        }));
        assert_eq!(
            proxy_for(&map, "douyin"),
            None,
            "与 StreamCap 一样区分大小写"
        );
        let map = defaults_with(json!({
            "enable_proxy": false,
            "proxy_address": "127.0.0.1:7890",
            "default_platform_with_proxy": "douyin",
        }));
        assert_eq!(proxy_for(&map, "douyin"), None);
    }

    #[test]
    fn reads_platform_cookies_by_platform_key() {
        let map = defaults_with(json!({
            "platform_cookies": { "douyin": "  ttwid=abc; a=1 ", "kuaishou": "" },
        }));
        assert_eq!(
            cookies_for(&map, "douyin").as_deref(),
            Some("ttwid=abc; a=1")
        );
        assert_eq!(cookies_for(&map, "kuaishou"), None);
        assert_eq!(cookies_for(&defaults_with(json!({})), "douyin"), None);
        let options = fetch_options(&map, Platform::Douyin, Quality::Hd);
        assert_eq!(options.cookies.as_deref(), Some("ttwid=abc; a=1"));
        assert_eq!(options.proxy, None);
        assert_eq!(options.quality, Quality::Hd);
    }

    #[test]
    fn validates_platform_cookies_as_config() {
        let defaults = defaults(Path::new("D:/录像"));
        let ok = json!({ "douyin": "ttwid=abc; 中文=值\t", "kuaishou": "" });
        assert!(validate(&defaults, "platform_cookies", &ok).is_ok());
        for bad in [
            json!({ "douyin": "a=1\nb=2" }),
            json!({ "douyin": "a=1\r" }),
            json!({ "douyin": "a\u{0}" }),
            json!({ "douyin": 1 }),
            json!("ttwid=abc"),
        ] {
            let error = validate(&defaults, "platform_cookies", &bad).unwrap_err();
            assert_eq!(error.kind, ErrorKind::Config, "{bad}");
        }
        // 其他设置项仍是 invalid_input。
        let error = validate(&defaults, "loop_time_seconds", &json!(0)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidInput);
    }
}
