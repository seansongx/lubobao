//! 设置的默认值与校验。
//!
//! 键名沿用 StreamCap `config/default_settings.json`，值改为对应的 JSON 类型
//! （上游把数字存成字符串）。数据库只存用户改过的键，读取时与默认值合并。

use std::path::Path;

use serde_json::{Map, Value, json};

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
        "execute_custom_script": false,
        "custom_script_command": "",
        "system_notification_enabled": true,
        "stream_start_notification_enabled": false,
        "stream_end_notification_enabled": false,
    });
    match value {
        Value::Object(map) => map,
        _ => unreachable!(),
    }
}

/// 校验一项更新：键必须存在于默认值中，值的类型必须一致，数值项必须为正。
pub fn validate(defaults: &Map<String, Value>, key: &str, value: &Value) -> Result<(), String> {
    let Some(default) = defaults.get(key) else {
        return Err(format!("未知的设置项：{key}"));
    };
    let same_type = match (default, value) {
        (Value::Bool(_), Value::Bool(_)) | (Value::String(_), Value::String(_)) => true,
        (Value::Number(_), Value::Number(number)) => number.as_f64().is_some_and(|n| n > 0.0),
        _ => false,
    };
    if !same_type {
        return Err(format!("设置项 {key} 的值无效：{value}"));
    }
    if default.is_u64() && !value.is_u64() {
        return Err(format!("设置项 {key} 需要正整数：{value}"));
    }
    Ok(())
}
