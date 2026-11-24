//! YouTube adapter, following StreamCap's `YoutubeHandler` and StreamGet
//! v4.0.10 `YoutubeLiveStream`, with the stream taken from yt-dlp.
//!
//! Ported in part from StreamGet (https://github.com/ihmily/streamget,
//! v4.0.10), `streamget/platforms/youtube/live_stream.py`, MIT License,
//! Copyright (c) 2025 Hmily. Reading `ytInitialPlayerResponse` from the page,
//! the `videoDetails.isLive` check and choosing a variant by position after
//! sorting best first follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * The page of the link is requested: `watch?v={id}` for a video, or
//!   `/@{handle}/live` (`/channel/{id}/live`, …) for a channel. While the
//!   channel is live, YouTube serves the watch page of the live video there;
//!   otherwise it serves the channel page, which has no player response and
//!   counts as offline. An unknown channel answers 404. This needs no
//!   component, so offline channels cost one page request per check.
//! * When the page shows a live video, `yt-dlp -j` (with Deno for YouTube's
//!   JS challenges, see [`crate::ytdlp`]) gives the HLS formats; StreamGet
//!   records the page's `hlsManifestUrl`, whose segments now answer 403
//!   until the challenge is solved. The components are downloaded on first
//!   use; until then the check answers [`PlatformError::Preparing`].
//! * YouTube asks some IP ranges (often proxy and data-centre addresses) to
//!   sign in before it plays live streams ("Sign in to confirm you're not a
//!   bot"). Without a cookie that is reported as needing login; with one the
//!   link goes to yt-dlp, which sends the cookie from a temporary file.
//!
//! YouTube is not reachable from mainland China; the proxy in `FetchOptions`
//! is used for the page, the component download, yt-dlp and the recording.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};
use crate::ytdlp::ToolPaths;

/// `BaseLiveStream._get_pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36 Edg/121.0.0.0";
/// English pages, so the playability reasons can be told apart.
const ACCEPT_LANGUAGE_VALUE: &str = "en-US,en;q=0.9";
/// Cookie sent without a user cookie: declines the EU consent page the way
/// yt-dlp does, so proxies in Europe still get the real page.
const DEFAULT_COOKIE: &str = "SOCS=CAI";
const SITE: &str = "https://www.youtube.com";

/// A recognized YouTube link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YoutubeLink {
    /// One video (`watch?v=`, `youtu.be/`, `/live/{id}`), live or not.
    Video { id: String },
    /// A channel's live page, which follows the channel's current stream.
    /// `path` is `@handle`, `channel/{UC…}`, `c/{name}` or `user/{name}`.
    Channel { path: String },
}

impl YoutubeLink {
    /// The page to request, also the canonical URL for duplicate detection.
    pub fn normalized_url(&self) -> String {
        match self {
            YoutubeLink::Video { id } => format!("{SITE}/watch?v={id}"),
            YoutubeLink::Channel { path } => format!("{SITE}/{path}/live"),
        }
    }

    /// Room id: the video id, or the channel path without its kind
    /// (`@handle`, the `UC…` id, the custom name).
    pub fn room_id(&self) -> String {
        match self {
            YoutubeLink::Video { id } => id.clone(),
            YoutubeLink::Channel { path } => match path.split_once('/') {
                Some((_, name)) => name.to_string(),
                None => path.clone(),
            },
        }
    }
}

fn is_video_id(text: &str) -> bool {
    text.len() == 11
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Handles, channel ids and custom names: no separators or spaces.
fn is_channel_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 100
        && !text
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '&'))
}

