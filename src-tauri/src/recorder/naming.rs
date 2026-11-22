//! 录像目录与文件命名。
//!
//! 参照 StreamCap `StreamManager._get_filename`、`_get_output_dir`、`_get_save_path`
//! 和 `utils.clean_name`：文件名为「主播名_YYYY-MM-DD_HH-MM-SS」，分段时追加 `_%03d`；
//! 目录默认为「输出目录/平台/主播名」。全部是纯函数，便于单元测试。

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

/// 主播名为空时的默认名称，对应 StreamCap 语言包中的 `live_room`。
pub const DEFAULT_ANCHOR_NAME: &str = "直播间";

/// 平台名为空时的目录名。
pub const DEFAULT_PLATFORM_NAME: &str = "其他平台";

/// 清理后名称的最大字符数。StreamCap 不截断；这里限制长度，避免 Windows 路径过长。
pub const MAX_NAME_CHARS: usize = 60;

/// 时间部分的格式，与 StreamCap 相同。
pub const TIME_FORMAT: &str = "%Y-%m-%d_%H-%M-%S";

/// 录像文件扩展名（目前只支持 TS）。
pub const TS_EXTENSION: &str = "ts";

/// StreamCap `clean_name` 替换为 `_` 的字符，另加 `%`（分段文件名会被 FFmpeg 当作格式串）
/// 和 `'`、`` ` ``（避免脚本处理路径时出错）。
const REPLACED_CHARS: &[char] = &[
    '/', '\\', ':', '：', '*', '？', '?', '"', '<', '>', '|', '&', '#', '.', '。', ',', '，', ' ',
    '~', '！', '·', '%', '\'', '`',
];

/// Windows 保留的设备名，不能作为文件或目录名（不区分大小写）。
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 按 StreamCap `clean_name` 的规则清理名称，使其可用作 Windows、macOS 和 Linux 的文件名：
///
/// - 去掉首尾空白，全角括号换成半角；
/// - 路径分隔符、Windows 禁用字符、标点、空格、控制字符和 emoji 换成 `_`；
/// - 连续的 `_` 合并为一个，去掉首尾的 `_`；
/// - 截断到 [`MAX_NAME_CHARS`] 个字符，避开 Windows 保留设备名。
///
/// 清理后为空时返回 `None`。中文等非 ASCII 字符原样保留。
pub fn clean_name(input: &str) -> Option<String> {
    let mut cleaned = String::with_capacity(input.len());
    for ch in input.trim().chars() {
        let ch = match ch {
            '（' => '(',
            '）' => ')',
            other => other,
        };
        if REPLACED_CHARS.contains(&ch) || ch.is_control() || ch.is_whitespace() || is_emoji(ch) {
            if !cleaned.ends_with('_') {
                cleaned.push('_');
            }
        } else {
            cleaned.push(ch);
        }
    }

    let truncated: String = cleaned
        .trim_matches('_')
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    let mut name = truncated.trim_matches('_').to_string();
    if name.is_empty() {
        return None;
    }
    if RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(&name))
    {
        name.push('_');
    }
    Some(name)
}

/// 主播名清理后的结果，为空时用 [`DEFAULT_ANCHOR_NAME`]。
pub fn anchor_label(anchor_name: &str) -> String {
    clean_name(anchor_name).unwrap_or_else(|| DEFAULT_ANCHOR_NAME.to_string())
}

/// 文件名主体：`主播名_YYYY-MM-DD_HH-MM-SS`，不含扩展名和分段序号。
pub fn file_stem(anchor_name: &str, time: NaiveDateTime) -> String {
    format!("{}_{}", anchor_label(anchor_name), time.format(TIME_FORMAT))
}

/// 录像目录。StreamCap 默认按平台和主播分目录（`folder_name_platform`、
/// `folder_name_author` 均为 `true`）。
pub fn recording_dir(
    output_root: &Path,
    platform: &str,
    anchor_name: &str,
    by_platform: bool,
    by_anchor: bool,
) -> PathBuf {
    let mut dir = output_root.to_path_buf();
    if by_platform {
        dir.push(clean_name(platform).unwrap_or_else(|| DEFAULT_PLATFORM_NAME.to_string()));
    }
    if by_anchor {
        dir.push(anchor_label(anchor_name));
    }
    dir
}

/// 交给 FFmpeg 的输出路径。分段时为 `stem_%03d.ts`，由 segment 封装器编号。
pub fn output_path(dir: &Path, stem: &str, segmented: bool) -> PathBuf {
    if segmented {
        dir.join(format!("{stem}_%03d.{TS_EXTENSION}"))
    } else {
        dir.join(format!("{stem}.{TS_EXTENSION}"))
    }
}

/// 第一个文件的实际路径，用于判断是否与已有录像重名。
pub fn first_file_path(dir: &Path, stem: &str, segmented: bool) -> PathBuf {
    if segmented {
        dir.join(format!("{stem}_000.{TS_EXTENSION}"))
    } else {
        dir.join(format!("{stem}.{TS_EXTENSION}"))
    }
}

/// 同一秒内重复开始录制时给文件名加序号：`stem`、`stem_2`、`stem_3`……
/// `exists` 判断某个主体名对应的第一个文件是否已存在。
pub fn unique_stem(base: &str, mut exists: impl FnMut(&str) -> bool) -> String {
    if !exists(base) {
        return base.to_string();
    }
    (2..)
        .map(|index| format!("{base}_{index}"))
        .find(|candidate| !exists(candidate))
        .expect("an unused suffix always exists")
}

