//! 六间房 adapter, following StreamCap's `SixRoomHandler` and StreamGet
//! v4.0.10 `SixRoomLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/sixroom/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The request flow, the iOS app headers, the `inroom` form body
//! and the FLV address follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * The room page `v.6.cn/{room id}` carries `rid: '{anchor uid}'` right
//!   before `roomid:` in its page script. Unknown rooms answer HTTP 404.
//! * `v.6.cn/coop/mobile/index.php?padapi=coop-mobile-inroom.php` with the
//!   anchor uid as `ruid` returns `flag: "001"` and `content.roominfo.alias`
//!   (anchor name) and `content.liveinfo.flvtitle`, which is empty unless the
//!   anchor is live. Rooms it will not open answer another flag with a
//!   message in `content`.
//! * The stream is `wlive.6rooms.com/httpflv/{flvtitle}.flv`.
//!
//! 六间房 offers one stream, so every tier records the same FLV, as StreamGet
//! does.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{
    ACCEPT_LANGUAGE, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, REFERER, USER_AGENT,
};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

/// `SixRoomLiveStream.mobile_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str = "ios/7.830 (ios 17.0; ; iPhone 15 (A2846/A3089/A3090/A3092))";
const ACCEPT_LANGUAGE_VALUE: &str = "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6";
const APP_REFERER: &str = "https://ios.6.cn/?ver=8.0.3&build=4";
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const ROOM_PAGE: &str = "https://v.6.cn/";
const INROOM_API: &str = "https://v.6.cn/coop/mobile/index.php?padapi=coop-mobile-inroom.php";
const FLV_BASE: &str = "https://wlive.6rooms.com/httpflv/";
/// `inroom` flag for a successful answer.
const FLAG_OK: &str = "001";
/// Name of the only variant.
const VARIANT_NAME: &str = "原画";

/// Recognize a room link and return the room id, without any network access.
///
/// StreamCap registers `v.6.cn/` and StreamGet takes the last path segment
/// as the room id; its example is `v.6.cn/634435`. Only `v.6.cn/{room id}`
/// (and the mobile `m.v.6.cn/{room id}`, which the `v.6.cn/` pattern also
/// matches) with a numeric room id is accepted.
pub fn parse_link(input: &str) -> Result<String, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的六间房直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "v.6.cn" && host != "m.v.6.cn" {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        [room_id] if is_room_id(room_id) => Ok(room_id.to_string()),
        _ => Err(invalid()),
    }
}

fn is_room_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 12 && text.bytes().all(|b| b.is_ascii_digit())
}

/// Canonical room URL for a room id.
pub fn room_url(room_id: &str) -> String {
    format!("{ROOM_PAGE}{room_id}")
}

/// The anchor uid from the room page, as StreamGet's
/// `rid: '(.*?)',\n\s+roomid` regex finds it.
pub fn parse_room_page(html: &str) -> Result<String, PlatformError> {
    let mut rest = html;
    while let Some(start) = rest.find("rid: '") {
        let after = &rest[start + "rid: '".len()..];
        let Some(end) = after.find('\'') else { break };
        let uid = &after[..end];
        let tail = &after[end + 1..];
        if let Some(tail) = tail.strip_prefix(",\n")
            && tail.trim_start().starts_with("roomid")
            && !uid.is_empty()
            && uid.bytes().all(|b| b.is_ascii_digit())
        {
            return Ok(uid.to_string());
        }
        rest = after;
    }
    Err(PlatformError::Parse(
        "六间房直播间页面里没有找到主播 ID".to_string(),
    ))
}

/// What `inroom` says about the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomInfo {
    pub anchor_name: String,
    pub title: Option<String>,
    /// `liveinfo.flvtitle`, present only while the anchor is live.
    pub flv_title: Option<String>,
}

fn text_field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// Parse an `inroom` response for the room `room_id` from the link.
pub fn parse_inroom(body: &str, room_id: &str) -> Result<RoomInfo, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited("六间房房间接口返回的不是 JSON，可能触发风控".to_string())
        } else {
            PlatformError::Parse("六间房房间接口返回的数据无法解析".to_string())
        }
    })?;
    let flag = match json.get("flag") {
        Some(Value::String(flag)) => flag.clone(),
        Some(Value::Number(flag)) => flag.to_string(),
        _ => {
            return Err(PlatformError::Parse(
                "六间房房间接口返回的数据无法解析".to_string(),
            ));
        }
    };
    let content = json.get("content");
    if flag != FLAG_OK {
        let detail = content
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("未知错误");
        let message = format!("六间房房间接口返回错误 {flag}：{detail}");
        return Err(if detail.contains("登录") {
            PlatformError::AuthRequired(message)
        } else if detail.contains("频繁") {
            PlatformError::RateLimited(message)
        } else if detail.contains("不能进入") || detail.contains("不存在") {
            PlatformError::InvalidUrl(message)
        } else {
            PlatformError::Parse(message)
        });
    }
    let room = content
        .and_then(|content| content.get("roominfo"))
        .filter(|room| room.is_object())
        .ok_or_else(|| PlatformError::Parse("六间房房间接口缺少 roominfo".to_string()))?;
    let live = content
        .and_then(|content| content.get("liveinfo"))
        .filter(|live| live.is_object())
        .ok_or_else(|| PlatformError::Parse("六间房房间接口缺少 liveinfo".to_string()))?;
    let anchor_name = text_field(Some(room), "alias")
        .map(str::to_string)
        .unwrap_or_else(|| format!("六间房 {room_id}"));
    Ok(RoomInfo {
        anchor_name,
        title: text_field(Some(live), "title").map(str::to_string),
        flv_title: text_field(Some(live), "flvtitle").map(str::to_string),
    })
}