/// Recognize a YouTube link without any network access.
///
/// StreamCap registers `.*\.youtube\.com/` and StreamGet requests the link as
/// given. Channel live pages (`/@{handle}/live`, `/channel/{id}/live`, also
/// without `/live`), video links (`watch?v=`, `/live/{id}`, `youtu.be/{id}`)
/// and the mobile site are accepted and normalized.
pub fn parse_link(input: &str) -> Result<YoutubeLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的 YouTube 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    let segments: Vec<String> = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .map(|segment| percent_decode(segment).unwrap_or_else(|| segment.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let video = |id: &str| {
        is_video_id(id)
            .then(|| YoutubeLink::Video { id: id.to_string() })
            .ok_or_else(invalid)
    };
    if host == "youtu.be" {
        return video(segments.first().map(String::as_str).unwrap_or_default());
    }
    if host != "youtube.com" && !host.ends_with(".youtube.com") {
        return Err(invalid());
    }
    let first = segments.first().map(String::as_str).unwrap_or_default();
    let second = segments.get(1).map(String::as_str).unwrap_or_default();
    match first {
        "watch" => {
            let id = url
                .query_pairs()
                .find(|(key, _)| key == "v")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            video(&id)
        }
        "live" | "embed" => video(second),
        "channel" | "c" | "user" if is_channel_name(second) => Ok(YoutubeLink::Channel {
            path: format!("{first}/{second}"),
        }),
        handle if handle.starts_with('@') && is_channel_name(&handle[1..]) => {
            Ok(YoutubeLink::Channel {
                path: handle.to_lowercase(),
            })
        }
        _ => Err(PlatformError::InvalidUrl(format!(
            "YouTube 链接里没有频道或直播视频，请复制频道的 /@名称/live 链接或直播视频链接: {trimmed}"
        ))),
    }
}

/// Decode `%XX` escapes (handles can be non-ASCII). `None` when the result
/// is not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = text.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The JSON value assigned to `var {name} = ` (or `window["{name}"] = `) in
/// the page.
pub fn initial_json(html: &str, name: &str) -> Option<Value> {
    let markers = [format!("var {name} = "), format!("window[\"{name}\"] = ")];
    let start = markers
        .iter()
        .find_map(|marker| html.find(marker.as_str()).map(|at| at + marker.len()))?;
    serde_json::Deserializer::from_str(&html[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// What the page says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInfo {
    pub channel_id: Option<String>,
    pub author: Option<String>,
    pub video_id: Option<String>,
    pub title: Option<String>,
    /// `videoDetails.isLive`.
    pub is_live: bool,
}

/// Parse the page of a link.
pub fn parse_page(html: &str, link: &YoutubeLink) -> Result<PageInfo, PlatformError> {
    let Some(player) = initial_json(html, "ytInitialPlayerResponse") else {
        // A channel that is not live: its live page is the channel page.
        let metadata = initial_json(html, "ytInitialData").and_then(|data| {
            data.get("metadata")
                .and_then(|metadata| metadata.get("channelMetadataRenderer"))
                .cloned()
        });
        return match (link, metadata) {
            (YoutubeLink::Channel { .. }, Some(metadata)) => Ok(PageInfo {
                channel_id: text(metadata.get("externalId")),
                author: text(metadata.get("title")),
                video_id: None,
                title: None,
                is_live: false,
            }),
            _ if html.contains("consent.youtube.com") => Err(PlatformError::Config(
                "YouTube 返回了 Cookie 同意页，请在设置里填写 YouTube Cookie 或换其他地区的代理节点".to_string(),
            )),
            _ => Err(PlatformError::Parse(
                "YouTube 页面没有播放器数据，无法判断是否开播".to_string(),
            )),
        };
    };
    let playability = player.get("playabilityStatus");
    let status = text(playability.and_then(|status| status.get("status"))).unwrap_or_default();
    let reason = text(playability.and_then(|status| status.get("reason"))).unwrap_or_default();
    let details = player
        .get("videoDetails")
        .filter(|details| details.is_object());
    let Some(details) = details else {
        return Err(match status.as_str() {
            "LOGIN_REQUIRED" if reason.contains("bot") => PlatformError::AuthRequired(
                "YouTube 要求登录确认不是机器人（代理节点的 IP 被 YouTube 风控）：请在设置里填写登录后的 YouTube Cookie，或换一个代理节点".to_string(),
            ),
            "LOGIN_REQUIRED" => PlatformError::AuthRequired(format!(
                "YouTube 要求登录后观看：{reason}，请在设置里填写 YouTube Cookie"
            )),
            "ERROR" | "UNPLAYABLE" => PlatformError::InvalidUrl(format!(
                "YouTube 视频不可用：{reason}"
            )),
            _ => PlatformError::Parse(format!(
                "YouTube 页面缺少视频信息（{status} {reason}）"
            )),
        });
    };
    let is_live = details.get("isLive").and_then(Value::as_bool) == Some(true);
    if is_live && status == "LOGIN_REQUIRED" {
        return Err(PlatformError::AuthRequired(format!(
            "YouTube 要求登录后观看这场直播：{reason}，请在设置里填写 YouTube Cookie"
        )));
    }
    Ok(PageInfo {
        channel_id: text(details.get("channelId")),
        author: text(details.get("author")),
        video_id: text(details.get("videoId")),
        title: text(details.get("title")),
        is_live,
    })
}

/// `LiveInfo` for a link that is not live, named from the page.
pub fn offline_info(link: &YoutubeLink, page: Option<&PageInfo>) -> LiveInfo {
    let room_id = link.room_id();
    LiveInfo {
        platform: Platform::Youtube,
        anchor_name: page
            .and_then(|page| page.author.clone())
            .unwrap_or_else(|| room_id.clone()),
        room_id,
        anchor_id: page.and_then(|page| page.channel_id.clone()),
        live_url: Some(link.normalized_url()),
        title: None,
        is_live: false,
        variants: Vec::new(),
        selected: None,
        stream_headers: stream_headers(),
    }
}

/// One recordable format from `yt-dlp -j`.
#[derive(Debug, Clone, PartialEq)]
struct Format {
    name: String,
    url: String,
    height: u64,
    tbr: f64,
    headers: BTreeMap<String, String>,
}

fn formats(json: &Value) -> Vec<Format> {
    let mut list: Vec<Format> = json
        .get("formats")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|format| {
            let protocol = format.get("protocol").and_then(Value::as_str)?;
            let has = |key: &str| {
                format
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|codec| codec != "none")
            };
            if !protocol.starts_with("m3u8") || !has("vcodec") || !has("acodec") {
                return None;
            }
            let url = text(format.get("url"))?;
            let height = format.get("height").and_then(Value::as_u64).unwrap_or(0);
            let fps = format.get("fps").and_then(Value::as_f64).unwrap_or(0.0);
            let name = match (height, fps) {
                (0, _) => text(format.get("format_id")).unwrap_or_else(|| "HLS".to_string()),
                (height, fps) if fps > 30.5 => format!("{height}p{}", fps.round() as u64),
                (height, _) => format!("{height}p"),
            };
            let headers = format
                .get("http_headers")
                .and_then(Value::as_object)
                .map(|headers| {
                    headers
                        .iter()
                        .filter(|(key, _)| !key.eq_ignore_ascii_case("cookie"))
                        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Some(Format {
                name,
                url,
                height,
                tbr: format.get("tbr").and_then(Value::as_f64).unwrap_or(0.0),
                headers,
            })
        })
        .collect();
    list.sort_by(|a, b| {
        b.height.cmp(&a.height).then(
            b.tbr
                .partial_cmp(&a.tbr)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    list
}

/// Turn `yt-dlp -j` output into `LiveInfo`: the HLS formats with audio and
/// video, highest resolution first, map to the qualities by position (StreamGet
/// sorts the master playlist by bandwidth). The selected format's
/// `http_headers` (never cookies) go to FFmpeg.
pub fn parse_ytdlp(
    json: &Value,
    link: &YoutubeLink,
    page: Option<&PageInfo>,
    quality: Quality,
) -> Result<LiveInfo, PlatformError> {
    let is_live = json.get("is_live").and_then(Value::as_bool) == Some(true)
        || json.get("live_status").and_then(Value::as_str) == Some("is_live");
    let mut info = offline_info(link, page);
    if let Some(author) = text(json.get("channel")).or_else(|| text(json.get("uploader"))) {
        info.anchor_name = author;
    }
    if let Some(channel_id) = text(json.get("channel_id")) {
        info.anchor_id = Some(channel_id);
    }
    if !is_live {
        return Ok(info);
    }
    let formats = formats(json);
    if formats.is_empty() {
        return Err(PlatformError::Parse(
            "YouTube 正在直播，但 yt-dlp 没有给出可录制的 HLS 流，YouTube 组件可能需要更新"
                .to_string(),
        ));
    }
    let variants: Vec<StreamVariant> = formats
        .iter()
        .enumerate()
        .map(|(index, format)| StreamVariant {
            name: format.name.clone(),
            quality: Quality::from_index(index),
            flv_url: None,
            hls_url: Some(format.url.clone()),
        })
        .collect();
    let index = select_variant_index(variants.len(), quality).unwrap_or(0);
    info.selected = selected_stream(quality, &variants[index]);
    if !formats[index].headers.is_empty() {
        info.stream_headers = formats[index].headers.clone();
    }
    info.title = text(json.get("fulltitle")).or_else(|| text(json.get("title")));
    info.is_live = info.selected.is_some();
    info.variants = variants;
    Ok(info)
}

/// What a failed `yt-dlp` run means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YtdlpFailure {
    /// The link is not live (upcoming, ended, channel not live).
    Offline,
    Error(PlatformError),
}

/// Replace every URL with its scheme and host, so signed stream parameters
/// never reach messages or logs.
pub fn strip_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("http") {
        let candidate = &rest[at..];
        let scheme_len = if candidate.starts_with("https://") {
            8
        } else if candidate.starts_with("http://") {
            7
        } else {
            out.push_str(&rest[..at + 4]);
            rest = &rest[at + 4..];
            continue;
        };
        out.push_str(&rest[..at]);
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>'))
            .unwrap_or(candidate.len());
        let url = &candidate[..end];
        let host_end = url[scheme_len..]
            .find(['/', '?', '#'])
            .map_or(url.len(), |index| index + scheme_len);
        out.push_str(&url[..host_end]);
        if host_end < url.len() {
            out.push_str("/…");
        }
        rest = &candidate[end..];
    }
    out.push_str(rest);
    out
}

/// Map yt-dlp's error output to a result.
pub fn classify_ytdlp_error(stderr: &str, with_cookie: bool) -> YtdlpFailure {
    let line = stderr
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("ERROR:"))
        .or_else(|| stderr.lines().rev().find(|line| !line.trim().is_empty()))
        .unwrap_or_default()
        .trim();
    let mut message = line.trim_start_matches("ERROR:").trim();
    // `[youtube] id: …` / `[youtube:tab] @name: …`
    if message.starts_with('[')
        && let Some((_, rest)) = message.split_once(']')
    {
        message = rest.trim();
        if let Some((_, rest)) = message.split_once(": ") {
            message = rest.trim();
        }
    }
    let mut message = strip_urls(message);
    if message.chars().count() > 300 {
        message = message.chars().take(300).collect::<String>() + "…";
    }
    let lower = message.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    if any(&[
        "not currently live",
        "will begin in",
        "premieres in",
        "live event will begin",
        "recording is not available",
        "this live event has ended",
    ]) {
        return YtdlpFailure::Offline;
    }
    let error = if lower.contains("sign in to confirm") && lower.contains("bot") {
        PlatformError::AuthRequired(if with_cookie {
            "YouTube 要求登录确认不是机器人：设置里的 YouTube Cookie 可能已经失效，请重新复制登录后的 Cookie，或换一个代理节点".to_string()
        } else {
            "YouTube 要求登录确认不是机器人（代理节点的 IP 被 YouTube 风控）：请在设置里填写登录后的 YouTube Cookie，或换一个代理节点".to_string()
        })
    } else if any(&[
        "sign in to confirm your age",
        "members-only",
        "join this channel",
        "private video",
        "requires payment",
        "cookies are no longer valid",
        "login required",
    ]) {
        PlatformError::AuthRequired(format!(
            "YouTube 要求登录后观看：{message}，请在设置里填写 YouTube Cookie"
        ))
    } else if any(&[
        "video unavailable",
        "does not exist",
        "http error 404",
        "unable to recognize",
        "unsupported url",
    ]) {
        PlatformError::InvalidUrl(format!("YouTube 视频或频道不可用：{message}"))
    } else if any(&["http error 429", "too many requests"]) {
        PlatformError::RateLimited(format!("YouTube 限制了访问：{message}"))
    } else if any(&[
        "timed out",
        "unable to download",
        "connection",
        "proxy",
        "ssl",
        "network is unreachable",
        "getaddrinfo",
        "name resolution",
    ]) {
        PlatformError::Network(format!("连接 YouTube 失败：{message}"))
    } else if any(&[
        "challenge",
        "signature",
        "nsig",
        "update",
        "requested format is not available",
        "no video formats",
        "javascript runtime",
    ]) {
        PlatformError::Parse(format!("YouTube 组件可能需要更新，请更新录播宝：{message}"))
    } else {
        PlatformError::Parse(format!("yt-dlp 无法读取这个直播：{message}"))
    };
    YtdlpFailure::Error(error)
}

/// How long one `yt-dlp -j` may take (it starts Deno to solve YouTube's
/// challenges; the single-file build unpacks itself first).
const YTDLP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Arguments for `yt-dlp -j`. The cookie file and proxy are optional.
pub fn ytdlp_args(
    paths: &ToolPaths,
    url: &str,
    proxy: Option<&str>,
    cookie_file: Option<&std::path::Path>,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "--ignore-config",
        "--no-warnings",
        "--no-progress",
        "--no-playlist",
        "--socket-timeout",
        "20",
        "-j",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let mut runtime = std::ffi::OsString::from("deno:");
    runtime.push(&paths.deno);
    args.push("--js-runtimes".into());
    args.push(runtime);
    args.push("--cache-dir".into());
    args.push(paths.cache_dir.clone().into_os_string());
    if let Some(proxy) = proxy {
        args.push("--proxy".into());
        args.push(proxy.into());
    }
    if let Some(file) = cookie_file {
        args.push("--cookies".into());
        args.push(file.as_os_str().to_owned());
    }
    args.push(url.into());
    args
}

/// Run `yt-dlp -j` on `url`. The user cookie only exists as a temporary
/// Netscape cookies file while the process runs.
pub async fn run_ytdlp(
    paths: &ToolPaths,
    url: &str,
    proxy: Option<&str>,
    cookie: Option<&str>,
) -> Result<Value, YtdlpFailure> {
    let cookie_file = match cookie {
        Some(cookie) => Some(
            crate::ytdlp::TempFile::create(
                &paths.temp_dir,
                "cookies",
                &crate::ytdlp::netscape_cookies(cookie),
            )
            .map_err(|error| {
                YtdlpFailure::Error(PlatformError::Config(format!(
                    "无法写入临时 Cookie 文件：{error}"
                )))
            })?,
        ),
        None => None,
    };
    let proxy = super::normalize_proxy(proxy.unwrap_or_default()).map_err(YtdlpFailure::Error)?;
    let mut command = std::process::Command::new(&paths.ytdlp);
    command
        .args(ytdlp_args(
            paths,
            url,
            proxy.as_deref(),
            cookie_file.as_ref().map(|file| file.path()),
        ))
        .env("PYTHONIOENCODING", "utf-8")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::ytdlp::no_window(&mut command);
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let child = command.spawn().map_err(|error| {
        YtdlpFailure::Error(PlatformError::Config(format!("无法启动 yt-dlp：{error}")))
    })?;
    let output = tokio::time::timeout(YTDLP_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            YtdlpFailure::Error(PlatformError::Network(
                "yt-dlp 读取 YouTube 超时，稍后会自动重试".to_string(),
            ))
        })?
        .map_err(|error| {
            YtdlpFailure::Error(PlatformError::Network(format!("yt-dlp 运行失败：{error}")))
        })?;
    drop(cookie_file);
    if !output.status.success() {
        return Err(classify_ytdlp_error(
            &String::from_utf8_lossy(&output.stderr),
            cookie.is_some(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with('{'))
        .unwrap_or_default();
    serde_json::from_str(line).map_err(|_| {
        YtdlpFailure::Error(PlatformError::Parse(
            "yt-dlp 的输出无法解析，YouTube 组件可能需要更新".to_string(),
        ))
    })
}

/// Headers for pulling YouTube streams when yt-dlp gives none: the
/// User-Agent only. User cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("User-Agent".to_string(), USER_AGENT_VALUE.to_string())])
}

/// Page request headers: User-Agent, English, and the user cookie (or the
/// consent cookie).
pub fn page_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
    );
    headers.insert(COOKIE, cookie_header(cookie.unwrap_or(DEFAULT_COOKIE))?);
    Ok(headers)
}

/// Whether the page is YouTube's "Sign in to confirm you're not a bot" gate.
pub fn is_bot_check(html: &str) -> bool {
    initial_json(html, "ytInitialPlayerResponse").is_some_and(|player| {
        player.get("videoDetails").is_none()
            && player
                .get("playabilityStatus")
                .and_then(|status| status.get("reason"))
                .and_then(Value::as_str)
                .is_some_and(|reason| reason.contains("bot"))
    })
}

/// YouTube adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct YoutubeAdapter {
    clients: ClientCache,
}

impl YoutubeAdapter {
    pub fn new() -> Self {
        YoutubeAdapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);
        let page_url = link.normalized_url();
        let response = client
            .get(&page_url)
            .headers(page_headers(cookie)?)
            .send()
            .await?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Err(PlatformError::InvalidUrl(format!(
                "YouTube 没有这个频道或视频，请检查链接: {page_url}"
            )));
        }
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), &page_url));
        }
        let html = response.text().await?;
        // The page tells offline channels, ended streams and unknown videos
        // apart without any component. yt-dlp is only needed for the stream,
        // and when YouTube's bot check hides the page from a logged-in user.
        let page = match parse_page(&html, &link) {
            Ok(page) if !page.is_live => return Ok(offline_info(&link, Some(&page))),
            Ok(page) => Some(page),
            Err(_) if cookie.is_some() && is_bot_check(&html) => None,
            Err(error) => return Err(error),
        };
        let manager = crate::ytdlp::global().ok_or_else(|| {
            PlatformError::Config("YouTube 组件目录没有设置，无法录制 YouTube".to_string())
        })?;
        let paths = manager.prepare(options.proxy.as_deref())?;
        let target = match page.as_ref().and_then(|page| page.video_id.as_deref()) {
            Some(id) => format!("{SITE}/watch?v={id}"),
            None => page_url,
        };
        match run_ytdlp(&paths, &target, options.proxy.as_deref(), cookie).await {
            Ok(json) => parse_ytdlp(&json, &link, page.as_ref(), options.quality),
            Err(YtdlpFailure::Offline) => Ok(offline_info(&link, page.as_ref())),
            Err(YtdlpFailure::Error(error)) => Err(error),
        }
    }
}