/// 判断目录中的文件名是否属于某次分段录制：`stem_NNN.ts`。
pub fn is_segment_of(file_name: &str, stem: &str) -> bool {
    let Some(rest) = file_name.strip_prefix(stem) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('_') else {
        return false;
    };
    let Some(index) = rest.strip_suffix(&format!(".{TS_EXTENSION}")) else {
        return false;
    };
    index.len() >= 3 && index.bytes().all(|byte| byte.is_ascii_digit())
}

/// 常见 emoji 所在的码位区间。StreamCap 用 `EMOJI_PATTERN` 正则移除 emoji，这里按区间近似。
fn is_emoji(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1F000..=0x1FAFF
            | 0x2600..=0x27BF
            | 0x2B00..=0x2BFF
            | 0xFE00..=0xFE0F
            | 0x200D
            | 0x20E3
            | 0xE0020..=0xE007F
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn sample_time() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 22)
            .unwrap()
            .and_hms_opt(9, 5, 7)
            .unwrap()
    }

    #[test]
    fn clean_name_replaces_windows_forbidden_characters() {
        assert_eq!(
            clean_name(r#"a/b\c:d*e?f"g<h>i|j"#).as_deref(),
            Some("a_b_c_d_e_f_g_h_i_j")
        );
    }

    #[test]
    fn clean_name_follows_streamcap_punctuation_rules() {
        assert_eq!(
            clean_name("  主播：小明。今天，直播！~ ").as_deref(),
            Some("主播_小明_今天_直播")
        );
        assert_eq!(clean_name("（官方）号").as_deref(), Some("(官方)号"));
        assert_eq!(clean_name("a&b#c.d·e").as_deref(), Some("a_b_c_d_e"));
    }

    #[test]
    fn clean_name_removes_emoji_and_control_characters() {
        assert_eq!(clean_name("小🐟儿🎉").as_deref(), Some("小_儿"));
        assert_eq!(clean_name("a\tb\nc\u{7}d").as_deref(), Some("a_b_c_d"));
        assert_eq!(clean_name("❤️love").as_deref(), Some("love"));
    }

    #[test]
    fn clean_name_escapes_percent_for_segment_patterns() {
        assert_eq!(clean_name("100%真实").as_deref(), Some("100_真实"));
    }

    #[test]
    fn clean_name_returns_none_when_nothing_is_left() {
        assert_eq!(clean_name(""), None);
        assert_eq!(clean_name("  "), None);
        assert_eq!(clean_name("..._?/"), None);
        assert_eq!(clean_name("🎉🎉"), None);
    }

    #[test]
    fn clean_name_avoids_windows_reserved_names() {
        assert_eq!(clean_name("con").as_deref(), Some("con_"));
        assert_eq!(clean_name("LPT1").as_deref(), Some("LPT1_"));
        assert_eq!(clean_name("console").as_deref(), Some("console"));
    }

    #[test]
    fn clean_name_truncates_long_names() {
        let long = "长".repeat(100);
        assert_eq!(clean_name(&long).unwrap().chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn file_stem_uses_anchor_and_local_time() {
        assert_eq!(
            file_stem("测试 主播", sample_time()),
            "测试_主播_2026-11-22_09-05-07"
        );
        assert_eq!(file_stem("  ", sample_time()), "直播间_2026-11-22_09-05-07");
    }

    #[test]
    fn recording_dir_groups_by_platform_and_anchor() {
        let root = Path::new(r"D:\录像");
        assert_eq!(
            recording_dir(root, "抖音", "小明/Official", true, true),
            root.join("抖音").join("小明_Official")
        );
        assert_eq!(recording_dir(root, "抖音", "小明", false, false), root);
        assert_eq!(
            recording_dir(root, "", "", true, true),
            root.join(DEFAULT_PLATFORM_NAME).join(DEFAULT_ANCHOR_NAME)
        );
    }

    #[test]
    fn output_path_adds_segment_pattern() {
        let dir = Path::new("out");
        assert_eq!(output_path(dir, "a_1", false), dir.join("a_1.ts"));
        assert_eq!(output_path(dir, "a_1", true), dir.join("a_1_%03d.ts"));
        assert_eq!(first_file_path(dir, "a_1", true), dir.join("a_1_000.ts"));
    }

    #[test]
    fn unique_stem_appends_counter_when_taken() {
        assert_eq!(unique_stem("a", |_| false), "a");
        let taken = ["a", "a_2"];
        assert_eq!(unique_stem("a", |stem| taken.contains(&stem)), "a_3");
    }

    #[test]
    fn is_segment_of_matches_numbered_files_only() {
        assert!(is_segment_of("主播_2026_000.ts", "主播_2026"));
        assert!(is_segment_of("主播_2026_1234.ts", "主播_2026"));
        assert!(!is_segment_of("主播_2026.ts", "主播_2026"));
        assert!(!is_segment_of("主播_2026_2_000.ts", "主播_2026"));
        assert!(!is_segment_of("主播_2026_00a.ts", "主播_2026"));
        assert!(!is_segment_of("主播_2026_000.mp4", "主播_2026"));
    }
}
