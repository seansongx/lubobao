//! CHZZK (치지직, NAVER's live platform) adapter, following StreamCap's
//! `ChzzkHandler` and StreamGet v4.0.10 `ChzzkLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/chzzk/live_stream.py`, MIT License, Copyright (c) 2025
//! Hmily. The request headers, the `service/v3/channels/{id}/live-detail`
//! request, the `status == "OPEN"` live check, reading the master playlist
//! from `livePlaybackJson` and choosing a variant by position after sorting
//! by bandwidth follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! `live-detail` answers with the channel, the broadcast `status` (`OPEN` or
//! `CLOSE`) and, for a broadcast that can be watched, `livePlaybackJson`
//! (a JSON string) whose `media[0].path` is the HLS master playlist. A
//! `CLOSE` answer can still carry the previous broadcast's playback JSON, so
//! it is only read while the status is `OPEN`. Adult broadcasts have no
//! playback JSON without a logged-in cookie (`userAdultStatus:
//! NOT_LOGIN_USER`).
//!
//! The media segments do not match the extension FFmpeg 8's HLS demuxer
//! expects for their container; [`Platform::relaxed_hls_extension`] makes the
//! recorder pass `-extension_picky 0`, as StreamCap's
//! `RELAXED_HLS_EXTENSION_CHECK_PLATFORMS` does.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT,
};
use serde_json::Value;

use super::hls::{MasterVariant, parse_master};
use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

/// `ChzzkLiveStream._get_pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:109.0) Gecko/20100101 Firefox/115.0";
const ORIGIN_VALUE: &str = "https://chzzk.naver.com";
const REFERER_VALUE: &str = "https://chzzk.naver.com/live/458f6ec20b034f49e0fc6d03921646d2";
const API: &str = "https://api.chzzk.naver.com/service/v3/channels";

