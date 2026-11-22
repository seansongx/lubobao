//! FFmpeg 命令行组装。
//!
//! 参照 StreamCap `app/core/media/ffmpeg_builders/base.py` 的 `DEFAULT_CONFIG` 与
//! `_get_basic_ffmpeg_command`，以及 `video/ts.py` 的 `TSCommandBuilder`：直接复制音视频流、
//! 输出 MPEG-TS，分段时使用 segment 封装器。与上游的差异见 `mod.rs` 模块文档。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use crate::platforms::SourceFormat;

/// 读写超时（微秒），StreamCap `DEFAULT_CONFIG["rw_timeout"]`。
pub const RW_TIMEOUT_US: &str = "15000000";
/// StreamCap `DEFAULT_CONFIG["analyzeduration"]`。
pub const ANALYZE_DURATION_US: &str = "20000000";
/// StreamCap `DEFAULT_CONFIG["probesize"]`。
pub const PROBE_SIZE: &str = "10000000";
/// StreamCap `DEFAULT_CONFIG["max_muxing_queue_size"]`。
pub const MAX_MUXING_QUEUE_SIZE: &str = "1024";
/// HLS 播放列表连续多少次刷新没有新分片后结束。FFmpeg 默认 1000 次，
/// 下播后会挂起很久；10 次约等于 10 个分片时长。
pub const HLS_HOLD_COUNTERS: &str = "10";
/// 与 StreamCap 相同的协议白名单。
pub const PROTOCOL_WHITELIST: &str = "rtmp,crypto,file,http,https,tcp,tls,udp,rtp,httpproxy";
/// 请求头里没有 User-Agent 时使用的默认值，StreamCap `FFMPEG_USER_AGENT`。
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 11; SAMSUNG SM-G973U) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/14.2 Chrome/87.0.4280.141 Mobile Safari/537.36";

/// 组装命令行所需的全部输入。
#[derive(Debug, Clone)]
pub struct CommandSpec<'a> {
    pub source_url: &'a str,
    pub source_format: SourceFormat,
    /// 取流请求头。`User-Agent` 和 `Referer` 分别转为 `-user_agent`、`-referer`，
    /// 其余放进 `-headers`。键不区分大小写。
    pub headers: &'a BTreeMap<String, String>,
    /// 输出路径；分段时须包含 `%03d`（见 [`super::naming::output_path`]）。
    pub output_path: &'a Path,
    /// 分段时长（秒）。`None` 表示不分段。
    pub segment_secs: Option<u64>,
    /// 已经过 [`ffmpeg_proxy`] 规范化的 HTTP 代理地址。
    pub proxy: Option<&'a str>,
}

/// 生成 ffmpeg 的参数列表（不含程序路径）。
pub fn build_args(spec: &CommandSpec<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::with_capacity(64);
    let mut push = |values: &[&str]| args.extend(values.iter().map(OsString::from));

    // 全局选项。StreamCap 同时写了 `-v verbose` 和 `-loglevel error`，后者生效；
    // 这里只保留 `error`，并显式开启统计行，供进度解析。
    push(&[
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-stats",
        "-stats_period",
        "1",
    ]);

    // 输入选项。
    if let Some(proxy) = spec.proxy {
        push(&["-http_proxy", proxy]);
    }
    push(&["-rw_timeout", RW_TIMEOUT_US]);
    let request = split_headers(spec.headers);
    push(&[
        "-user_agent",
        request.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT),
    ]);
    if let Some(referer) = request.referer.as_deref() {
        push(&["-referer", referer]);
    }
    if let Some(headers) = request.extra.as_deref() {
        push(&["-headers", headers]);
    }
    push(&[
        "-protocol_whitelist",
        PROTOCOL_WHITELIST,
        "-thread_queue_size",
        "1024",
        "-analyzeduration",
        ANALYZE_DURATION_US,
        "-probesize",
        PROBE_SIZE,
        "-fflags",
        "+discardcorrupt+igndts",
        "-correct_ts_overflow",
        "1",
    ]);
    if spec.source_format == SourceFormat::Hls {
        push(&["-m3u8_hold_counters", HLS_HOLD_COUNTERS]);
    }
    push(&["-i", spec.source_url]);

    // 输出选项：不重新编码，只保留音视频。
    push(&[
        "-map",
        "0",
        "-sn",
        "-dn",
        "-c:v",
        "copy",
        "-c:a",
        "copy",
        "-max_muxing_queue_size",
        MAX_MUXING_QUEUE_SIZE,
        "-avoid_negative_ts",
        "1",
        "-flush_packets",
        "1",
    ]);
    match spec.segment_secs {
        Some(secs) => {
            let secs = secs.max(1).to_string();
            push(&[
                "-f",
                "segment",
                "-segment_time",
                &secs,
                "-segment_format",
                "mpegts",
                "-reset_timestamps",
                "1",
            ]);
        }
        None => push(&["-f", "mpegts"]),
    }
    push(&[
        "-mpegts_flags",
        "+resend_headers",
        "-muxdelay",
        "0",
        "-muxpreload",
        "0",
    ]);
    args.push(spec.output_path.as_os_str().to_owned());
    args
}

