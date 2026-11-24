//! 17Live (17直播) adapter, following StreamCap's `YiqiLiveHandler` and
//! StreamGet v4.0.10 `YiqiLiveStream`. The platform key is `17live` (it
//! starts with a digit, like `6room`), the Rust type is `Platform::Live17`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/yiqilive/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The request headers, the `user/room/{id}` name lookup, the
//! `lives/{id}/viewers/alive` live check (`status == 2`) and taking
//! `pullURLsInfo.rtmpURLs[0].urlHighQuality` as the recorded FLV stream
//! follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Errors come as `{"errorCode": …, "errorMessage": …}` with HTTP 420, so
//! the body is read whatever the HTTP status: `7` is "not found" (the room
//! does not exist), `37` is "stream has been already ended" (not live).

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{
    ACCEPT_LANGUAGE, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT,
};
use serde_json::{Value, json};

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header, user_cookie,
};

/// `YiqiLiveStream._get_pc_headers` User-Agent.
pub const USER_AGENT_VALUE: &str = "ios/7.830 (ios 17.0; ; iPhone 15 (A2846/A3089/A3090/A3092))";
const ORIGIN_VALUE: &str = "https://17.live";
const REFERER_VALUE: &str = "https://17.live/";
const API: &str = "https://wap-api.17app.co/api/v1";
const ERROR_NOT_FOUND: i64 = 7;
const ERROR_STREAM_ENDED: i64 = 37;

/// Room ids (`roomID`) are digits.
fn is_room_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 12 && text.bytes().all(|b| b.is_ascii_digit())
}

/// Canonical room page.
pub fn room_url(room_id: &str) -> String {
    format!("https://17.live/en/live/{room_id}")
}

/// Recognize a 17Live link and return the room id, without any network
/// access.
///
/// StreamCap registers `17.live/`; StreamGet takes the last path segment as
/// the room id. Accepted: `17.live/{lang}/live/{id}`, `17.live/live/{id}` and
/// the profile page `17.live/{lang}/profile/r/{id}`.
pub fn parse_link(input: &str) -> Result<String, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的 17Live 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "17.live" && host != "www.17.live" {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    let id = match segments.as_slice() {
        ["live", id] | [_, "live", id] | ["profile", "r", id] | [_, "profile", "r", id]
            if is_room_id(id) =>
        {
            Some(*id)
        }
        _ => None,
    };
    id.map(str::to_string).ok_or_else(|| {
        PlatformError::InvalidUrl(format!(
            "17Live 链接里没有直播间号，请复制形如 17.live/zh-Hant/live/房间号 的链接: {trimmed}"
        ))
    })
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn json_body(body: &str) -> Result<Value, PlatformError> {
    serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited("17Live 接口返回的不是 JSON，可能触发了风控".to_string())
        } else {
            PlatformError::Parse("17Live 接口返回的数据无法解析".to_string())
        }
    })
}

/// What `user/room/{id}` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub room_id: String,
    pub user_id: Option<String>,
    pub name: String,
}

/// Parse `user/room/{id}`; an `errorCode` 7 answer means the room does not
/// exist.
pub fn parse_anchor(body: &str, room_id: &str) -> Result<Anchor, PlatformError> {
    let json = json_body(body)?;
    if let Some(code) = json.get("errorCode").and_then(Value::as_i64) {
        let message = text(json.get("errorMessage")).unwrap_or_default();
        return Err(if code == ERROR_NOT_FOUND {
            PlatformError::InvalidUrl(format!("17Live 没有找到直播间（{room_id}），请检查链接"))
        } else {
            PlatformError::Parse(format!("17Live 接口返回错误 {code}：{message}"))
        });
    }
    let name = text(json.get("displayName"))
        .or_else(|| text(json.get("openID")))
        .ok_or_else(|| PlatformError::Parse("17Live 接口缺少主播名".to_string()))?;
    Ok(Anchor {
        room_id: room_id.to_string(),
        user_id: text(json.get("userID")),
        name,
    })
}

/// What `lives/{id}/viewers/alive` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alive {
    Live { flv_url: String },
    Offline,
}