/// FLV address for a `flvtitle`.
pub fn flv_url(flv_title: &str) -> String {
    format!("{FLV_BASE}{flv_title}.flv")
}

/// Turn the room facts into `LiveInfo`.
pub fn build_live_info(
    room_id: &str,
    anchor_uid: &str,
    room: RoomInfo,
    quality: Quality,
) -> LiveInfo {
    let variants: Vec<StreamVariant> = room
        .flv_title
        .as_deref()
        .map(|flv_title| StreamVariant {
            name: VARIANT_NAME.to_string(),
            quality: Some(Quality::Od),
            flv_url: Some(flv_url(flv_title)),
            hls_url: None,
        })
        .into_iter()
        .collect();
    let selected = select_variant_index(variants.len(), quality)
        .and_then(|index| selected_stream(quality, &variants[index]));
    let is_live = selected.is_some();
    LiveInfo {
        platform: Platform::SixRoom,
        room_id: room_id.to_string(),
        anchor_id: Some(anchor_uid.to_string()),
        live_url: Some(room_url(room_id)),
        anchor_name: room.anchor_name,
        title: if is_live { room.title } else { None },
        is_live,
        variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling the stream: the app User-Agent and Referer. User
/// cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Referer".to_string(), APP_REFERER.to_string()),
    ])
}

/// `SixRoomLiveStream.mobile_headers`: Accept-Language, Referer, User-Agent
/// and the user cookie (StreamGet sends an empty cookie when there is none).
pub fn mobile_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
    );
    headers.insert(REFERER, HeaderValue::from_static(APP_REFERER));
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// `inroom` form body. The uid is all digits, so it needs no escaping.
pub fn inroom_body(anchor_uid: &str) -> String {
    format!("av=3.1&encpass=&logiuid=&project=v6iphone&rate=1&rid=&ruid={anchor_uid}")
}

/// 六间房 adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct SixRoomAdapter {
    clients: ClientCache,
}

impl SixRoomAdapter {
    pub fn new() -> Self {
        SixRoomAdapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let room_id = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let headers = mobile_headers(user_cookie(options))?;

        let page_url = room_url(&room_id);
        let response = client
            .get(&page_url)
            .headers(headers.clone())
            .send()
            .await?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Err(PlatformError::InvalidUrl(format!(
                "六间房没有找到直播间 {room_id}，请检查链接"
            )));
        }
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), &page_url));
        }
        let anchor_uid = parse_room_page(&response.text().await?)?;

        let response = client
            .post(INROOM_API)
            .headers(headers)
            .header(CONTENT_TYPE, FORM_CONTENT_TYPE)
            .body(inroom_body(&anchor_uid))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), INROOM_API));
        }
        let room = parse_inroom(&response.text().await?, &room_id)?;
        Ok(build_live_info(
            &room_id,
            &anchor_uid,
            room,
            options.quality,
        ))
    }
}

impl PlatformAdapter for SixRoomAdapter {
    fn platform(&self) -> Platform {
        Platform::SixRoom
    }

    fn matches(&self, url: &str) -> bool {
        parse_link(url).is_ok()
    }

