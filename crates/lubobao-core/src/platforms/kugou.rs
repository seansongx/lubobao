//! Kugou live (酷狗直播 / 繁星) adapter, following StreamCap's `KugouHandler`
//! and StreamGet v4.0.10 `KugouLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/kugou/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The request flow, the stream request parameters and the
//! choice of stream follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * A room link `fanxing.kugou.com/{房间号}` (StreamCap README example
//!   `fanxing2.kugou.com/50428671?refer=2177&sourceFrom=`) carries the room
//!   id as its only path segment; links with a `roomId=` parameter (mobile
//!   share pages) carry it there.
//! * `getEnterRoomInfo` answers with the anchor (`normalRoomInfo.nickName`,
//!   `kugouId`) and `liveType`, which is `-1` while the room is offline.
//!   Unknown rooms come back with `code: 0` and an empty anchor (`kugouId`
//!   0); StreamGet reports every empty nickname as an unsupported music
//!   channel room.
//! * For other `liveType` values the `mutiline/streamaddr` API lists the
//!   stream lines; an empty list means offline.
//!
//! Streams: every line is a different CDN carrying the same single
//! profile (one rate, H.264), so there is one variant, 原画, recorded for
//! every quality. Like StreamGet it is the first HTTPS FLV URL of the last
//! line's first profile. No HLS is offered to the web player.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, REFERER, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

/// StreamGet `BaseLiveStream._get_pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36 Edg/121.0.0.0";
const ACCEPT_LANGUAGE_VALUE: &str = "zh-CN,zh;q=0.8,zh-TW;q=0.7,zh-HK;q=0.5,en-US;q=0.3,en;q=0.2";
const WEB_REFERER: &str = "https://fanxing.kugou.com/";
const ROOM_PAGE: &str = "https://fanxing.kugou.com/";
const ROOM_API: &str = "https://service2.fanxing.kugou.com/roomcen/room/web/cdn/getEnterRoomInfo";
const STREAM_API: &str = "https://fx1.service.kugou.com/video/pc/live/pull/mutiline/streamaddr";
/// `liveType` reported for an offline room.
const LIVE_TYPE_OFFLINE: i64 = -1;
/// Name of the only variant.
const SOURCE_VARIANT_NAME: &str = "原画";

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Room id from a `roomId=` parameter anywhere in the query or fragment,
/// StreamGet's `re.search('roomId=(\d+)', url)`.
fn room_id_parameter(url: &Url) -> Option<String> {
    [url.query(), url.fragment()]
        .into_iter()
        .flatten()
        .flat_map(|part| part.split(['?', '&']))
        .find_map(|pair| {
            let value = pair.strip_prefix("roomId=")?;
            let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
            Some(digits).filter(|digits| !digits.is_empty())
        })
}

/// Recognize a Kugou link and return its room id, without any network access.
///
/// StreamCap registers `.*\.kugou\.com`; StreamGet takes `roomId=` when the
/// link has it, otherwise the last path segment. Only links whose room id
/// is all digits are accepted, so other pages of the site are rejected here.
pub fn parse_link(input: &str) -> Result<String, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的酷狗直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if !host.ends_with(".kugou.com") {
        return Err(invalid());
    }
    if let Some(room_id) = room_id_parameter(&url) {
        return Ok(room_id);
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        [room_id] if is_digits(room_id) => Ok(room_id.to_string()),
        _ => Err(invalid()),
    }
}

/// Canonical room page for a room id.
pub fn room_page_url(room_id: &str) -> String {
    format!("{ROOM_PAGE}{room_id}")
}

/// URL of the room API for a room id.
pub fn room_api_url(room_id: &str) -> String {
    let mut url = Url::parse(ROOM_API).expect("valid room API URL");
    url.query_pairs_mut().append_pair("roomId", room_id);
    url.to_string()
}

/// URL of the stream API with StreamGet's parameters; `timestamp_ms` is the
/// cache-busting `_` parameter.
pub fn stream_api_url(room_id: &str, timestamp_ms: u128) -> String {
    let mut url = Url::parse(STREAM_API).expect("valid stream API URL");
    url.query_pairs_mut()
        .append_pair("std_rid", room_id)
        .append_pair("std_plat", "7")
        .append_pair("std_kid", "0")
        .append_pair("streamType", "1-2-4-5-8")
        .append_pair("ua", "fx-flash")
        .append_pair("targetLiveTypes", "1-5-6")
        .append_pair("version", "1000")
        .append_pair("supportEncryptMode", "1")
        .append_pair("appid", "1010")
        .append_pair("_", &timestamp_ms.to_string());
    url.to_string()
}

