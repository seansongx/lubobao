//! 录像目录与文件命名。
//!
//! 参照 StreamCap `StreamManager._get_filename`、`_get_output_dir`、`_get_save_path`
//! 和 `utils.clean_name`：文件名为「主播名_YYYY-MM-DD_HH-MM-SS」，分段时追加 `_%03d`；
//! 目录默认为「输出目录/平台/主播名」，可再加「日期」「标题_主播名」两层。全部是纯函数，
//! 便于单元测试。

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use super::RecordOptions;

/// 主播名为空时的默认名称，对应 StreamCap 语言包中的 `live_room`。
pub const DEFAULT_ANCHOR_NAME: &str = "直播间";

/// 平台名为空时的目录名。
pub const DEFAULT_PLATFORM_NAME: &str = "其他平台";

/// 清理后名称的最大字符数。StreamCap 不截断；这里限制长度，避免 Windows 路径过长。
pub const MAX_NAME_CHARS: usize = 60;

/// 标题（用于文件名和目录名）清理后的最大字符数。
pub const MAX_TITLE_CHARS: usize = 30;

/// 日期目录的格式。
pub const DATE_FORMAT: &str = "%Y-%m-%d";

/// 时间部分的格式，与 StreamCap 相同。
pub const TIME_FORMAT: &str = "%Y-%m-%d_%H-%M-%S";

/// ffmpeg 录制的扩展名。
pub const TS_EXTENSION: &str = "ts";

/// FLV 直接下载（不经 ffmpeg，见 [`super::direct`]）的扩展名。
pub const FLV_EXTENSION: &str = "flv";

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
/// - 路径分隔符、Windows 禁用字符、标点、空格和控制字符换成 `_`；
/// - `remove_emojis` 为 `true` 时 emoji 也换成 `_`，为 `false` 时原样保留（包括 ZWJ、
///   变体选择符、键帽符等组成部分；只由这些组成部分构成的名称视为空）；
/// - 连续的 `_` 合并为一个，去掉首尾的 `_`；
/// - 截断到 [`MAX_NAME_CHARS`] 个字符，避开 Windows 保留设备名。
///
/// 清理后为空时返回 `None`。中文等非 ASCII 字符原样保留。
pub fn clean_name(input: &str, remove_emojis: bool) -> Option<String> {
    let mut name = clean_limited(input, remove_emojis, MAX_NAME_CHARS)?;
    if RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(&name))
    {
        name.push('_');
    }
    Some(name)
}

/// 清理标题：规则同 [`clean_name`]，截到 [`MAX_TITLE_CHARS`] 个字符。标题只作为文件名或
/// 目录名的一部分，不会单独成名，所以不必避开保留设备名。清理后为空时返回 `None`。
pub fn clean_title(input: &str, remove_emojis: bool) -> Option<String> {
    clean_limited(input, remove_emojis, MAX_TITLE_CHARS)
}

fn clean_limited(input: &str, remove_emojis: bool, limit: usize) -> Option<String> {
    let mut cleaned = String::with_capacity(input.len());
    for ch in input.trim().chars() {
        let ch = match ch {
            '（' => '(',
            '）' => ')',
            other => other,
        };
        if REPLACED_CHARS.contains(&ch)
            || ch.is_control()
            || ch.is_whitespace()
            || (remove_emojis && is_emoji(ch))
        {
            if !cleaned.ends_with('_') {
                cleaned.push('_');
            }
        } else {
            cleaned.push(ch);
        }
    }

    let truncated: String = cleaned.trim_matches('_').chars().take(limit).collect();
    let name = truncated.trim_matches('_').to_string();
    // 保留 emoji 时，只剩 ZWJ、变体选择符之类不可见的组成部分，等于什么都没有。
    if name.chars().all(is_emoji_component) {
        return None;
    }
    Some(name)
}

/// 主播名清理后的结果，为空时用 [`DEFAULT_ANCHOR_NAME`]。
pub fn anchor_label(anchor_name: &str, remove_emojis: bool) -> String {
    clean_name(anchor_name, remove_emojis).unwrap_or_else(|| DEFAULT_ANCHOR_NAME.to_string())
}

/// 平台名清理后的结果，为空时用 [`DEFAULT_PLATFORM_NAME`]。
pub fn platform_label(platform: &str, remove_emojis: bool) -> String {
    clean_name(platform, remove_emojis).unwrap_or_else(|| DEFAULT_PLATFORM_NAME.to_string())
}

