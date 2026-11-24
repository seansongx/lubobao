//! ShowRoom adapter, following StreamCap's `ShowRoomHandlerHandler` and
//! StreamGet v4.0.10 `ShowRoomLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/showroom/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The `api/live/live_info` request and its `live_status == 2`
//! live check, the `api/live/streaming_url?abr_available=1` request, taking
//! the `hls_all` master playlist and choosing a variant by position after
//! sorting by bandwidth follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * A room link carries either the numeric room id (`/room/profile?room_id=`)
//!   or the room URL key (`/r/{key}`). StreamGet reads the numeric id out of
//!   the room page's HTML; `api/room/status?room_url_key=` returns it as JSON.
//! * `api/live/live_info?room_id=` answers with the room name and
//!   `live_status` (`2` is live).
//! * `api/live/streaming_url` lists WebRTC and HLS streams; `hls_all` is the
//!   adaptive master playlist (1080p, 360p, 144p).

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
const API: &str = "https://www.showroom-live.com/api";

/// First path segments of showroom-live.com pages that are not rooms.
const RESERVED_PATHS: [&str; 22] = [
    "room",
    "onlive",
    "api",
    "event",
    "ranking",
    "follow",
    "timetable",
    "notice",
    "search",
    "genre",
    "payment",
    "user",
    "login",
    "premium_live",
    "campaign",
    "lp",
    "about",
    "help",
    "terms",
    "privacy",
    "r",
    "s",
];

/// How a ShowRoom link names the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomRef {
    /// `/room/profile?room_id=…`
    Id(String),
    /// `/r/{key}` or `/{key}`; lowercase.
    Key(String),
}

/// A recognized ShowRoom room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowroomLink {
    pub room: RoomRef,
}

impl ShowroomLink {
    /// Canonical URL used to detect duplicate tasks.
    pub fn normalized_url(&self) -> String {
        match &self.room {
            RoomRef::Id(id) => format!("https://www.showroom-live.com/room/profile?room_id={id}"),
            RoomRef::Key(key) => format!("https://www.showroom-live.com/r/{key}"),
        }
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.len() <= 12 && text.bytes().all(|b| b.is_ascii_digit())
}

fn is_room_key(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Recognize a ShowRoom link without any network access.
///
/// StreamCap registers `.*\.showroom-live\.com`; StreamGet reads the room id
/// from `/room/profile?room_id=` or from the room page. Accepted:
/// `/r/{key}`, `/room/profile?room_id={id}` and a bare `/{key}`.
pub fn parse_link(input: &str) -> Result<ShowroomLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid =
        || PlatformError::InvalidUrl(format!("不是可识别的 ShowRoom 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "showroom-live.com" && !host.ends_with(".showroom-live.com") {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    let room_id = || {
        url.query_pairs()
            .find(|(key, _)| key == "room_id")
            .map(|(_, value)| value.trim().to_string())
            .filter(|id| is_digits(id))
    };
    let room = match segments.as_slice() {
        ["room", "profile"] => room_id().map(RoomRef::Id),
        ["r", key, ..] if is_room_key(key) => Some(RoomRef::Key(key.to_ascii_lowercase())),
        [key]
            if is_room_key(key) && !RESERVED_PATHS.contains(&key.to_ascii_lowercase().as_str()) =>
        {
            Some(RoomRef::Key(key.to_ascii_lowercase()))
        }
        _ => None,
    };
    match room {
        Some(room) => Ok(ShowroomLink { room }),
        None => Err(PlatformError::InvalidUrl(format!(
            "ShowRoom 链接里没有直播间，请复制形如 showroom-live.com/r/房间名 的链接: {trimmed}"
        ))),
    }
}

fn json_body(body: &str, what: &str) -> Result<Value, PlatformError> {
    serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited(format!("ShowRoom {what}返回的不是 JSON，可能触发了风控"))
        } else {
            PlatformError::Parse(format!("ShowRoom {what}返回的数据无法解析"))
        }
    })
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Numeric room id from `api/room/status` (`404` answers carry `errors`).
pub fn parse_room_status(body: &str, key: &str) -> Result<String, PlatformError> {
    let json = json_body(body, "房间接口")?;
    text(json.get("room_id")).ok_or_else(|| {
        PlatformError::InvalidUrl(format!("ShowRoom 没有找到直播间「{key}」，请检查链接"))
    })
}