    fn normalize_url(&self, url: &str) -> Option<String> {
        parse_link(url).ok().map(|room_id| room_url(&room_id))
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

    const ROOM_PAGE_HTML: &str = include_str!("../../tests/fixtures/6room/room_page.html");
    const INROOM_LIVE: &str = include_str!("../../tests/fixtures/6room/inroom_live.json");
    const INROOM_OFFLINE: &str = include_str!("../../tests/fixtures/6room/inroom_offline.json");
    const INROOM_BLOCKED: &str = include_str!("../../tests/fixtures/6room/inroom_blocked.json");

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let adapter = SixRoomAdapter::new();
        for (input, room_id) in [
            ("https://v.6.cn/634435", "634435"),
            ("  v.6.cn/634435/ ", "634435"),
            ("http://V.6.CN/58018?src=share#x", "58018"),
            ("https://m.v.6.cn/58018", "58018"),
        ] {
            assert_eq!(parse_link(input).unwrap(), room_id, "{input}");
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            adapter
                .normalize_url("https://m.v.6.cn/58018?x=1")
                .as_deref(),
            Some("https://v.6.cn/58018")
        );
        let adapter = super::super::adapter_for("https://v.6.cn/634435").unwrap();
        assert_eq!(adapter.platform(), Platform::SixRoom);
        assert_eq!(Platform::SixRoom.key(), "6room");
        assert_eq!(Platform::SixRoom.display_name(), "六间房直播");
        assert_eq!(
            serde_json::to_value(Platform::SixRoom).unwrap(),
            Value::String("6room".to_string())
        );
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        let adapter = SixRoomAdapter::new();
        for input in [
            "https://v.6.cn/",
            "https://v.6.cn/abc",
            "https://v.6.cn/coop/pub/getRand.php",
            "https://v.6.cn/1/2",
            "https://v.6.cn/1234567890123",
            "https://www.6.cn/634435",
            "https://v.6.cn.evil.example/634435",
            "ftp://v.6.cn/634435",
            "https://live.acfun.cn/live/179922",
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
    fn room_page_gives_the_anchor_uid() {
        assert_eq!(parse_room_page(ROOM_PAGE_HTML).unwrap(), "10000001");
        // Only the `rid` right before `roomid` counts, as in StreamGet's regex.
        let other = "var x = {rid: '5', a: 1};\n".to_string() + ROOM_PAGE_HTML;
        assert_eq!(parse_room_page(&other).unwrap(), "10000001");
        for html in ["", "<html></html>", "rid: '',\n roomid: 1"] {
            assert_eq!(parse_room_page(html).unwrap_err().kind(), "parse", "{html}");
        }
    }

    #[test]
    fn live_room_records_the_flv_for_every_tier() {
        let room = parse_inroom(INROOM_LIVE, "10001").unwrap();
        assert_eq!(room.anchor_name, "测试主播🎤");
        assert_eq!(room.flv_title.as_deref(), Some("v10000001-200000001"));
        assert_eq!(room.title, None);
        for quality in Quality::ALL {
            let info = build_live_info("10001", "10000001", room.clone(), quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::SixRoom);
            assert_eq!(info.room_id, "10001");
            assert_eq!(info.anchor_id.as_deref(), Some("10000001"));
            assert_eq!(info.live_url.as_deref(), Some("https://v.6.cn/10001"));
            assert_eq!(info.variants.len(), 1);
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert_eq!(
                selected.record_url,
                "https://wlive.6rooms.com/httpflv/v10000001-200000001.flv"
            );
            assert_eq!(
                selected.source_url(false, true),
                (
                    super::super::SourceFormat::Flv,
                    selected.record_url.as_str()
                )
            );
            assert!(
                info.stream_headers
                    .keys()
                    .all(|key| !key.eq_ignore_ascii_case("cookie"))
            );
        }
        let titled = INROOM_LIVE.replacen("\"title\": \"\"", "\"title\": \"晚间聊天\"", 1);
        assert_eq!(
            parse_inroom(&titled, "10001").unwrap().title.as_deref(),
            Some("晚间聊天")
        );
    }

    #[test]
    fn offline_rooms_are_not_live() {
        let room = parse_inroom(INROOM_OFFLINE, "10002").unwrap();
        assert_eq!(room.anchor_name, "下播主播");
        assert_eq!(room.flv_title, None);
        let info = build_live_info("10002", "10000002", room, Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        let nameless = INROOM_OFFLINE.replace("下播主播", " ");
        assert_eq!(
            parse_inroom(&nameless, "10002").unwrap().anchor_name,
            "六间房 10002"
        );
    }

    #[test]
    fn inroom_errors_map_to_error_kinds() {
        let blocked = parse_inroom(INROOM_BLOCKED, "1").unwrap_err();
        assert_eq!(blocked.kind(), "invalid_url", "{blocked}");
        assert!(blocked.message().contains("暂不能进入此房间"), "{blocked}");
        for (body, kind) in [
            ("", "rate_limited"),
            ("<html>", "rate_limited"),
            ("{\"flag\":", "parse"),
            ("[]", "parse"),
            ("{\"flag\":\"001\"}", "parse"),
            ("{\"flag\":\"001\",\"content\":{\"roominfo\":{}}}", "parse"),
            (
                "{\"flag\":\"203\",\"content\":\"请先登录\"}",
                "auth_required",
            ),
            (
                "{\"flag\":\"500\",\"content\":\"操作太频繁\"}",
                "rate_limited",
            ),
            ("{\"flag\":\"500\",\"content\":\"系统繁忙\"}", "parse"),
        ] {
            assert_eq!(parse_inroom(body, "1").unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn mobile_headers_carry_the_user_cookie_only_when_set() {
        assert!(mobile_headers(None).unwrap().get(COOKIE).is_none());
        assert_eq!(
            mobile_headers(Some("_v6=1")).unwrap().get(COOKIE).unwrap(),
            "_v6=1"
        );
        assert_eq!(
            mobile_headers(Some("a=1\nb=2")).unwrap_err().kind(),
            "config"
        );
        assert_eq!(
            inroom_body("10000001"),
            "av=3.1&encpass=&logiuid=&project=v6iphone&rate=1&rid=&ruid=10000001"
        );
    }
}