/// 一次录制用于命名的信息。
#[derive(Debug, Clone, Copy)]
pub struct NameParts<'a> {
    pub platform: &'a str,
    pub anchor_name: &'a str,
    /// 直播标题；`None` 或清理后为空时，开关打开也不会出现在名称里。
    pub title: Option<&'a str>,
    /// 开始录制的本地时间。
    pub started: NaiveDateTime,
}

impl NameParts<'_> {
    fn title_label(&self, options: &RecordOptions) -> Option<String> {
        clean_title(self.title?, options.remove_emojis)
    }
}

/// 文件名主体：`主播名_YYYY-MM-DD_HH-MM-SS`，不含扩展名和分段序号。
/// `filename_includes_title` 开启且标题清理后非空时为 `主播名_标题_YYYY-MM-DD_HH-MM-SS`。
pub fn file_stem(parts: &NameParts, options: &RecordOptions) -> String {
    let anchor = anchor_label(parts.anchor_name, options.remove_emojis);
    let time = parts.started.format(TIME_FORMAT);
    let title = if options.filename_includes_title {
        parts.title_label(options)
    } else {
        None
    };
    match title {
        Some(title) => format!("{anchor}_{title}_{time}"),
        None => format!("{anchor}_{time}"),
    }
}

/// 录像目录：保存目录 / 平台 / 主播 / 开始当天的日期 / `标题_主播名`，各层由
/// [`RecordOptions`] 的开关独立控制（StreamCap 默认只开平台和主播两层）。
/// 标题层只在标题清理后非空时出现。最多四层。
pub fn recording_dir(output_root: &Path, parts: &NameParts, options: &RecordOptions) -> PathBuf {
    let anchor = anchor_label(parts.anchor_name, options.remove_emojis);
    let mut dir = output_root.to_path_buf();
    if options.folder_by_platform {
        dir.push(platform_label(parts.platform, options.remove_emojis));
    }
    if options.folder_by_anchor {
        dir.push(&anchor);
    }
    if options.folder_by_date {
        dir.push(parts.started.format(DATE_FORMAT).to_string());
    }
    if options.folder_by_title
        && let Some(title) = parts.title_label(options)
    {
        dir.push(format!("{title}_{anchor}"));
    }
    dir
}

/// 录像的输出路径，`extension` 为 [`TS_EXTENSION`] 或 [`FLV_EXTENSION`]。分段时为
/// `stem_%03d.ts`，由 segment 封装器编号（只有 ffmpeg 录制会分段）。
pub fn output_path(dir: &Path, stem: &str, segmented: bool, extension: &str) -> PathBuf {
    if segmented {
        dir.join(format!("{stem}_%03d.{extension}"))
    } else {
        dir.join(format!("{stem}.{extension}"))
    }
}

/// 分段录制时实际交给 ffmpeg 的输出参数。segment 封装器把整条路径当作格式串，
/// 目录里的 `%` 必须写成 `%%`，否则 `D:@% 录像` 这样的目录会让 ffmpeg 报错。
/// 文件名部分已经由 [`clean_name`] 去掉了 `%`。非 UTF-8 路径无法安全改写，原样返回。
pub fn ffmpeg_segment_pattern(dir: &Path, stem: &str) -> PathBuf {
    let name = format!("{stem}_%03d.{TS_EXTENSION}");
    match dir.to_str() {
        Some(text) if text.contains('%') => PathBuf::from(text.replace('%', "%%")).join(name),
        _ => dir.join(name),
    }
}

/// 第一个文件的实际路径，用于判断是否与已有录像重名。
pub fn first_file_path(dir: &Path, stem: &str, segmented: bool, extension: &str) -> PathBuf {
    if segmented {
        dir.join(format!("{stem}_000.{extension}"))
    } else {
        dir.join(format!("{stem}.{extension}"))
    }
}

/// 把 [`output_path`] 给出的分段模板 `stem_%03d.ts` 换成第一个分段 `stem_000.ts`，给界面
/// 和日志显示实际文件。只改文件名部分；不是模板的路径原样返回。
pub fn first_segment_path(path: &Path) -> PathBuf {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) if name.contains("_%03d.") => {
            path.with_file_name(name.replacen("_%03d.", "_000.", 1))
        }
        _ => path.to_path_buf(),
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

/// 解析分段文件名 `stem_NNN.ts` 中的序号；不属于该次录制时返回 `None`。
/// 超过 999 段后序号变成 4 位以上，按数字排序才不会乱。
pub fn segment_index(file_name: &str, stem: &str) -> Option<u64> {
    let rest = file_name.strip_prefix(stem)?.strip_prefix('_')?;
    let index = rest.strip_suffix(&format!(".{TS_EXTENSION}"))?;
    if index.len() < 3 || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    index.parse().ok()
}