/// What `live_info` says about a room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub room_id: String,
    pub name: String,
    pub is_live: bool,
}

/// Parse `api/live/live_info`.
pub fn parse_live_info(body: &str, room_id: &str) -> Result<Room, PlatformError> {
    let json = json_body(body, "直播接口")?;
    let Some(name) = text(json.get("room_name")) else {
        return Err(PlatformError::InvalidUrl(format!(
            "ShowRoom 没有找到直播间（{room_id}），请检查链接"
        )));
    };
    Ok(Room {
        room_id: text(json.get("room_id")).unwrap_or_else(|| room_id.to_string()),
        name,
        is_live: json.get("live_status").and_then(Value::as_i64) == Some(2),
    })
}

/// The `hls_all` master playlist URL from `api/live/streaming_url`.
pub fn parse_streaming_url(body: &str) -> Result<String, PlatformError> {
    let json = json_body(body, "播放接口")?;
    json.get("streaming_url_list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|entry| entry.get("type").and_then(Value::as_str) == Some("hls_all"))
        .and_then(|entry| text(entry.get("url")))
        .ok_or_else(|| {
            PlatformError::AuthRequired(
                "ShowRoom 没有给出播放地址，这个直播可能是付费或需要登录的限定直播".to_string(),
            )
        })
}

/// Master playlist variants, best first. A media playlist (no variants) is
/// recorded as the only variant, as StreamGet does.
pub fn parse_variants(body: &str, url: &Url) -> Result<Vec<MasterVariant>, PlatformError> {
    let variants: Vec<MasterVariant> = parse_master(body, url)
        .into_iter()
        .filter(|variant| !variant.audio_only)
        .collect();
    if !variants.is_empty() {
        return Ok(variants);
    }
    if body.contains("#EXTINF") {
        return Ok(vec![MasterVariant {
            url: url.to_string(),
            bandwidth: 0,
            resolution: None,
            frame_rate: None,
            name: Some("原画".to_string()),
            audio_only: false,
        }]);
    }
    Err(PlatformError::Parse(
        "ShowRoom 播放列表里没有视频流".to_string(),
    ))
}

/// Turn the room and its variants into `LiveInfo`.
pub fn build_live_info(room: &Room, variants: &[MasterVariant], quality: Quality) -> LiveInfo {
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
        .filter(|_| room.is_live)
        .and_then(|variant| selected_stream(quality, variant));
    LiveInfo {
        platform: Platform::Showroom,
        room_id: room.room_id.clone(),
        anchor_id: Some(room.room_id.clone()),
        live_url: Some(format!(
            "https://www.showroom-live.com/room/profile?room_id={}",
            room.room_id
        )),
        anchor_name: room.name.clone(),
        title: None,
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

/// Headers for pulling ShowRoom streams: the User-Agent only.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("User-Agent".to_string(), USER_AGENT_VALUE.to_string())])
}

/// `_get_pc_headers`: User-Agent, Accept-Language and the user cookie.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-CN,zh;q=0.8,zh-TW;q=0.7,zh-HK;q=0.5,en-US;q=0.3,en;q=0.2"),
    );
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// ShowRoom adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct ShowroomAdapter {
    clients: ClientCache,
}

impl ShowroomAdapter {
    pub fn new() -> Self {
        ShowroomAdapter::default()
    }