/// Channel ids are 32 hexadecimal characters.
fn is_channel_id(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A recognized CHZZK channel; the id is lowercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChzzkLink {
    pub channel_id: String,
}

impl ChzzkLink {
    /// Canonical URL used to detect duplicate tasks.
    pub fn normalized_url(&self) -> String {
        room_url(&self.channel_id)
    }
}

/// Live page of a channel.
pub fn room_url(channel_id: &str) -> String {
    format!("https://chzzk.naver.com/live/{channel_id}")
}

/// Recognize a CHZZK link without any network access.
///
/// StreamCap registers `chzzk.naver.com/`; StreamGet takes the last path
/// segment as the channel id. Accepted: `/live/{id}` and the channel page
/// `/{id}` (also `m.chzzk.naver.com`).
pub fn parse_link(input: &str) -> Result<ChzzkLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的 CHZZK 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "chzzk.naver.com" && host != "m.chzzk.naver.com" {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    let id = match segments.as_slice() {
        ["live", id, ..] | [id, ..] if is_channel_id(id) => Some(*id),
        _ => None,
    };
    match id {
        Some(id) => Ok(ChzzkLink {
            channel_id: id.to_ascii_lowercase(),
        }),
        None => Err(PlatformError::InvalidUrl(format!(
            "CHZZK 链接里没有频道 ID，请复制形如 chzzk.naver.com/live/频道ID 的链接: {trimmed}"
        ))),
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// What `live-detail` says about the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub channel_id: String,
    pub name: String,
    pub title: Option<String>,
    /// `status == "OPEN"` with a playable stream.
    pub is_live: bool,
    /// HLS master playlist from `livePlaybackJson`, only while live.
    pub master_url: Option<String>,
}

/// Parse the `live-detail` answer for channel `channel_id` from the link.
pub fn parse_detail(body: &str, channel_id: &str) -> Result<Channel, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited("CHZZK 接口返回的不是 JSON，可能触发了风控".to_string())
        } else {
            PlatformError::Parse("CHZZK 接口返回的数据无法解析".to_string())
        }
    })?;
    let code = json.get("code").and_then(Value::as_i64);
    if code == Some(404) {
        return Err(PlatformError::InvalidUrl(format!(
            "CHZZK 没有找到频道（{channel_id}），请检查链接"
        )));
    }
    let content = json.get("content").filter(|content| content.is_object());
    let (Some(200), Some(content)) = (code, content) else {
        let message = text(json.get("message")).unwrap_or_default();
        return Err(PlatformError::Parse(format!(
            "CHZZK 接口返回错误 {}：{message}",
            code.map_or_else(String::new, |code| code.to_string())
        )));
    };
    let name = text(content.pointer("/channel/channelName"))
        .unwrap_or_else(|| format!("CHZZK {channel_id}"));
    let title = text(content.get("liveTitle"));
    let open = content.get("status").and_then(Value::as_str) == Some("OPEN");
    let mut channel = Channel {
        channel_id: text(content.pointer("/channel/channelId"))
            .map(|id| id.to_ascii_lowercase())
            .unwrap_or_else(|| channel_id.to_string()),
        name,
        // The title of a closed broadcast is the previous one.
        title: title.filter(|_| open),
        is_live: false,
        master_url: None,
    };
    if !open {
        return Ok(channel);
    }
    let playback = text(content.get("livePlaybackJson"));
    let Some(playback) = playback else {
        let adult = content.get("adult").and_then(Value::as_bool) == Some(true)
            || text(content.get("userAdultStatus")).as_deref() == Some("NOT_LOGIN_USER");
        return Err(if adult {
            PlatformError::AuthRequired(
                "CHZZK 这个直播是成人直播，需要登录后的 Cookie（NID_AUT、NID_SES）".to_string(),
            )
        } else if content.get("krOnlyViewing").and_then(Value::as_bool) == Some(true) {
            PlatformError::Config(
                "CHZZK 这个直播只允许韩国地区观看，请换韩国的代理节点".to_string(),
            )
        } else {
            PlatformError::AuthRequired(
                "CHZZK 没有给出播放地址，这个直播可能需要登录或付费".to_string(),
            )
        });
    };
    let playback: Value = serde_json::from_str(&playback)
        .map_err(|_| PlatformError::Parse("CHZZK 播放信息无法解析".to_string()))?;
    if playback.pointer("/meta/paidLive").and_then(Value::as_bool) == Some(true) {
        return Err(PlatformError::AuthRequired(
            "CHZZK 这个直播是付费直播，无法录制".to_string(),
        ));
    }
    channel.master_url = text(playback.pointer("/media/0/path"));
    channel.is_live = channel.master_url.is_some();
    if !channel.is_live {
        return Err(PlatformError::Parse(
            "CHZZK 播放信息里没有播放地址".to_string(),
        ));
    }
    Ok(channel)
}

/// Master playlist variants, best first. Audio-only entries are dropped.
pub fn parse_variants(body: &str, base: &Url) -> Result<Vec<MasterVariant>, PlatformError> {
    let variants: Vec<MasterVariant> = parse_master(body, base)
        .into_iter()
        .filter(|variant| !variant.audio_only)
        .collect();
    if variants.is_empty() {
        return Err(PlatformError::Parse(
            "CHZZK 播放列表里没有视频流".to_string(),
        ));
    }
    Ok(variants)
}

/// Turn the channel and its variants into `LiveInfo`.
pub fn build_live_info(
    channel: &Channel,
    variants: &[MasterVariant],
    quality: Quality,
) -> LiveInfo {
    let variants: Vec<StreamVariant> = variants
        .iter()
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
        .filter(|_| channel.is_live)
        .and_then(|variant| selected_stream(quality, variant));
    LiveInfo {
        platform: Platform::Chzzk,
        room_id: channel.channel_id.clone(),
        anchor_id: Some(channel.channel_id.clone()),
        live_url: Some(room_url(&channel.channel_id)),
        anchor_name: channel.name.clone(),
        title: channel.title.clone(),
        is_live: selected.is_some(),
        variants: if selected.is_some() {
            variants
        } else {
            Vec::new()
        },
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling CHZZK streams: User-Agent, Origin and Referer. User
/// cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Origin".to_string(), ORIGIN_VALUE.to_string()),
        ("Referer".to_string(), format!("{ORIGIN_VALUE}/")),
    ])
}