fn parse_json(status: u16, body: &str, what: &str, api: &str) -> Result<Value, PlatformError> {
    if status >= 400 {
        return Err(classify_http_status(status, api));
    }
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited(format!("酷狗{what}返回的不是 JSON，可能触发风控"))
        } else {
            PlatformError::Parse(format!("酷狗{what}返回的数据无法解析"))
        }
    })?;
    if !json.is_object() {
        return Err(PlatformError::Parse(format!(
            "酷狗{what}返回的数据无法解析"
        )));
    }
    if let Some(code) = json.get("code").and_then(Value::as_i64)
        && code != 0
    {
        let message = text(json.get("msg")).unwrap_or_default();
        return Err(PlatformError::Parse(format!(
            "酷狗{what}返回错误 {code}: {message}"
        )));
    }
    json.get("data")
        .filter(|data| data.is_object())
        .cloned()
        .ok_or_else(|| PlatformError::Parse(format!("酷狗{what}缺少 data")))
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Room reported by `getEnterRoomInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomInfo {
    pub room_id: String,
    /// `kugouId` of the anchor.
    pub anchor_id: Option<String>,
    pub anchor_name: String,
    /// `liveType` is not `-1`; the stream API decides whether it is live.
    pub maybe_live: bool,
}

/// Parse a `getEnterRoomInfo` answer.
pub fn parse_room(status: u16, body: &str, room_id: &str) -> Result<RoomInfo, PlatformError> {
    let data = parse_json(status, body, "直播间接口", ROOM_API)?;
    let room = data
        .get("normalRoomInfo")
        .filter(|room| room.is_object())
        .ok_or_else(|| PlatformError::Parse("酷狗直播间接口缺少主播信息".to_string()))?;
    let anchor_id = text(room.get("kugouId"))
        .or_else(|| text(room.get("userId")))
        .filter(|id| id != "0");
    let Some(anchor_name) = text(room.get("nickName")) else {
        return Err(PlatformError::InvalidUrl(if anchor_id.is_none() {
            "酷狗没有找到这个直播间，请检查链接".to_string()
        } else {
            "酷狗音乐频道的直播间不支持录制，请换一个直播间".to_string()
        }));
    };
    let live_type = data.get("liveType").and_then(Value::as_i64);
    Ok(RoomInfo {
        room_id: room_id.to_string(),
        anchor_id,
        anchor_name,
        maybe_live: live_type != Some(LIVE_TYPE_OFFLINE),
    })
}

/// Parse a `mutiline/streamaddr` answer into variants. Empty when the room
/// has no stream lines (offline).
pub fn parse_streams(status: u16, body: &str) -> Result<Vec<StreamVariant>, PlatformError> {
    let data = parse_json(status, body, "取流接口", STREAM_API)?;
    let lines = match data.get("lines") {
        Some(Value::Array(lines)) => lines,
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(_) => {
            return Err(PlatformError::Parse(
                "酷狗取流接口的线路列表无法解析".to_string(),
            ));
        }
    };
    let Some(line) = lines.last() else {
        return Ok(Vec::new());
    };
    let flv = line
        .get("streamProfiles")
        .and_then(Value::as_array)
        .and_then(|profiles| profiles.first())
        .and_then(|profile| profile.get("httpsFlv"))
        .and_then(Value::as_array)
        .and_then(|urls| urls.first())
        .and_then(|url| text(Some(url)))
        .ok_or_else(|| {
            PlatformError::Parse("酷狗直播间正在直播，但取流接口没有返回 FLV 地址".to_string())
        })?;
    Ok(vec![StreamVariant {
        name: SOURCE_VARIANT_NAME.to_string(),
        quality: Some(Quality::Od),
        flv_url: Some(flv),
        hls_url: None,
    }])
}

/// Turn a room answer and its variants into `LiveInfo`.
pub fn build_live_info(room: RoomInfo, variants: Vec<StreamVariant>, quality: Quality) -> LiveInfo {
    let selected = select_variant_index(variants.len(), quality)
        .and_then(|index| selected_stream(quality, &variants[index]));
    LiveInfo {
        platform: Platform::Kugou,
        live_url: Some(room_page_url(&room.room_id)),
        room_id: room.room_id,
        anchor_id: room.anchor_id,
        anchor_name: room.anchor_name,
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

/// Headers for pulling Kugou streams: User-Agent and the web Referer. User
/// cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Referer".to_string(), WEB_REFERER.to_string()),
    ])
}

/// StreamGet's PC headers; the cookie only when the user set one.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
    );
    headers.insert(REFERER, HeaderValue::from_static(WEB_REFERER));
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// Kugou adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct KugouAdapter {
    clients: ClientCache,
}

impl KugouAdapter {
    pub fn new() -> Self {
        KugouAdapter::default()
    }

    async fn get(
        client: &reqwest::Client,
        url: &str,
        cookie: Option<&str>,
    ) -> Result<(u16, String), PlatformError> {
        let response = client.get(url).headers(pc_headers(cookie)?).send().await?;
        let status = response.status().as_u16();
        Ok((status, response.text().await?))
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let room_id = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let (status, body) = Self::get(&client, &room_api_url(&room_id), cookie).await?;
        let room = parse_room(status, &body, &room_id)?;
        let variants = if room.maybe_live {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or_default();
            let (status, body) = Self::get(&client, &stream_api_url(&room_id, now), cookie).await?;
            parse_streams(status, &body)?
        } else {
            Vec::new()
        };
        Ok(build_live_info(room, variants, options.quality))
    }
}

impl PlatformAdapter for KugouAdapter {
    fn platform(&self) -> Platform {
        Platform::Kugou
    }

    fn matches(&self, url: &str) -> bool {
        parse_link(url).is_ok()
    }

    fn normalize_url(&self, url: &str) -> Option<String> {
        parse_link(url).ok().map(|room_id| room_page_url(&room_id))
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

    const ROOM_LIVE: &str = include_str!("../../tests/fixtures/kugou/enter_room_live.json");
    const ROOM_OFFLINE: &str = include_str!("../../tests/fixtures/kugou/enter_room_offline.json");
    const ROOM_MISSING: &str = include_str!("../../tests/fixtures/kugou/enter_room_missing.json");
    const STREAM_LIVE: &str = include_str!("../../tests/fixtures/kugou/stream_live.json");
    const STREAM_OFFLINE: &str = include_str!("../../tests/fixtures/kugou/stream_offline.json");

    const ROOM_ID: &str = "50000001";

    #[test]
    fn recognizes_and_normalizes_links() {
        let adapter = KugouAdapter::new();
        for (input, expected) in [
            (
                "https://fanxing2.kugou.com/50428671?refer=2177&sourceFrom=",
                "50428671",
            ),
            ("  fanxing.kugou.com/1039409/ ", "1039409"),
            ("http://FANXING.KUGOU.COM/7143948#top", "7143948"),
            (
                "https://mfanxing.kugou.com/staticPub/rmobile/sharePage/normalRoom/index.html?roomId=51202136&channel=1",
                "51202136",
            ),
            (
                "https://mfanxing.kugou.com/share/index.html#/?roomId=2451676",
                "2451676",
            ),
        ] {
            assert_eq!(parse_link(input).unwrap(), expected, "{input}");
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            adapter
                .normalize_url("https://fanxing2.kugou.com/50428671?refer=2177&sourceFrom=")
                .as_deref(),
            Some("https://fanxing.kugou.com/50428671")
        );
        let kugou =
            super::super::adapter_for("https://fanxing2.kugou.com/50428671?refer=2177").unwrap();
        assert_eq!(kugou.platform(), Platform::Kugou);
        assert_eq!(Platform::Kugou.key(), "kugou");
        assert_eq!(Platform::Kugou.display_name(), "酷狗直播");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        let adapter = KugouAdapter::new();
        for input in [
            "https://fanxing.kugou.com/",
            "https://fanxing.kugou.com/pcindex/category/1001",
            "https://fanxing.kugou.com/abc",
            "https://www.kugou.com/song/123",
            "https://kugou.com/123",
            "https://fanxing.kugou.com.evil.example/123",
            "https://fanxing.kugou.com/x?roomId=abc",
            "ftp://fanxing.kugou.com/123",
            "https://www.yy.com/22490906",
            "",
        ] {
            assert_eq!(
                parse_link(input).unwrap_err().kind(),
                "invalid_url",
                "{input}"
            );
            assert!(!adapter.matches(input), "{input}");
        }
    }

    #[test]
    fn live_room_records_the_last_line_for_every_quality() {
        let room = parse_room(200, ROOM_LIVE, ROOM_ID).unwrap();
        assert!(room.maybe_live);
        assert_eq!(room.anchor_id.as_deref(), Some("1700000001"));
        assert_eq!(room.anchor_name, "测试主播🎤");
        let variants = parse_streams(200, STREAM_LIVE).unwrap();
        let expected = "https://tx2.liveplay.live.kugou.com/live/fx_hifi_1700000001.flv?cn=fx&fx-ps=40-0-1&token=b";
        assert_eq!(
            variants,
            vec![StreamVariant {
                name: "原画".to_string(),
                quality: Some(Quality::Od),
                flv_url: Some(expected.to_string()),
                hls_url: None,
            }]
        );
        for quality in Quality::ALL {
            let info = build_live_info(room.clone(), variants.clone(), quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Kugou);
            assert_eq!(info.room_id, ROOM_ID);
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://fanxing.kugou.com/50000001")
            );
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert_eq!(
                selected.source_url(false, false),
                (super::super::SourceFormat::Flv, expected)
            );
            assert!(
                info.stream_headers
                    .keys()
                    .all(|key| !key.eq_ignore_ascii_case("cookie"))
            );
        }
    }

    #[test]
    fn offline_and_missing_rooms() {
        let room = parse_room(200, ROOM_OFFLINE, ROOM_ID).unwrap();
        assert!(!room.maybe_live);
        let info = build_live_info(room, Vec::new(), Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert_eq!(info.anchor_name, "测试主播🎤");

        // liveType is not -1 but the stream API has no lines.
        let room = parse_room(200, ROOM_LIVE, ROOM_ID).unwrap();
        let variants = parse_streams(200, STREAM_OFFLINE).unwrap();
        assert!(variants.is_empty());
        let info = build_live_info(room, variants, Quality::Od);
        assert!(!info.is_live);
        assert!(info.variants.is_empty());

        let missing = parse_room(200, ROOM_MISSING, "1").unwrap_err();
        assert_eq!(missing.kind(), "invalid_url", "{missing}");
        assert!(missing.message().contains("没有找到"), "{missing}");
        let music = parse_room(
            200,
            r#"{"code":0,"data":{"liveType":0,"normalRoomInfo":{"kugouId":5,"nickName":""}}}"#,
            "1",
        )
        .unwrap_err();
        assert_eq!(music.kind(), "invalid_url", "{music}");
        assert!(music.message().contains("音乐频道"), "{music}");
    }

    #[test]
    fn broken_answers_map_to_error_kinds() {
        for (status, body, kind) in [
            (200, "", "rate_limited"),
            (200, "<html>", "rate_limited"),
            (403, "", "rate_limited"),
            (502, "", "network"),
            (200, "[]", "parse"),
            (200, r#"{"code":1,"msg":"x"}"#, "parse"),
            (200, r#"{"code":0,"data":{}}"#, "parse"),
        ] {
            assert_eq!(
                parse_room(status, body, ROOM_ID).unwrap_err().kind(),
                kind,
                "{status} {body}"
            );
        }
        for (body, kind) in [
            (r#"{"code":0,"data":{"lines":{}}}"#, "parse"),
            (
                r#"{"code":0,"data":{"lines":[{"streamProfiles":[{"httpsFlv":[]}]}]}}"#,
                "parse",
            ),
            (r#"{"code":0}"#, "parse"),
        ] {
            assert_eq!(parse_streams(200, body).unwrap_err().kind(), kind, "{body}");
        }
        assert!(
            parse_streams(200, r#"{"code":0,"data":{"lines":null}}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn requests_follow_streamget() {
        assert!(pc_headers(None).unwrap().get(COOKIE).is_none());
        assert_eq!(
            pc_headers(Some("kg_mid=1")).unwrap().get(COOKIE).unwrap(),
            "kg_mid=1"
        );
        assert_eq!(pc_headers(Some("a=1\nb=2")).unwrap_err().kind(), "config");
        assert_eq!(
            room_api_url("50428671"),
            "https://service2.fanxing.kugou.com/roomcen/room/web/cdn/getEnterRoomInfo?roomId=50428671"
        );
        assert_eq!(
            stream_api_url("50428671", 1),
            "https://fx1.service.kugou.com/video/pc/live/pull/mutiline/streamaddr?std_rid=50428671&std_plat=7&std_kid=0&streamType=1-2-4-5-8&ua=fx-flash&targetLiveTypes=1-5-6&version=1000&supportEncryptMode=1&appid=1010&_=1"
        );
    }
}