/// Parse the live check. `errorCode` 37 (the stream ended) and any other
/// `status` than 2 mean the room is not live.
pub fn parse_alive(body: &str) -> Result<Alive, PlatformError> {
    let json = json_body(body)?;
    if let Some(code) = json.get("errorCode").and_then(Value::as_i64) {
        let message = text(json.get("errorMessage")).unwrap_or_default();
        return match code {
            ERROR_STREAM_ENDED => Ok(Alive::Offline),
            ERROR_NOT_FOUND => Err(PlatformError::InvalidUrl(
                "17Live 没有找到这个直播间，请检查链接".to_string(),
            )),
            _ => Err(PlatformError::Parse(format!(
                "17Live 接口返回错误 {code}：{message}"
            ))),
        };
    }
    if json.get("status").and_then(Value::as_i64) != Some(2) {
        return Ok(Alive::Offline);
    }
    let flv_url = text(json.pointer("/pullURLsInfo/rtmpURLs/0/urlHighQuality"))
        .or_else(|| text(json.pointer("/rtmpUrls/0/urlHighQuality")))
        .ok_or_else(|| PlatformError::Parse("17Live 没有返回播放地址".to_string()))?;
    Ok(Alive::Live { flv_url })
}

/// Turn the room into `LiveInfo`. StreamGet records the one
/// `urlHighQuality` FLV stream, so every tier gets it.
pub fn build_live_info(anchor: &Anchor, alive: &Alive, quality: Quality) -> LiveInfo {
    let variants = match alive {
        Alive::Live { flv_url } => vec![StreamVariant {
            name: "原画".to_string(),
            quality: Some(Quality::Od),
            flv_url: Some(flv_url.clone()),
            hls_url: None,
        }],
        Alive::Offline => Vec::new(),
    };
    let selected = variants.first().map(|variant| SelectedStream {
        requested: quality,
        variant: variant.clone(),
        record_url: variant.flv_url.clone().unwrap_or_default(),
    });
    LiveInfo {
        platform: Platform::Live17,
        room_id: anchor.room_id.clone(),
        anchor_id: anchor.user_id.clone(),
        live_url: Some(room_url(&anchor.room_id)),
        anchor_name: anchor.name.clone(),
        title: None,
        is_live: selected.is_some(),
        variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling 17Live streams: User-Agent and Referer (StreamCap's
/// `get_headers_params` sends a `referer` for `17live`).
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Referer".to_string(), REFERER_VALUE.to_string()),
    ])
}

/// `_get_pc_headers`: Origin, Referer, User-Agent and the user cookie.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(ORIGIN, HeaderValue::from_static(ORIGIN_VALUE));
    headers.insert(REFERER, HeaderValue::from_static(REFERER_VALUE));
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US"));
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// Body of the live-check request.
pub fn alive_body(room_id: &str) -> String {
    json!({ "liveStreamID": room_id }).to_string()
}

/// 17Live adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct Live17Adapter {
    clients: ClientCache,
}

impl Live17Adapter {
    pub fn new() -> Self {
        Live17Adapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let room_id = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);
        // The answers carry the error in the body, with HTTP 420.
        let read = |status: reqwest::StatusCode, body: &str, api: &str| {
            if !status.is_success() && !body.trim_start().starts_with('{') {
                Err(classify_http_status(status.as_u16(), api))
            } else {
                Ok(())
            }
        };

        let api = format!("{API}/user/room/{room_id}");
        let response = client.get(&api).headers(pc_headers(cookie)?).send().await?;
        let status = response.status();
        let body = response.text().await?;
        read(status, &body, &api)?;
        let anchor = parse_anchor(&body, &room_id)?;

        let api = format!("{API}/lives/{room_id}/viewers/alive");
        let mut headers = pc_headers(cookie)?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let response = client
            .post(&api)
            .headers(headers)
            .body(alive_body(&room_id))
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        read(status, &body, &api)?;
        let alive = parse_alive(&body)?;
        Ok(build_live_info(&anchor, &alive, options.quality))
    }
}