impl PlatformAdapter for YoutubeAdapter {
    fn platform(&self) -> Platform {
        Platform::Youtube
    }

    fn matches(&self, url: &str) -> bool {
        parse_link(url).is_ok()
    }

    fn normalize_url(&self, url: &str) -> Option<String> {
        parse_link(url).ok().map(|link| link.normalized_url())
    }

    fn fetch<'a>(
        &'a self,
        url: &'a str,
        options: &'a FetchOptions,
    ) -> BoxFuture<'a, Result<LiveInfo, PlatformError>> {
        Box::pin(self.fetch_inner(url, options))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL_OFFLINE: &str = include_str!("../../tests/fixtures/youtube/channel_offline.html");
    const VIDEO_NOT_LIVE: &str = include_str!("../../tests/fixtures/youtube/video_not_live.html");
    const LIVE_BOT_CHECK: &str = include_str!("../../tests/fixtures/youtube/live_bot_check.html");
    const YTDLP_LIVE: &str = include_str!("../../tests/fixtures/youtube/ytdlp_live.json");

    fn channel(path: &str) -> YoutubeLink {
        YoutubeLink::Channel {
            path: path.to_string(),
        }
    }

    fn video(id: &str) -> YoutubeLink {
        YoutubeLink::Video { id: id.to_string() }
    }

    #[test]
    fn recognizes_channel_and_video_links() {
        let adapter = YoutubeAdapter::new();
        for (input, link, normalized) in [
            (
                "https://www.youtube.com/@LofiGirl/live",
                channel("@lofigirl"),
                "https://www.youtube.com/@lofigirl/live",
            ),
            (
                "youtube.com/@LofiGirl",
                channel("@lofigirl"),
                "https://www.youtube.com/@lofigirl/live",
            ),
            (
                "https://m.youtube.com/@LofiGirl/streams?app=desktop",
                channel("@lofigirl"),
                "https://www.youtube.com/@lofigirl/live",
            ),
            (
                "https://www.youtube.com/channel/UCSJ4gkVC6NrvII8umztf0Ow/live",
                channel("channel/UCSJ4gkVC6NrvII8umztf0Ow"),
                "https://www.youtube.com/channel/UCSJ4gkVC6NrvII8umztf0Ow/live",
            ),
            (
                "https://www.youtube.com/c/LofiGirl",
                channel("c/LofiGirl"),
                "https://www.youtube.com/c/LofiGirl/live",
            ),
            (
                "https://www.youtube.com/watch?v=jfKfPfyJRdk&t=10",
                video("jfKfPfyJRdk"),
                "https://www.youtube.com/watch?v=jfKfPfyJRdk",
            ),
            (
                "https://www.youtube.com/live/jfKfPfyJRdk?si=abc",
                video("jfKfPfyJRdk"),
                "https://www.youtube.com/watch?v=jfKfPfyJRdk",
            ),
            (
                "https://youtu.be/jfKfPfyJRdk?si=abc",
                video("jfKfPfyJRdk"),
                "https://www.youtube.com/watch?v=jfKfPfyJRdk",
            ),
            (
                "https://www.youtube.com/@%E5%BD%95%E6%92%AD/live",
                channel("@录播"),
                "https://www.youtube.com/@录播/live",
            ),
        ] {
            assert_eq!(parse_link(input).unwrap(), link, "{input}");
            assert!(adapter.matches(input), "{input}");
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some(normalized),
                "{input}"
            );
        }
        assert_eq!(channel("@lofigirl").room_id(), "@lofigirl");
        assert_eq!(channel("channel/UC1").room_id(), "UC1");
        assert_eq!(video("jfKfPfyJRdk").room_id(), "jfKfPfyJRdk");
        let youtube = super::super::adapter_for("https://youtu.be/jfKfPfyJRdk").unwrap();
        assert_eq!(youtube.platform(), Platform::Youtube);
        assert_eq!(Platform::Youtube.key(), "youtube");
        assert_eq!(Platform::Youtube.display_name(), "YouTube");
    }

    #[test]
    fn rejects_links_without_a_channel_or_video() {
        for input in [
            "https://www.youtube.com/",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/feed/subscriptions",
            "https://www.youtube.com/results?search_query=lofi",
            "https://www.youtube.com/@/live",
            "https://youtu.be/",
            "https://www.youtube.com.evil.example/@a/live",
            "https://notyoutube.com/@a/live",
            "ftp://www.youtube.com/@a/live",
            "https://www.twitch.tv/nix",
            "",
        ] {
            assert_eq!(
                parse_link(input).unwrap_err().kind(),
                "invalid_url",
                "{input}"
            );
        }
    }

    #[test]
    fn offline_channel_page_names_the_channel() {
        let link = channel("@mkbhd");
        let page = parse_page(CHANNEL_OFFLINE, &link).unwrap();
        assert_eq!(page.author.as_deref(), Some("Marques Brownlee"));
        assert_eq!(page.channel_id.as_deref(), Some("UCBJycsmduvYEL83R_U4JriQ"));
        let info = offline_info(&link, Some(&page));
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert_eq!(info.room_id, "@mkbhd");
        assert_eq!(info.anchor_name, "Marques Brownlee");
        assert_eq!(
            info.live_url.as_deref(),
            Some("https://www.youtube.com/@mkbhd/live")
        );
        // A video link has no channel page to fall back to.
        assert_eq!(
            parse_page(CHANNEL_OFFLINE, &video("jfKfPfyJRdk"))
                .unwrap_err()
                .kind(),
            "parse"
        );
    }

    #[test]
    fn ordinary_videos_are_not_live() {
        let link = video("dQw4w9WgXcQ");
        let page = parse_page(VIDEO_NOT_LIVE, &link).unwrap();
        assert!(!page.is_live);
        assert_eq!(page.author.as_deref(), Some("Rick Astley"));
        let info = offline_info(&link, Some(&page));
        assert!(!info.is_live);
        assert_eq!(info.anchor_id.as_deref(), Some("UCuAXFkgsw1L7xaCfnd5JJOw"));
        assert_eq!(info.title, None);
    }

    #[test]
    fn the_bot_check_asks_for_a_cookie_or_another_node() {
        let error = parse_page(LIVE_BOT_CHECK, &channel("@lofigirl")).unwrap_err();
        assert_eq!(error.kind(), "auth_required", "{error}");
        assert!(error.message().contains("Cookie"), "{error}");
        assert!(error.message().contains("代理节点"), "{error}");
    }

    #[test]
    fn broken_pages_map_to_error_kinds() {
        let link = video("dQw4w9WgXcQ");
        for (html, kind) in [
            ("<html></html>", "parse"),
            (
                "<a href=\"https://consent.youtube.com/m?continue=x\">",
                "config",
            ),
            (
                r#"var ytInitialPlayerResponse = {"playabilityStatus":{"status":"ERROR","reason":"Video unavailable"}};"#,
                "invalid_url",
            ),
            (
                r#"var ytInitialPlayerResponse = {"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Sign in to confirm your age"}};"#,
                "auth_required",
            ),
        ] {
            assert_eq!(parse_page(html, &link).unwrap_err().kind(), kind, "{html}");
        }
        // A live page needs no HLS address any more: yt-dlp gets the stream.
        let live = parse_page(
            r#"var ytInitialPlayerResponse = {"playabilityStatus":{"status":"OK"},"videoDetails":{"videoId":"dQw4w9WgXcQ","isLive":true,"author":"A"}};"#,
            &link,
        )
        .unwrap();
        assert!(live.is_live);
        assert_eq!(live.video_id.as_deref(), Some("dQw4w9WgXcQ"));
    }

    #[test]
    fn page_requests_carry_the_user_cookie_or_decline_consent() {
        assert_eq!(page_headers(None).unwrap().get(COOKIE).unwrap(), "SOCS=CAI");
        assert_eq!(
            page_headers(Some("SID=1")).unwrap().get(COOKIE).unwrap(),
            "SID=1"
        );
        assert_eq!(page_headers(Some("a\nb")).unwrap_err().kind(), "config");
        assert_eq!(
            page_headers(None).unwrap().get(ACCEPT_LANGUAGE).unwrap(),
            "en-US,en;q=0.9"
        );
    }

    #[test]
    fn ytdlp_formats_map_to_qualities_highest_first() {
        let json: Value = serde_json::from_str(YTDLP_LIVE).unwrap();
        let link = channel("@lofigirl");
        let expected = [
            (Quality::Od, "1080p", "96"),
            (Quality::Uhd, "720p", "95"),
            (Quality::Hd, "480p", "94"),
            (Quality::Sd, "360p", "93"),
            (Quality::Ld, "240p", "92"),
        ];
        for (quality, name, itag) in expected {
            let info = parse_ytdlp(&json, &link, None, quality).unwrap();
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Youtube);
            assert_eq!(info.room_id, "@lofigirl");
            assert_eq!(info.anchor_name, "Lofi Girl");
            assert_eq!(info.anchor_id.as_deref(), Some("UCSJ4gkVC6NrvII8umztf0Ow"));
            assert_eq!(
                info.title.as_deref(),
                Some("christmas lofi radio 🎄 cozy ambience to relax to")
            );
            assert_eq!(info.variants.len(), 6);
            let selected = info.selected.unwrap();
            assert_eq!(selected.variant.name, name);
            assert!(selected.record_url.contains(&format!("/itag/{itag}/")));
            assert_eq!(
                selected.source_url(false, false).0,
                super::super::SourceFormat::Hls
            );
            assert!(info.stream_headers.contains_key("User-Agent"));
            assert!(info.stream_headers.contains_key("Sec-Fetch-Mode"));
            assert!(
                info.stream_headers
                    .keys()
                    .all(|key| !key.eq_ignore_ascii_case("cookie"))
            );
        }
        // Not live: offline, keeping the channel name.
        let mut ended = json.clone();
        ended["is_live"] = Value::Bool(false);
        ended["live_status"] = Value::String("was_live".into());
        let info = parse_ytdlp(&ended, &link, None, Quality::Od).unwrap();
        assert!(!info.is_live && info.selected.is_none() && info.variants.is_empty());
        assert_eq!(info.anchor_name, "Lofi Girl");
        // Live without a recordable format points at an outdated component.
        let mut empty = json;
        empty["formats"] = Value::Array(Vec::new());
        let error = parse_ytdlp(&empty, &link, None, Quality::Od).unwrap_err();
        assert_eq!(error.kind(), "parse");
        assert!(error.message().contains("需要更新"), "{error}");
    }

    #[test]
    fn ytdlp_errors_map_to_states_without_urls() {
        let offline = [
            "ERROR: [youtube:tab] @mkbhd: The channel is not currently live",
            "ERROR: [youtube] abcdefghijk: This live event will begin in 3 hours.",
            "ERROR: [youtube] abcdefghijk: Premieres in 10 minutes",
        ];
        for stderr in offline {
            assert_eq!(
                classify_ytdlp_error(stderr, false),
                YtdlpFailure::Offline,
                "{stderr}"
            );
        }
        let kind = |stderr: &str, cookie: bool| match classify_ytdlp_error(stderr, cookie) {
            YtdlpFailure::Error(error) => error,
            YtdlpFailure::Offline => panic!("offline: {stderr}"),
        };
        let bot = kind(
            "WARNING: x\nERROR: [youtube] abcdefghijk: Sign in to confirm you’re not a bot. Use --cookies-from-browser",
            true,
        );
        assert_eq!(bot.kind(), "auth_required");
        assert!(bot.message().contains("可能已经失效"), "{bot}");
        assert_eq!(
            kind(
                "ERROR: [youtube] x: Join this channel to get access to members-only content",
                false
            )
            .kind(),
            "auth_required"
        );
        assert_eq!(
            kind("ERROR: [youtube] x: Video unavailable", false).kind(),
            "invalid_url"
        );
        assert_eq!(
            kind("ERROR: HTTP Error 429: Too Many Requests", false).kind(),
            "rate_limited"
        );
        let network = kind(
            "ERROR: [youtube] x: Unable to download API page: timed out (caused by https://www.youtube.com/youtubei/v1/player?key=SECRET&sig=1)",
            false,
        );
        assert_eq!(network.kind(), "network");
        assert!(!network.message().contains("SECRET"), "{network}");
        assert!(
            network.message().contains("https://www.youtube.com/…"),
            "{network}"
        );
        let outdated = kind("ERROR: [youtube] x: n challenge solving failed", false);
        assert_eq!(outdated.kind(), "parse");
        assert!(outdated.message().contains("需要更新"), "{outdated}");
    }

    #[test]
    fn urls_are_reduced_to_their_host() {
        assert_eq!(
            strip_urls("see https://rr1---sn.googlevideo.com/videoplayback?sig=abc&n=x now"),
            "see https://rr1---sn.googlevideo.com/… now"
        );
        assert_eq!(strip_urls("http://a.example"), "http://a.example");
        assert_eq!(strip_urls("no links, httpx only"), "no links, httpx only");
    }

    #[test]
    fn ytdlp_runs_with_deno_cache_proxy_and_cookie_file() {
        let paths = ToolPaths {
            ytdlp: "/t/yt-dlp".into(),
            deno: "/t/deno".into(),
            cache_dir: "/t/cache".into(),
            temp_dir: "/t/.tmp".into(),
        };
        let args: Vec<String> = ytdlp_args(
            &paths,
            "https://www.youtube.com/watch?v=x",
            Some("http://127.0.0.1:7890"),
            Some(std::path::Path::new("/t/.tmp/cookies-1.txt")),
        )
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
        let after = |flag: &str| {
            args.iter()
                .position(|arg| arg == flag)
                .map(|index| args[index + 1].clone())
        };
        assert!(args.contains(&"--ignore-config".to_string()));
        assert!(args.contains(&"-j".to_string()));
        assert_eq!(after("--js-runtimes").as_deref(), Some("deno:/t/deno"));
        assert_eq!(after("--cache-dir").as_deref(), Some("/t/cache"));
        assert_eq!(after("--proxy").as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(after("--cookies").as_deref(), Some("/t/.tmp/cookies-1.txt"));
        assert_eq!(args.last().unwrap(), "https://www.youtube.com/watch?v=x");
        let bare = ytdlp_args(&paths, "u", None, None);
        assert!(
            !bare
                .iter()
                .any(|arg| arg == "--proxy" || arg == "--cookies")
        );
    }
}
