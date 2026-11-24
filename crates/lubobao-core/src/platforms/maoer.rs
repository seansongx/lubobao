//! Maoer FM (猫耳FM, audio live rooms) adapter, following StreamCap's
//! `MaoerFMHandler` and StreamGet v4.0.10 `MaoerLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/maoer/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily; see `THIRD_PARTY_NOTICES.md`.
//!
//! `fm.missevan.com/api/v2/live/{room_id}` answers with the anchor
//! (`info.creator.username`), the title (`info.room.name`), the live flag
//! (`info.room.status.broadcasting`) and one FLV and one HLS pull URL
//! (`info.room.channel`). StreamGet records the FLV stream.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header, user_cookie,
};

/// `BaseLiveStream._get_pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36 Edg/121.0.0.0";
const ROOM_PAGE: &str = "https://fm.missevan.com/live/";
const API: &str = "https://fm.missevan.com/api/v2/live/";
/// `code` the API answers for a room that does not exist.
const CODE_ROOM_MISSING: i64 = 500030004;

/// Recognize a room link and return the room id, without any network access.
///
/// StreamCap registers `fm.missevan.com/`; StreamGet takes the last path
/// segment as the room id. Only `fm.missevan.com/live/{id}` is accepted.
pub fn parse_link(input: &str) -> Result<String, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的猫耳FM直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    if url.host_str().map(str::to_ascii_lowercase).as_deref() != Some("fm.missevan.com") {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        ["live", id] if is_room_id(id) => Ok(id.to_string()),
        _ => Err(invalid()),
    }
}

fn is_room_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 19 && text.bytes().all(|b| b.is_ascii_digit())
}

/// Canonical room URL for a room id.
pub fn room_url(room_id: &str) -> String {
    format!("{ROOM_PAGE}{room_id}")
}

/// What the room API says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub room_id: String,
    pub anchor_id: Option<String>,
    pub anchor_name: String,
    pub title: Option<String>,
    pub flv_url: Option<String>,
    pub hls_url: Option<String>,
    pub is_live: bool,
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn number_or_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// Parse the room API answer for room `room_id` from the link.
pub fn parse_room(body: &str, room_id: &str) -> Result<Room, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited("猫耳FM接口返回的不是 JSON，可能触发风控".to_string())
        } else {
            PlatformError::Parse("猫耳FM接口返回的数据无法解析".to_string())
        }
    })?;
    let code = json.get("code").and_then(Value::as_i64);
    if code == Some(CODE_ROOM_MISSING) {
        return Err(PlatformError::InvalidUrl(format!(
            "猫耳FM没有找到这个直播间（{room_id}），请检查链接"
        )));
    }
    if code != Some(0) {
        let detail = json
            .get("info")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        let message = format!(
            "猫耳FM接口返回错误 {}：{detail}",
            code.map(|code| code.to_string()).unwrap_or_default()
        );
        return Err(if detail.contains("登录") {
            PlatformError::AuthRequired(message)
        } else if detail.contains("频繁") || detail.contains("风控") {
            PlatformError::RateLimited(message)
        } else {
            PlatformError::Parse(message)
        });
    }
    let info = json
        .get("info")
        .filter(|info| info.is_object())
        .ok_or_else(|| PlatformError::Parse("猫耳FM接口缺少 info".to_string()))?;
    let anchor_name =
        text(info.pointer("/creator/username")).unwrap_or_else(|| format!("猫耳FM {room_id}"));
    let anchor_id = number_or_text(info.pointer("/creator/user_id"));
    // StreamGet: only reads the status when `room` exists.
    let room = info.get("room").filter(|room| room.is_object());
    let live = room
        .and_then(|room| room.pointer("/status/broadcasting"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let title = room.and_then(|room| text(room.get("name")));
    let channel = |key: &str| {
        if live {
            room.and_then(|room| text(room.pointer(&format!("/channel/{key}"))))
        } else {
            None
        }
    };
    let flv_url = channel("flv_pull_url");
    let hls_url = channel("hls_pull_url");
    Ok(Room {
        room_id: room_id.to_string(),
        anchor_id,
        anchor_name,
        title,
        is_live: live && (flv_url.is_some() || hls_url.is_some()),
        flv_url,
        hls_url,
    })
}

/// Turn the room data into `LiveInfo`. StreamGet records the FLV stream
/// (`record_url = flv_url`), HLS only when there is no FLV.
pub fn build_live_info(room: Room, quality: Quality) -> LiveInfo {
    let variants = if room.is_live {
        vec![StreamVariant {
            name: "原画".to_string(),
            quality: Some(Quality::Od),
            flv_url: room.flv_url,
            hls_url: room.hls_url,
        }]
    } else {
        Vec::new()
    };
    // One stream only: every tier gets it.
    let selected = variants.first().and_then(|variant| {
        let record_url = variant
            .flv_url
            .clone()
            .or_else(|| variant.hls_url.clone())?;
        Some(SelectedStream {
            requested: quality,
            variant: variant.clone(),
            record_url,
        })
    });
    LiveInfo {
        platform: Platform::Maoer,
        live_url: Some(room_url(&room.room_id)),
        anchor_id: room.anchor_id,
        room_id: room.room_id,
        anchor_name: room.anchor_name,
        title: room.title,
        is_live: selected.is_some(),
        variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Maoer streams: the User-Agent only.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("User-Agent".to_string(), USER_AGENT_VALUE.to_string())])
}