/// 请求头按 FFmpeg 选项拆分后的结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RequestHeaders {
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    /// 其余请求头，格式为 `Key: Value\r\n` 逐行拼接。
    pub extra: Option<String>,
}

/// 拆分请求头，并去掉键值中的换行，防止注入额外的请求头。
pub fn split_headers(headers: &BTreeMap<String, String>) -> RequestHeaders {
    let mut result = RequestHeaders::default();
    let mut extra = String::new();
    for (key, value) in headers {
        let key = strip_line_breaks(key);
        let value = strip_line_breaks(value);
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || key.contains(':') || value.is_empty() {
            continue;
        }
        if key.eq_ignore_ascii_case("user-agent") {
            result.user_agent = Some(value.to_string());
        } else if key.eq_ignore_ascii_case("referer") {
            result.referer = Some(value.to_string());
        } else {
            extra.push_str(key);
            extra.push_str(": ");
            extra.push_str(value);
            extra.push_str("\r\n");
        }
    }
    if !extra.is_empty() {
        result.extra = Some(extra);
    }
    result
}

fn strip_line_breaks(text: &str) -> String {
    text.chars()
        .filter(|ch| *ch != '\r' && *ch != '\n')
        .collect()
}

/// 规范化代理地址供 `-http_proxy` 使用：没有协议时补 `http://`。
/// FFmpeg 的 HTTP 协议只支持 HTTP 代理，SOCKS 代理返回错误说明。
pub fn ffmpeg_proxy(proxy: &str) -> Result<Option<String>, String> {
    let proxy = proxy.trim();
    if proxy.is_empty() {
        return Ok(None);
    }
    match proxy.split_once("://") {
        None => Ok(Some(format!("http://{proxy}"))),
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("http") => Ok(Some(proxy.to_string())),
        Some((scheme, _)) => Err(format!(
            "FFmpeg 录制只支持 HTTP 代理，当前代理协议为 {scheme}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args_of(spec: &CommandSpec<'_>) -> Vec<String> {
        build_args(spec)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let index = args.iter().position(|arg| arg == flag)?;
        args.get(index + 1).map(String::as_str)
    }

    fn position(args: &[String], flag: &str) -> usize {
        args.iter()
            .position(|arg| arg == flag)
            .unwrap_or_else(|| panic!("missing {flag} in {args:?}"))
    }

    fn douyin_headers() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("User-Agent".to_string(), "Mozilla/5.0 Chrome".to_string()),
            (
                "Referer".to_string(),
                "https://live.douyin.com/".to_string(),
            ),
        ])
    }

    #[test]
    fn flv_command_copies_streams_into_mpegts() {
        let headers = douyin_headers();
        let output = PathBuf::from(r"D:\录像\抖音\主播\主播_2026-11-22_09-05-07.ts");
        let args = args_of(&CommandSpec {
            source_url: "https://pull.example.com/live/abc.flv?codec=h264",
            source_format: SourceFormat::Flv,
            headers: &headers,
            output_path: &output,
            segment_secs: None,
            proxy: None,
        });

        let input = position(&args, "-i");
        assert_eq!(
            args[input + 1],
            "https://pull.example.com/live/abc.flv?codec=h264"
        );
        assert_eq!(value_after(&args, "-c:v"), Some("copy"));
        assert_eq!(value_after(&args, "-c:a"), Some("copy"));
        assert_eq!(value_after(&args, "-map"), Some("0"));
        assert_eq!(value_after(&args, "-f"), Some("mpegts"));
        assert_eq!(value_after(&args, "-rw_timeout"), Some(RW_TIMEOUT_US));
        assert_eq!(value_after(&args, "-mpegts_flags"), Some("+resend_headers"));
        assert!(!args.iter().any(|arg| arg == "segment"));
        assert!(!args.iter().any(|arg| arg == "-m3u8_hold_counters"));
        assert!(!args.iter().any(|arg| arg.starts_with("-reconnect")));
        assert!(!args.iter().any(|arg| arg == "-re"));
        // 输入选项都在 -i 之前。
        for flag in ["-rw_timeout", "-user_agent", "-analyzeduration", "-fflags"] {
            assert!(position(&args, flag) < input, "{flag} must precede -i");
        }
        assert_eq!(args.last().unwrap(), &output.to_string_lossy());
    }

    #[test]
    fn hls_command_limits_playlist_hold() {
        let headers = BTreeMap::new();
        let output = PathBuf::from("out.ts");
        let args = args_of(&CommandSpec {
            source_url: "https://pull.example.com/live/abc.m3u8",
            source_format: SourceFormat::Hls,
            headers: &headers,
            output_path: &output,
            segment_secs: None,
            proxy: None,
        });
        let input = position(&args, "-i");
        assert!(position(&args, "-m3u8_hold_counters") < input);
        assert_eq!(
            value_after(&args, "-m3u8_hold_counters"),
            Some(HLS_HOLD_COUNTERS)
        );
        assert!(position(&args, "-protocol_whitelist") < input);
        assert_eq!(value_after(&args, "-user_agent"), Some(DEFAULT_USER_AGENT));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "-referer" || arg == "-headers")
        );
    }

    #[test]
    fn headers_map_to_user_agent_referer_and_extra_headers() {
        let mut headers = douyin_headers();
        headers.insert("Origin".to_string(), "https://live.douyin.com".to_string());
        headers.insert("Cookie".to_string(), "a=1\r\nX-Evil: 1".to_string());
        let output = PathBuf::from("out.ts");
        let args = args_of(&CommandSpec {
            source_url: "https://example.com/a.flv",
            source_format: SourceFormat::Flv,
            headers: &headers,
            output_path: &output,
            segment_secs: None,
            proxy: Some("http://127.0.0.1:7890"),
        });
        let input = position(&args, "-i");
        assert_eq!(
            value_after(&args, "-user_agent"),
            Some("Mozilla/5.0 Chrome")
        );
        assert_eq!(
            value_after(&args, "-referer"),
            Some("https://live.douyin.com/")
        );
        assert_eq!(
            value_after(&args, "-headers"),
            Some("Cookie: a=1X-Evil: 1\r\nOrigin: https://live.douyin.com\r\n")
        );
        assert_eq!(
            value_after(&args, "-http_proxy"),
            Some("http://127.0.0.1:7890")
        );
        for flag in ["-user_agent", "-referer", "-headers", "-http_proxy"] {
            assert!(position(&args, flag) < input, "{flag} must precede -i");
        }
    }

    #[test]
    fn header_keys_are_case_insensitive_and_empty_values_dropped() {
        let headers = BTreeMap::from([
            ("user-agent".to_string(), " UA ".to_string()),
            ("REFERER".to_string(), "".to_string()),
            ("Bad:Key".to_string(), "x".to_string()),
        ]);
        assert_eq!(
            split_headers(&headers),
            RequestHeaders {
                user_agent: Some("UA".to_string()),
                referer: None,
                extra: None,
            }
        );
    }

    #[test]
    fn segment_command_uses_segment_muxer() {
        let headers = douyin_headers();
        let output = PathBuf::from(r"D:\录像\主播_2026-11-22_09-05-07_%03d.ts");
        let args = args_of(&CommandSpec {
            source_url: "https://example.com/a.flv",
            source_format: SourceFormat::Flv,
            headers: &headers,
            output_path: &output,
            segment_secs: Some(1800),
            proxy: None,
        });
        assert_eq!(value_after(&args, "-f"), Some("segment"));
        assert_eq!(value_after(&args, "-segment_time"), Some("1800"));
        assert_eq!(value_after(&args, "-segment_format"), Some("mpegts"));
        assert_eq!(value_after(&args, "-reset_timestamps"), Some("1"));
        assert_eq!(args.last().unwrap(), &output.to_string_lossy());
    }

    #[test]
    fn segment_time_is_at_least_one_second() {
        let headers = BTreeMap::new();
        let output = PathBuf::from("a_%03d.ts");
        let args = args_of(&CommandSpec {
            source_url: "https://example.com/a.flv",
            source_format: SourceFormat::Flv,
            headers: &headers,
            output_path: &output,
            segment_secs: Some(0),
            proxy: None,
        });
        assert_eq!(value_after(&args, "-segment_time"), Some("1"));
    }

    #[test]
    fn proxy_is_normalized_for_ffmpeg() {
        assert_eq!(ffmpeg_proxy(""), Ok(None));
        assert_eq!(
            ffmpeg_proxy("127.0.0.1:7890"),
            Ok(Some("http://127.0.0.1:7890".to_string()))
        );
        assert_eq!(
            ffmpeg_proxy("HTTP://proxy:8080"),
            Ok(Some("HTTP://proxy:8080".to_string()))
        );
        assert!(ffmpeg_proxy("socks5://127.0.0.1:7891").is_err());
        assert!(ffmpeg_proxy("https://proxy:443").is_err());
    }
}