    async fn get_text(
        client: &reqwest::Client,
        url: &str,
        cookie: Option<&str>,
        accept_not_found: bool,
    ) -> Result<String, PlatformError> {
        let response = client.get(url).headers(pc_headers(cookie)?).send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() && !(accept_not_found && status.as_u16() == 404) {
            return Err(classify_http_status(status.as_u16(), url));
        }
        Ok(body)
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);
        let room_id = match &link.room {
            RoomRef::Id(id) => id.clone(),
            RoomRef::Key(key) => {
                let api = format!("{API}/room/status?room_url_key={key}");
                let body = Self::get_text(&client, &api, cookie, true).await?;
                parse_room_status(&body, key)?
            }
        };
        let api = format!("{API}/live/live_info?room_id={room_id}");
        let body = Self::get_text(&client, &api, cookie, true).await?;
        let room = parse_live_info(&body, &room_id)?;
        if !room.is_live {
            return Ok(build_live_info(&room, &[], options.quality));
        }
        let api = format!("{API}/live/streaming_url?room_id={room_id}&abr_available=1");
        let body = Self::get_text(&client, &api, cookie, false).await?;
        let master = Url::parse(&parse_streaming_url(&body)?)
            .map_err(|_| PlatformError::Parse("ShowRoom 播放地址无法解析".to_string()))?;
        let response = client
            .get(master.clone())
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            // The broadcast ended after the live check.
            if matches!(status.as_u16(), 403 | 404) {
                let ended = Room {
                    is_live: false,
                    ..room
                };
                return Ok(build_live_info(&ended, &[], options.quality));
            }
            return Err(classify_http_status(status.as_u16(), "ShowRoom 播放列表"));
        }
        let base = response.url().clone();
        let variants = parse_variants(&response.text().await?, &base)?;
        Ok(build_live_info(&room, &variants, options.quality))
    }
}