/// `_get_pc_headers`: JSON accept, Origin, Referer and the user cookie.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6"),
    );
    headers.insert(ORIGIN, HeaderValue::from_static(ORIGIN_VALUE));
    headers.insert(REFERER, HeaderValue::from_static(REFERER_VALUE));
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// CHZZK adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct ChzzkAdapter {
    clients: ClientCache,
}

impl ChzzkAdapter {
    pub fn new() -> Self {
        ChzzkAdapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let api = format!("{API}/{}/live-detail", link.channel_id);
        let response = client
            .get(&api)
            .headers(pc_headers(user_cookie(options))?)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() && !body.trim_start().starts_with('{') {
            return Err(classify_http_status(status.as_u16(), &api));
        }
        let channel = parse_detail(&body, &link.channel_id)?;
        let Some(master_url) = channel.master_url.clone() else {
            return Ok(build_live_info(&channel, &[], options.quality));
        };
        let response = client
            .get(&master_url)
            .headers(pc_headers(None)?)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            // The broadcast ended after the live-detail request.
            if matches!(status.as_u16(), 403 | 404) {
                let ended = Channel {
                    is_live: false,
                    master_url: None,
                    ..channel
                };
                return Ok(build_live_info(&ended, &[], options.quality));
            }
            return Err(classify_http_status(status.as_u16(), "CHZZK 播放列表"));
        }
        let base = response.url().clone();
        let variants = parse_variants(&response.text().await?, &base)?;
        Ok(build_live_info(&channel, &variants, options.quality))
    }
}