/// 判断目录中的文件名是否属于某次分段录制：`stem_NNN.ts`。
pub fn is_segment_of(file_name: &str, stem: &str) -> bool {
    segment_index(file_name, stem).is_some()
}

/// 常见 emoji 所在的码位区间。StreamCap 用 `EMOJI_PATTERN` 正则移除 emoji，这里按区间近似。
fn is_emoji(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF
    ) || is_emoji_component(ch)
}

/// emoji 序列里本身不显示的组成部分：变体选择符、ZWJ、键帽符、标签字符。
fn is_emoji_component(ch: char) -> bool {
    matches!(
        ch as u32,
        0xFE00..=0xFE0F | 0x200D | 0x20E3 | 0xE0020..=0xE007F
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
            clean_name(r#"a/b\c:d*e?f"g<h>i|j"#, true).as_deref(),
            Some("a_b_c_d_e_f_g_h_i_j")
        );
    }

    #[test]
    fn clean_name_follows_streamcap_punctuation_rules() {
        assert_eq!(
            clean_name("  主播：小明。今天，直播！~ ", true).as_deref(),
            Some("主播_小明_今天_直播")
        );
        assert_eq!(clean_name("（官方）号", true).as_deref(), Some("(官方)号"));
        assert_eq!(clean_name("a&b#c.d·e", true).as_deref(), Some("a_b_c_d_e"));
    }

    #[test]
    fn clean_name_removes_emoji_and_control_characters() {
        assert_eq!(clean_name("小🐟儿🎉", true).as_deref(), Some("小_儿"));
        assert_eq!(
            clean_name("a\tb\nc\u{7}d", true).as_deref(),
            Some("a_b_c_d")
        );
        assert_eq!(clean_name("❤️love", true).as_deref(), Some("love"));
    }

    #[test]
    fn clean_name_escapes_percent_for_segment_patterns() {
        assert_eq!(clean_name("100%真实", true).as_deref(), Some("100_真实"));
    }

    #[test]
    fn clean_name_returns_none_when_nothing_is_left() {
        assert_eq!(clean_name("", true), None);
        assert_eq!(clean_name("  ", true), None);
        assert_eq!(clean_name("..._?/", true), None);
        assert_eq!(clean_name("🎉🎉", true), None);
    }

    #[test]
    fn clean_name_avoids_windows_reserved_names() {
        assert_eq!(clean_name("con", true).as_deref(), Some("con_"));
        assert_eq!(clean_name("LPT1", true).as_deref(), Some("LPT1_"));
        assert_eq!(clean_name("console", true).as_deref(), Some("console"));
    }

    #[test]
    fn clean_name_truncates_long_names() {
        let long = "长".repeat(100);
        assert_eq!(
            clean_name(&long, true).unwrap().chars().count(),
            MAX_NAME_CHARS
        );
    }

    fn parts<'a>(anchor: &'a str, title: Option<&'a str>) -> NameParts<'a> {
        NameParts {
            platform: "抖音",
            anchor_name: anchor,
            title,
            started: sample_time(),
        }
    }

    #[test]
    fn clean_name_keeps_emoji_when_removal_is_off() {
        assert_eq!(clean_name("小🐟儿🎉", false).as_deref(), Some("小🐟儿🎉"));
        assert_eq!(clean_name("❤️love", false).as_deref(), Some("❤️love"));
        // ZWJ 序列、键帽符和旗帜的组成部分都原样保留。
        assert_eq!(
            clean_name("👨\u{200D}👩\u{200D}👧 家", false).as_deref(),
            Some("👨\u{200D}👩\u{200D}👧_家")
        );
        assert_eq!(
            clean_name("1\u{FE0F}\u{20E3}号", false).as_deref(),
            Some("1\u{FE0F}\u{20E3}号")
        );
        assert_eq!(clean_name("🇨🇳中国", false).as_deref(), Some("🇨🇳中国"));
        // 其余规则不受开关影响。
        assert_eq!(clean_name("🎉 a/b", false).as_deref(), Some("🎉_a_b"));
        assert_eq!(clean_name("🎉🎉", false).as_deref(), Some("🎉🎉"));
    }

    #[test]
    fn clean_name_drops_names_made_only_of_invisible_emoji_parts() {
        assert_eq!(clean_name("\u{FE0F}\u{200D}", false), None);
        assert_eq!(clean_name(" \u{200D} ", false), None);
        // 删除 emoji 时这些组成部分一并换掉。
        assert_eq!(clean_name("a\u{200D}b", true).as_deref(), Some("a_b"));
    }

    #[test]
    fn clean_title_truncates_to_thirty_characters() {
        let long = "长".repeat(100);
        assert_eq!(
            clean_title(&long, true).unwrap().chars().count(),
            MAX_TITLE_CHARS
        );
        // 截断处正好是分隔符时不留尾部下划线。
        let title = format!("{} 后面的内容", "标".repeat(29));
        assert_eq!(clean_title(&title, true), Some("标".repeat(29)));
    }

    #[test]
    fn clean_title_returns_none_when_everything_is_replaced() {
        assert_eq!(clean_title("", true), None);
        assert_eq!(clean_title("？？ / ...", true), None);
        assert_eq!(clean_title("🎉🎉", true), None);
    }

    #[test]
    fn file_stem_uses_anchor_and_local_time() {
        let options = RecordOptions::default();
        assert_eq!(
            file_stem(&parts("测试 主播", None), &options),
            "测试_主播_2026-11-22_09-05-07"
        );
        assert_eq!(
            file_stem(&parts("  ", None), &options),
            "直播间_2026-11-22_09-05-07"
        );
    }

    #[test]
    fn file_stem_ignores_title_unless_enabled() {
        let options = RecordOptions::default();
        assert_eq!(
            file_stem(&parts("小明", Some("今晚唱歌")), &options),
            "小明_2026-11-22_09-05-07"
        );
    }

    #[test]
    fn file_stem_includes_cleaned_title_between_anchor_and_time() {
        let options = RecordOptions {
            filename_includes_title: true,
            ..RecordOptions::default()
        };
        assert_eq!(
            file_stem(&parts("小明", Some("今晚 唱歌：100%🎉")), &options),
            "小明_今晚_唱歌_100_2026-11-22_09-05-07"
        );
        let long = "题".repeat(50);
        assert_eq!(
            file_stem(&parts("小明", Some(&long)), &options),
            format!("小明_{}_2026-11-22_09-05-07", "题".repeat(30))
        );
    }

    #[test]
    fn file_stem_omits_title_when_missing_or_fully_replaced() {
        let options = RecordOptions {
            filename_includes_title: true,
            ..RecordOptions::default()
        };
        for title in [None, Some(""), Some("   "), Some("？？/"), Some("🎉")] {
            assert_eq!(
                file_stem(&parts("小明", title), &options),
                "小明_2026-11-22_09-05-07",
                "{title:?}"
            );
        }
    }

    #[test]
    fn file_stem_keeps_emoji_in_anchor_and_title_when_removal_is_off() {
        let options = RecordOptions {
            filename_includes_title: true,
            remove_emojis: false,
            ..RecordOptions::default()
        };
        assert_eq!(
            file_stem(&parts("小明🎮", Some("开播🎉")), &options),
            "小明🎮_开播🎉_2026-11-22_09-05-07"
        );
        // 关闭删除后，只有 emoji 的标题也算有内容。
        assert_eq!(
            file_stem(&parts("小明", Some("🎉")), &options),
            "小明_🎉_2026-11-22_09-05-07"
        );
    }

    #[test]
    fn recording_dir_groups_by_platform_and_anchor() {
        let root = Path::new(r"D:\录像");
        let options = RecordOptions::default();
        assert_eq!(
            recording_dir(root, &parts("小明/Official", None), &options),
            root.join("抖音").join("小明_Official")
        );
        let flat = RecordOptions {
            folder_by_platform: false,
            folder_by_anchor: false,
            ..RecordOptions::default()
        };
        assert_eq!(recording_dir(root, &parts("小明", None), &flat), root);
        let empty = NameParts {
            platform: "",
            ..parts("", None)
        };
        assert_eq!(
            recording_dir(root, &empty, &options),
            root.join(DEFAULT_PLATFORM_NAME).join(DEFAULT_ANCHOR_NAME)
        );
    }

    #[test]
    fn recording_dir_adds_the_start_date_layer_when_enabled() {
        let root = Path::new("out");
        let options = RecordOptions {
            folder_by_date: true,
            ..RecordOptions::default()
        };
        assert_eq!(
            recording_dir(root, &parts("小明", Some("标题")), &options),
            root.join("抖音").join("小明").join("2026-11-22")
        );
    }

    #[test]
    fn recording_dir_adds_the_title_layer_after_the_date() {
        let root = Path::new("out");
        let options = RecordOptions {
            folder_by_date: true,
            folder_by_title: true,
            ..RecordOptions::default()
        };
        assert_eq!(
            recording_dir(root, &parts("小明", Some("今晚 唱歌")), &options),
            root.join("抖音")
                .join("小明")
                .join("2026-11-22")
                .join("今晚_唱歌_小明")
        );
        // 只开标题层：不带日期层。
        let title_only = RecordOptions {
            folder_by_title: true,
            ..RecordOptions::default()
        };
        assert_eq!(
            recording_dir(root, &parts("小明", Some("唱歌")), &title_only),
            root.join("抖音").join("小明").join("唱歌_小明")
        );
    }

    #[test]
    fn recording_dir_skips_the_title_layer_without_a_usable_title() {
        let root = Path::new("out");
        let options = RecordOptions {
            folder_by_title: true,
            ..RecordOptions::default()
        };
        for title in [None, Some(""), Some("？？"), Some("🎉")] {
            assert_eq!(
                recording_dir(root, &parts("小明", title), &options),
                root.join("抖音").join("小明"),
                "{title:?}"
            );
        }
    }

    #[test]
    fn recording_dir_layers_are_independent_of_platform_and_anchor_switches() {
        let root = Path::new("out");
        let options = RecordOptions {
            folder_by_platform: false,
            folder_by_anchor: false,
            folder_by_date: true,
            folder_by_title: true,
            ..RecordOptions::default()
        };
        assert_eq!(
            recording_dir(root, &parts("小明", Some("唱歌")), &options),
            root.join("2026-11-22").join("唱歌_小明")
        );
    }

    #[test]
    fn recording_dir_names_follow_the_emoji_switch() {
        let root = Path::new("out");
        let keep = RecordOptions {
            remove_emojis: false,
            folder_by_title: true,
            ..RecordOptions::default()
        };
        let named = NameParts {
            platform: "抖音🎵",
            ..parts("小明🎮", Some("开播🎉"))
        };
        assert_eq!(
            recording_dir(root, &named, &keep),
            root.join("抖音🎵").join("小明🎮").join("开播🎉_小明🎮")
        );
        let drop = RecordOptions {
            remove_emojis: true,
            ..keep
        };
        assert_eq!(
            recording_dir(root, &named, &drop),
            root.join("抖音").join("小明").join("开播_小明")
        );
    }

    #[test]
    fn output_path_adds_segment_pattern() {
        let dir = Path::new("out");
        assert_eq!(
            output_path(dir, "a_1", false, TS_EXTENSION),
            dir.join("a_1.ts")
        );
        assert_eq!(
            output_path(dir, "a_1", true, TS_EXTENSION),
            dir.join("a_1_%03d.ts")
        );
        assert_eq!(
            first_file_path(dir, "a_1", true, TS_EXTENSION),
            dir.join("a_1_000.ts")
        );
    }

    #[test]
    fn direct_download_files_use_the_same_name_with_flv_extension() {
        let dir = Path::new("out");
        assert_eq!(
            output_path(dir, "a_1", false, FLV_EXTENSION),
            dir.join("a_1.flv")
        );
        assert_eq!(
            first_file_path(dir, "a_1", false, FLV_EXTENSION),
            dir.join("a_1.flv")
        );
    }

    #[test]
    fn first_segment_path_expands_only_the_file_name() {
        let dir = Path::new(r"D:\100% 录像\a_%03d.x");
        assert_eq!(
            first_segment_path(&output_path(dir, "主播_1", true, TS_EXTENSION)),
            dir.join("主播_1_000.ts")
        );
        let plain = output_path(dir, "主播_1", false, TS_EXTENSION);
        assert_eq!(first_segment_path(&plain), plain);
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

    #[test]
    fn segment_index_sorts_numerically_past_999() {
        let stem = "主播_2026";
        let mut names = vec!["主播_2026_1000.ts", "主播_2026_999.ts", "主播_2026_002.ts"];
        names.sort_by_key(|name| segment_index(name, stem));
        assert_eq!(
            names,
            vec!["主播_2026_002.ts", "主播_2026_999.ts", "主播_2026_1000.ts"]
        );
        assert_eq!(segment_index("主播_2026_1000.ts", stem), Some(1000));
        assert_eq!(segment_index("主播_2026_01.ts", stem), None);
    }

    #[test]
    fn segment_pattern_escapes_percent_in_directory() {
        let dir = Path::new(r"D:\100% 录像\rec%d");
        assert_eq!(
            ffmpeg_segment_pattern(dir, "主播_1"),
            PathBuf::from(r"D:\100%% 录像\rec%%d").join("主播_1_%03d.ts")
        );
        let plain = Path::new(r"D:\录像");
        assert_eq!(
            ffmpeg_segment_pattern(plain, "a"),
            output_path(plain, "a", true, TS_EXTENSION)
        );
    }
}