impl PlatformAdapter for ShowroomAdapter {
    fn platform(&self) -> Platform {
        Platform::Showroom
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

    const STATUS_LIVE: &str = include_str!("../../tests/fixtures/showroom/room_status_live.json");
    const LIVE_INFO: &str = include_str!("../../tests/fixtures/showroom/live_info_live.json");
    const LIVE_INFO_OFFLINE: &str =
        include_str!("../../tests/fixtures/showroom/live_info_offline.json");
    const NOT_FOUND: &str = include_str!("../../tests/fixtures/showroom/not_found.json");
    const STREAMING: &str = include_str!("../../tests/fixtures/showroom/streaming_url.json");
    const STREAMING_EMPTY: &str =
        include_str!("../../tests/fixtures/showroom/streaming_url_empty.json");
    const MASTER: &str = include_str!("../../tests/fixtures/showroom/master.m3u8");

    fn master_url() -> Url {
        Url::parse("https://shard902-cdn.showroom-txlive.com/live/TOKEN_520315_main_abr202510.m3u8")
            .unwrap()
    }

    #[test]
    fn recognizes_and_normalizes_links() {
        let adapter = ShowroomAdapter::new();
        for (input, normalized) in [
            (
                "https://www.showroom-live.com/r/yuo0503",
                "https://www.showroom-live.com/r/yuo0503",
            ),
            (
                "https://www.showroom-live.com/r/YUO0503?t=1",
                "https://www.showroom-live.com/r/yuo0503",
            ),
            (
                "showroom-live.com/yuo0503",
                "https://www.showroom-live.com/r/yuo0503",
            ),
            (
                "https://www.showroom-live.com/room/profile?room_id=520315",
                "https://www.showroom-live.com/room/profile?room_id=520315",
            ),
            (
                "https://m.showroom-live.com/r/615191698244",
                "https://www.showroom-live.com/r/615191698244",
            ),
        ] {
            assert!(adapter.matches(input), "{input}");
            assert_eq!(adapter.normalize_url(input).as_deref(), Some(normalized));
        }
        let showroom =
            super::super::adapter_for("https://www.showroom-live.com/r/yuo0503").unwrap();
        assert_eq!(showroom.platform(), Platform::Showroom);
        assert_eq!(Platform::Showroom.key(), "showroom");
        assert_eq!(Platform::Showroom.display_name(), "ShowRoom");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        for input in [
            "https://www.showroom-live.com/",
            "https://www.showroom-live.com/onlive",
            "https://www.showroom-live.com/room/profile",
            "https://www.showroom-live.com/room/profile?room_id=abc",
            "https://www.showroom-live.com.evil.example/r/yuo0503",
            "https://notshowroom-live.com/r/yuo0503",
            "ftp://www.showroom-live.com/r/yuo0503",
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
    fn live_room_maps_variants_by_bandwidth() {
        assert_eq!(parse_room_status(STATUS_LIVE, "yuo0503").unwrap(), "520315");
        let room = parse_live_info(LIVE_INFO, "520315").unwrap();
        assert!(room.is_live);
        assert_eq!(room.name, "Award超ガチ🔥一ノ瀬結衣【IWI!】");
        let master = parse_streaming_url(STREAMING).unwrap();
        assert_eq!(
            master,
            "https://shard902-cdn.showroom-txlive.com/live/TOKEN_520315_main_abr202510.m3u8"
        );
        let variants = parse_variants(MASTER, &master_url()).unwrap();
        let names: Vec<String> = variants.iter().map(MasterVariant::label).collect();
        assert_eq!(names, ["1080p", "360p", "144p"]);
        for (quality, name, suffix) in [
            (Quality::Od, "1080p", "ss"),
            (Quality::Uhd, "360p", "mm"),
            (Quality::Hd, "144p", "ll"),
            (Quality::Sd, "144p", "ll"),
            (Quality::Ld, "144p", "ll"),
        ] {
            let info = build_live_info(&room, &variants, quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Showroom);
            assert_eq!(info.room_id, "520315");
            assert_eq!(info.anchor_name, room.name);
            let selected = info.selected.unwrap();
            assert_eq!(selected.variant.name, name);
            assert_eq!(selected.requested, quality);
            assert_eq!(
                selected.record_url,
                format!(
                    "https://shard902-cdn.showroom-txlive.com/live/TOKEN_520315_main_{suffix}.m3u8"
                )
            );
            assert_eq!(
                selected.source_url(false, false).0,
                super::super::SourceFormat::Hls
            );
        }
        // A media playlist is recorded as the only variant.
        let media = parse_variants("#EXTM3U\n#EXTINF:2.0,\nseg.ts\n", &master_url()).unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].url, master_url().to_string());
    }

    #[test]
    fn offline_missing_and_unplayable_rooms() {
        let room = parse_live_info(LIVE_INFO_OFFLINE, "520315").unwrap();
        assert!(!room.is_live);
        let info = build_live_info(&room, &[], Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        assert_eq!(
            parse_room_status(NOT_FOUND, "zzz").unwrap_err().kind(),
            "invalid_url"
        );
        assert_eq!(
            parse_live_info(NOT_FOUND, "999").unwrap_err().kind(),
            "invalid_url"
        );
        assert_eq!(
            parse_streaming_url(STREAMING_EMPTY).unwrap_err().kind(),
            "auth_required"
        );
        for (body, kind) in [("<html>", "rate_limited"), ("{", "parse")] {
            assert_eq!(
                parse_live_info(body, "1").unwrap_err().kind(),
                kind,
                "{body}"
            );
        }
        assert_eq!(
            parse_variants("#EXTM3U\n", &master_url())
                .unwrap_err()
                .kind(),
            "parse"
        );
        // A live room without variants is not reported as live.
        let live = parse_live_info(LIVE_INFO, "520315").unwrap();
        assert!(!build_live_info(&live, &[], Quality::Od).is_live);
    }

    #[test]
    fn requests_carry_streamget_headers() {
        let headers = pc_headers(None).unwrap();
        assert_eq!(headers.get(USER_AGENT).unwrap(), USER_AGENT_VALUE);
        assert!(headers.get(COOKIE).is_none());
        assert_eq!(
            pc_headers(Some("sr_id=1")).unwrap().get(COOKIE).unwrap(),
            "sr_id=1"
        );
        assert_eq!(pc_headers(Some("a\nb")).unwrap_err().kind(), "config");
    }
}