impl PlatformAdapter for ChzzkAdapter {
    fn platform(&self) -> Platform {
        Platform::Chzzk
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

    const LIVE: &str = include_str!("../../tests/fixtures/chzzk/live_detail_live.json");
    const OFFLINE: &str = include_str!("../../tests/fixtures/chzzk/live_detail_offline.json");
    const ADULT: &str = include_str!("../../tests/fixtures/chzzk/live_detail_adult.json");
    const MISSING: &str = include_str!("../../tests/fixtures/chzzk/live_detail_missing.json");
    const MASTER: &str = include_str!("../../tests/fixtures/chzzk/master.m3u8");
    const ID: &str = "0b33823ac81de48d5b78a38cdbc0ab94";

    fn base() -> Url {
        Url::parse(
            "https://livecloud.akamaized.net/chzzk/lip2_kr/x/y/z_hls_playlist.m3u8?hdnts=TOKEN",
        )
        .unwrap()
    }

    #[test]
    fn recognizes_and_normalizes_links() {
        let adapter = ChzzkAdapter::new();
        for input in [
            format!("https://chzzk.naver.com/live/{ID}"),
            format!("https://chzzk.naver.com/live/{}?a=1", ID.to_uppercase()),
            format!("chzzk.naver.com/{ID}"),
            format!("https://m.chzzk.naver.com/live/{ID}"),
        ] {
            assert_eq!(parse_link(&input).unwrap().channel_id, ID, "{input}");
            assert!(adapter.matches(&input), "{input}");
            assert_eq!(
                adapter.normalize_url(&input),
                Some(format!("https://chzzk.naver.com/live/{ID}")),
                "{input}"
            );
        }
        let chzzk =
            super::super::adapter_for(&format!("https://chzzk.naver.com/live/{ID}")).unwrap();
        assert_eq!(chzzk.platform(), Platform::Chzzk);
        assert_eq!(Platform::Chzzk.key(), "chzzk");
        assert_eq!(Platform::Chzzk.display_name(), "CHZZK");
        assert!(Platform::Chzzk.relaxed_hls_extension());
        assert!(!Platform::Twitch.relaxed_hls_extension());
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        for input in [
            "https://chzzk.naver.com/",
            "https://chzzk.naver.com/live/notanid",
            "https://chzzk.naver.com/video/123456",
            "https://chzzk.naver.com.evil.example/live/0b33823ac81de48d5b78a38cdbc0ab94",
            "https://naver.com/0b33823ac81de48d5b78a38cdbc0ab94",
            "ftp://chzzk.naver.com/live/0b33823ac81de48d5b78a38cdbc0ab94",
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
    fn live_channel_maps_variants_by_bandwidth() {
        let channel = parse_detail(LIVE, ID).unwrap();
        assert!(channel.is_live);
        assert_eq!(channel.name, "울프");
        assert_eq!(channel.title.as_deref(), Some("해설의 시선"));
        assert!(
            channel
                .master_url
                .as_deref()
                .unwrap()
                .contains("hdnts=TOKEN")
        );
        let variants = parse_variants(MASTER, &base()).unwrap();
        let names: Vec<String> = variants.iter().map(MasterVariant::label).collect();
        assert_eq!(names, ["1080p", "720p", "480p", "360p", "144p"]);
        for (quality, name) in [
            (Quality::Od, "1080p"),
            (Quality::Uhd, "720p"),
            (Quality::Hd, "480p"),
            (Quality::Sd, "360p"),
            (Quality::Ld, "144p"),
        ] {
            let info = build_live_info(&channel, &variants, quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Chzzk);
            assert_eq!(info.room_id, ID);
            assert_eq!(info.anchor_id.as_deref(), Some(ID));
            assert_eq!(info.anchor_name, "울프");
            let selected = info.selected.unwrap();
            assert_eq!(selected.variant.name, name);
            assert_eq!(selected.requested, quality);
            assert!(
                selected.record_url.starts_with(
                    "https://livecloud.akamaized.net/chzzk/lip2_kr/x/y/hdntl=exp=1~acl=*%2fx%2f*~hmac=a/"
                ),
                "{}",
                selected.record_url
            );
            assert!(
                selected
                    .record_url
                    .ends_with(&format!("/{name}/z_hls_chunklist.m3u8"))
            );
            assert_eq!(
                selected.source_url(false, false).0,
                super::super::SourceFormat::Hls
            );
        }
    }

    #[test]
    fn offline_missing_and_adult_channels() {
        // A closed broadcast still carries the previous playback JSON.
        let channel = parse_detail(OFFLINE, "c42cd75ec4855a9edf204a407c3c1dd2").unwrap();
        assert!(!channel.is_live);
        assert!(channel.master_url.is_none());
        assert!(channel.title.is_none());
        let info = build_live_info(&channel, &[], Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert_eq!(info.anchor_name, "치지직");
        assert_eq!(parse_detail(MISSING, ID).unwrap_err().kind(), "invalid_url");
        assert_eq!(parse_detail(ADULT, ID).unwrap_err().kind(), "auth_required");
        let korea_only = ADULT
            .replace("\"adult\": true", "\"adult\": false")
            .replace("NOT_LOGIN_USER", "NONE")
            .replace(
                "\"livePlaybackJson\": null",
                "\"krOnlyViewing\": true, \"livePlaybackJson\": null",
            );
        assert_eq!(parse_detail(&korea_only, ID).unwrap_err().kind(), "config");
        let paid = LIVE.replace("\\\"paidLive\\\": false", "\\\"paidLive\\\": true");
        assert_eq!(parse_detail(&paid, ID).unwrap_err().kind(), "auth_required");
        for (body, kind) in [
            ("<html>", "rate_limited"),
            ("{", "parse"),
            (r#"{"code":500,"message":"x"}"#, "parse"),
        ] {
            assert_eq!(parse_detail(body, ID).unwrap_err().kind(), kind, "{body}");
        }
        assert_eq!(
            parse_variants("#EXTM3U\n", &base()).unwrap_err().kind(),
            "parse"
        );
        // A live channel without playable variants is not reported as live.
        let live = parse_detail(LIVE, ID).unwrap();
        assert!(!build_live_info(&live, &[], Quality::Od).is_live);
    }

    #[test]
    fn requests_carry_streamget_headers() {
        let headers = pc_headers(None).unwrap();
        assert_eq!(headers.get(ORIGIN).unwrap(), ORIGIN_VALUE);
        assert!(headers.get(COOKIE).is_none());
        assert_eq!(
            pc_headers(Some("NID_AUT=1")).unwrap().get(COOKIE).unwrap(),
            "NID_AUT=1"
        );
        assert_eq!(pc_headers(Some("a\nb")).unwrap_err().kind(), "config");
        let stream = stream_headers();
        assert_eq!(stream["Origin"], ORIGIN_VALUE);
        assert!(!stream.contains_key("Cookie"));
    }
}