impl PlatformAdapter for Live17Adapter {
    fn platform(&self) -> Platform {
        Platform::Live17
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

    const USER_ROOM: &str = include_str!("../../tests/fixtures/17live/user_room.json");
    const ALIVE_LIVE: &str = include_str!("../../tests/fixtures/17live/alive_live.json");
    const ALIVE_ENDED: &str = include_str!("../../tests/fixtures/17live/alive_ended.json");
    const NOT_FOUND: &str = include_str!("../../tests/fixtures/17live/not_found.json");

    #[test]
    fn recognizes_and_normalizes_links() {
        let adapter = Live17Adapter::new();
        for input in [
            "https://17.live/en/live/29796292",
            "https://17.live/zh-Hant/live/29796292?utm_source=x",
            "17.live/live/29796292/",
            "https://17.live/ja/profile/r/29796292",
            "https://www.17.live/en/live/29796292",
        ] {
            assert_eq!(parse_link(input).unwrap(), "29796292", "{input}");
            assert!(adapter.matches(input), "{input}");
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some("https://17.live/en/live/29796292"),
                "{input}"
            );
        }
        let live17 = super::super::adapter_for("https://17.live/en/live/29796292").unwrap();
        assert_eq!(live17.platform(), Platform::Live17);
        assert_eq!(Platform::Live17.key(), "17live");
        assert_eq!(Platform::Live17.display_name(), "17Live");
        // Stored platform names use the key.
        assert_eq!(
            serde_json::to_string(&Platform::Live17).unwrap(),
            "\"17live\""
        );
        assert_eq!(
            serde_json::from_str::<Platform>("\"17live\"").unwrap(),
            Platform::Live17
        );
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        for input in [
            "https://17.live/",
            "https://17.live/en/",
            "https://17.live/en/live/abc",
            "https://17.live/en/live/",
            "https://17.live.evil.example/en/live/29796292",
            "https://not17.live/en/live/29796292",
            "ftp://17.live/en/live/29796292",
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
    fn live_room_records_the_high_quality_flv_for_every_tier() {
        let anchor = parse_anchor(USER_ROOM, "29796292").unwrap();
        assert_eq!(anchor.name, "小蛋黃egg");
        assert_eq!(
            anchor.user_id.as_deref(),
            Some("68ecb40c-d6ef-4a79-b839-cd0ca9cdd3e6")
        );
        let alive = parse_alive(ALIVE_LIVE).unwrap();
        for quality in Quality::ALL {
            let info = build_live_info(&anchor, &alive, quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Live17);
            assert_eq!(info.room_id, "29796292");
            assert_eq!(info.anchor_name, "小蛋黃egg");
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://17.live/en/live/29796292")
            );
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert_eq!(
                selected.record_url,
                "http://tencent-global-pull-rtmp.17app.co/live/68ecb40c-d6ef-4a79-b839-cd0ca9cdd3e6.flv"
            );
            assert_eq!(
                selected.source_url(false, false).0,
                super::super::SourceFormat::Flv
            );
            assert_eq!(info.stream_headers["Referer"], "https://17.live/");
        }
    }

    #[test]
    fn offline_missing_and_broken_answers() {
        let anchor = parse_anchor(USER_ROOM, "29796292").unwrap();
        assert_eq!(parse_alive(ALIVE_ENDED).unwrap(), Alive::Offline);
        let info = build_live_info(&anchor, &Alive::Offline, Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        // `status` other than 2 is not live.
        assert_eq!(
            parse_alive(r#"{"status":1,"pullURLsInfo":{"rtmpURLs":[]}}"#).unwrap(),
            Alive::Offline
        );
        assert_eq!(
            parse_anchor(NOT_FOUND, "999").unwrap_err().kind(),
            "invalid_url"
        );
        assert_eq!(parse_alive(NOT_FOUND).unwrap_err().kind(), "invalid_url");
        assert_eq!(
            parse_alive(r#"{"errorCode":99,"errorMessage":"x"}"#)
                .unwrap_err()
                .kind(),
            "parse"
        );
        // Live without a pull URL.
        assert_eq!(
            parse_alive(r#"{"status":2,"pullURLsInfo":{"rtmpURLs":[]}}"#)
                .unwrap_err()
                .kind(),
            "parse"
        );
        for (body, kind) in [("<html>", "rate_limited"), ("{", "parse")] {
            assert_eq!(parse_alive(body).unwrap_err().kind(), kind, "{body}");
            assert_eq!(parse_anchor(body, "1").unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn requests_carry_streamget_headers() {
        let headers = pc_headers(None).unwrap();
        assert_eq!(headers.get(ORIGIN).unwrap(), ORIGIN_VALUE);
        assert_eq!(headers.get(USER_AGENT).unwrap(), USER_AGENT_VALUE);
        assert!(headers.get(COOKIE).is_none());
        assert_eq!(pc_headers(Some("a=1")).unwrap().get(COOKIE).unwrap(), "a=1");
        assert_eq!(pc_headers(Some("a\nb")).unwrap_err().kind(), "config");
        assert_eq!(alive_body("29796292"), r#"{"liveStreamID":"29796292"}"#);
    }
}
