//! YouTube adapter, following StreamCap's `YoutubeHandler` and StreamGet
//! v4.0.10 `YoutubeLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/youtube/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. Reading `ytInitialPlayerResponse` from the page, the
//! `videoDetails.isLive` check, recording `streamingData.hlsManifestUrl` and
//! picking a variant from that master playlist sorted by bandwidth follow
//! that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * The page of the link is requested: `watch?v={id}` for a video, or
//!   `/@{handle}/live` (`/channel/{id}/live`, …) for a channel. While the
//!   channel is live, YouTube serves the watch page of the live video there;
//!   otherwise it serves the channel page, which has no player response and
//!   counts as offline. An unknown channel answers 404.
//! * `ytInitialPlayerResponse.videoDetails.isLive` and
//!   `streamingData.hlsManifestUrl` decide whether the video is live. Only
//!   live streams are recorded; ended streams and ordinary videos count as
//!   offline.
//! * YouTube asks some IP ranges (often proxy and data-centre addresses) to
//!   sign in before it plays live streams ("Sign in to confirm you're not a
//!   bot"). That is reported as needing login; a logged-in cookie or another
//!   proxy node helps.
//!
//! YouTube is not reachable from mainland China; the proxy in `FetchOptions`
//! is used for the page and the playlist.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;

use super::hls::{MasterVariant, parse_master};
use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

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
    /// `hlsManifestUrl` while the video is live.
    pub hls_manifest: Option<String>,
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
                hls_manifest: None,
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
    let hls_manifest = player
        .get("streamingData")
        .and_then(|data| text(data.get("hlsManifestUrl")));
    if is_live && hls_manifest.is_none() {
        return Err(PlatformError::Parse(format!(
            "YouTube 正在直播，但没有返回 HLS 地址（{status} {reason}）"
        )));
    }
    Ok(PageInfo {
        channel_id: text(details.get("channelId")),
        author: text(details.get("author")),
        video_id: text(details.get("videoId")),
        title: text(details.get("title")),
        hls_manifest: if is_live { hls_manifest } else { None },
    })
}

/// Turn the page and the master playlist's variants into `LiveInfo`.
pub fn build_live_info(
    link: &YoutubeLink,
    page: &PageInfo,
    variants: &[MasterVariant],
    quality: Quality,
) -> LiveInfo {
    let variants: Vec<StreamVariant> = variants
        .iter()
        .filter(|variant| !variant.audio_only)
        .enumerate()
        .map(|(index, variant)| StreamVariant {
            name: variant.label(),
            quality: Quality::from_index(index),
            flv_url: None,
            hls_url: Some(variant.url.clone()),
        })
        .collect();
    let selected = select_variant_index(variants.len(), quality)
        .and_then(|index| variants.get(index))
        .filter(|_| page.hls_manifest.is_some())
        .and_then(|variant| selected_stream(quality, variant));
    let is_live = selected.is_some();
    let room_id = link.room_id();
    LiveInfo {
        platform: Platform::Youtube,
        anchor_name: page.author.clone().unwrap_or_else(|| room_id.clone()),
        room_id,
        anchor_id: page.channel_id.clone(),
        live_url: Some(link.normalized_url()),
        title: if is_live { page.title.clone() } else { None },
        is_live,
        variants: if is_live { variants } else { Vec::new() },
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling YouTube streams: the User-Agent only. User cookies are
/// never forwarded to CDN hosts.
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
        let page_url = link.normalized_url();
        let response = client
            .get(&page_url)
            .headers(page_headers(user_cookie(options))?)
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
        let page = parse_page(&html, &link)?;
        let Some(manifest) = &page.hls_manifest else {
            return Ok(build_live_info(&link, &page, &[], options.quality));
        };
        let response = client
            .get(manifest)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), "YouTube 播放列表"));
        }
        let base = response.url().clone();
        let body = response.text().await?;
        let variants = parse_master(&body, &base);
        if variants.iter().all(|variant| variant.audio_only) {
            return Err(PlatformError::Parse(
                "YouTube 播放列表里没有视频流".to_string(),
            ));
        }
        Ok(build_live_info(&link, &page, &variants, options.quality))
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
        let info = build_live_info(&link, &page, &[], Quality::Od);
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
        assert_eq!(page.hls_manifest, None);
        assert_eq!(page.author.as_deref(), Some("Rick Astley"));
        let info = build_live_info(&link, &page, &[], Quality::Od);
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
            (
                r#"var ytInitialPlayerResponse = {"playabilityStatus":{"status":"OK"},"videoDetails":{"videoId":"dQw4w9WgXcQ","isLive":true}};"#,
                "parse",
            ),
        ] {
            assert_eq!(parse_page(html, &link).unwrap_err().kind(), kind, "{html}");
        }
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
}