/// `BaseLiveStream._get_pc_headers`: User-Agent, Accept-Language and the user
/// cookie (if any).
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

/// Maoer FM adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct MaoerAdapter {
    clients: ClientCache,
}

impl MaoerAdapter {
    pub fn new() -> Self {
        MaoerAdapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let room_id = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let api = format!("{API}{room_id}");
        let response = client
            .get(&api)
            .headers(pc_headers(user_cookie(options))?)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), &api));
        }
        let room = parse_room(&response.text().await?, &room_id)?;
        Ok(build_live_info(room, options.quality))
    }
}

impl PlatformAdapter for MaoerAdapter {
    fn platform(&self) -> Platform {
        Platform::Maoer
    }

    fn matches(&self, url: &str) -> bool {
        parse_link(url).is_ok()
    }

    fn normalize_url(&self, url: &str) -> Option<String> {
        parse_link(url).ok().map(|id| room_url(&id))
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

    const LIVE: &str = include_str!("../../tests/fixtures/maoer/live.json");
    const OFFLINE: &str = include_str!("../../tests/fixtures/maoer/offline.json");
    const NO_ROOM: &str = include_str!("../../tests/fixtures/maoer/no_room.json");
    const MISSING: &str = include_str!("../../tests/fixtures/maoer/missing.json");

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let adapter = MaoerAdapter::new();
        for input in [
            "https://fm.missevan.com/live/868895007",
            "fm.missevan.com/live/868895007/",
            "http://FM.MISSEVAN.COM/live/868895007?from=share#x",
        ] {
            assert_eq!(parse_link(input).unwrap(), "868895007", "{input}");
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            adapter
                .normalize_url("http://fm.missevan.com/live/868895007?x=1")
                .as_deref(),
            Some("https://fm.missevan.com/live/868895007")
        );
        let found = super::super::adapter_for("https://fm.missevan.com/live/1").unwrap();
        assert_eq!(found.platform(), Platform::Maoer);
        assert_eq!(Platform::Maoer.key(), "maoerfm");
        assert_eq!(Platform::Maoer.display_name(), "猫耳FM直播");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        for input in [
            "https://fm.missevan.com/",
            "https://fm.missevan.com/live/",
            "https://fm.missevan.com/live/abc",
            "https://fm.missevan.com/live/1/2",
            "https://fm.missevan.com/sound/123",
            "https://www.missevan.com/live/1",
            "https://fm.missevan.com.evil.example/live/1",
            "ftp://fm.missevan.com/live/1",
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
    fn live_room_records_flv_like_streamget() {
        let room = parse_room(LIVE, "869138407").unwrap();
        assert!(room.is_live);
        assert_eq!(room.anchor_name, "测试主播🎤");
        assert_eq!(room.anchor_id.as_deref(), Some("34312404"));
        let info = build_live_info(room, Quality::Ld);
        assert_eq!(info.title.as_deref(), Some("测试直播间标题"));
        let selected = info.selected.unwrap();
        assert!(selected.record_url.contains("maoer_1_100.flv"));
        assert!(
            selected
                .variant
                .hls_url
                .unwrap()
                .contains("maoer_1_100.m3u8")
        );
    }

    #[test]
    fn offline_missing_and_broken_answers() {
        let offline = parse_room(OFFLINE, "868895007").unwrap();
        assert!(!offline.is_live);
        assert_eq!(offline.anchor_name, "下播主播");
        assert!(offline.flv_url.is_none());
        let info = build_live_info(offline, Quality::Od);
        assert!(!info.is_live && info.selected.is_none());
        let never = parse_room(NO_ROOM, "1").unwrap();
        assert!(!never.is_live);
        assert_eq!(never.title, None);
        let missing = parse_room(MISSING, "1").unwrap_err();
        assert_eq!(missing.kind(), "invalid_url");
        for (body, kind) in [
            ("", "rate_limited"),
            ("<html>", "rate_limited"),
            ("{\"code\":", "parse"),
            ("{\"code\":0}", "parse"),
            ("{\"code\":1,\"info\":\"请先登录\"}", "auth_required"),
            ("{\"code\":1,\"info\":\"服务异常\"}", "parse"),
        ] {
            assert_eq!(parse_room(body, "1").unwrap_err().kind(), kind, "{body}");
        }
    }
}
