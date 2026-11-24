//! Baidu Live adapter, following StreamCap's `BaiduHandler` and StreamGet
//! v4.0.10 `BaiduLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/baidu/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily; see `THIRD_PARTY_NOTICES.md`.
//!
//! One request does it all: the Baidu app endpoint
//! `mbd.baidu.com/searchbox?cmd=371&action=star…` answers with the host's
//! name, a `status` (`"0"` live, anything else not live; `"20"` is a replay)
//! and the stream list. StreamGet rewrites each stream to the public
//! `hls.liveshow.bdstatic.com/live/{file}.m3u8` host and records HLS only.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

/// `BaseLiveStream._get_pc_headers` User-Agent.
pub const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36 Edg/121.0.0.0";
const API: &str = "https://mbd.baidu.com/searchbox";
const HLS_PREFIX: &str = "https://hls.liveshow.bdstatic.com/live/";
/// StreamGet picks one of three fixed H5 device ids at random; all three are
/// accepted, so the first is used (and its id also goes into the `data` JSON,
/// as upstream).
const DEVICE_ID: &str = "h5-683e85bdf741bf2492586f7ca39bf465";
const ROOM_PAGE: &str = "https://live.baidu.com/m/media/pclive/pchome/live.html?room_id=";

/// Recognize a room link and return the room id, without any network access.
///
/// StreamCap registers `live.baidu.com/`; StreamGet reads `room_id=(.*?)&`,
/// which fails when `room_id` is the last parameter. The query is parsed
/// properly here instead.
pub fn parse_link(input: &str) -> Result<String, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的百度直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    if url.host_str().map(str::to_ascii_lowercase).as_deref() != Some("live.baidu.com") {
        return Err(invalid());
    }
    url.query_pairs()
        .find(|(key, _)| key == "room_id")
        .map(|(_, value)| value.into_owned())
        .filter(|id| is_room_id(id))
        .ok_or_else(invalid)
}

fn is_room_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 19 && text.bytes().all(|b| b.is_ascii_digit())
}

/// Canonical room URL for a room id.
pub fn room_url(room_id: &str) -> String {
    format!("{ROOM_PAGE}{room_id}")
}

/// Request URL for the room, with StreamGet's parameters.
pub fn api_url(room_id: &str, millis: u128) -> String {
    let data = format!(
        "{{\"data\":{{\"room_id\":\"{room_id}\",\"device_id\":\"{DEVICE_ID}\",\"source_type\":0,\"osname\":\"baiduboxapp\"}},\"replay_slice\":0,\"nid\":\"\",\"schemeParams\":{{\"src_pre\":\"pc\",\"src_suf\":\"other\",\"bd_vid\":\"\",\"share_uid\":\"\",\"share_cuk\":\"\",\"share_ecid\":\"\",\"zb_tag\":\"\",\"shareTaskInfo\":\"{{\\\"room_id\\\":\\\"{room_id}\\\"}}\",\"share_from\":\"\",\"ext_params\":\"\",\"nid\":\"\"}}}}"
    );
    let mut url = Url::parse(API).expect("valid API URL");
    url.query_pairs_mut()
        .append_pair("cmd", "371")
        .append_pair("action", "star")
        .append_pair("service", "bdbox")
        .append_pair("osname", "baiduboxapp")
        .append_pair("data", &data)
        .append_pair("ua", "360_740_ANDROID_0")
        .append_pair("bd_vid", "")
        .append_pair("uid", DEVICE_ID)
        .append_pair("_", &millis.to_string());
    url.to_string()
}

/// What the room endpoint says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomData {
    pub room_id: String,
    pub anchor_name: String,
    pub title: Option<String>,
    pub is_live: bool,
    /// HLS variants in the order the platform lists them (best first).
    pub variants: Vec<StreamVariant>,
}

/// Parse the room endpoint's answer.
pub fn parse_room(body: &str, room_id: &str) -> Result<RoomData, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited("百度直播接口返回的不是 JSON，可能触发风控".to_string())
        } else {
            PlatformError::Parse("百度直播接口返回的数据无法解析".to_string())
        }
    })?;
    let entry = json
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| PlatformError::Parse("百度直播接口返回的数据无法解析".to_string()))?
        .values()
        .next()
        .ok_or_else(|| PlatformError::Parse("百度直播接口没有返回直播间数据".to_string()))?;
    if !entry.is_object() {
        return Err(PlatformError::InvalidUrl(format!(
            "百度直播没有找到这个直播间（{room_id}），请检查链接"
        )));
    }
    let anchor_name = entry
        .pointer("/host/name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("百度直播 {room_id}"));
    // StreamGet: `data['status'] == "0"`.
    let live = match entry.get("status") {
        Some(Value::String(text)) => text == "0",
        Some(Value::Number(number)) => number.as_i64() == Some(0),
        _ => false,
    };
    let title = entry
        .pointer("/video/title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    let variants = if live {
        parse_variants(entry.get("video"))?
    } else {
        Vec::new()
    };
    Ok(RoomData {
        room_id: room_id.to_string(),
        anchor_name,
        title,
        is_live: live && !variants.is_empty(),
        variants,
    })
}

/// File name (`stream_….m3u8`) of an HLS URL without its query.
fn hls_file(url: &str) -> Option<String> {
    let file = url.split('?').next()?.rsplit('/').next()?;
    (!file.is_empty()).then(|| file.to_string())
}

/// FLV URL → `{stream}.m3u8`, as StreamGet does for `url_clarity_list`.
fn flv_to_hls_file(url: &str) -> Option<String> {
    let file = url.rsplit('/').next()?;
    let stem = file.rsplit_once('.').map(|(stem, _)| stem)?;
    (!stem.is_empty()).then(|| format!("{stem}.m3u8"))
}

fn parse_variants(video: Option<&Value>) -> Result<Vec<StreamVariant>, PlatformError> {
    let video = video.ok_or_else(|| PlatformError::Parse("百度直播缺少 video".to_string()))?;
    let mut variants = Vec::new();
    let clarity = video
        .get("url_clarity_list")
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty());
    if let Some(list) = clarity {
        for (index, item) in list.iter().enumerate() {
            let flv = item.pointer("/urls/flv").and_then(Value::as_str);
            let Some(file) = flv.and_then(flv_to_hls_file) else {
                continue;
            };
            let name = item
                .get("title")
                .or_else(|| item.get("description"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("清晰度 {}", index + 1));
            variants.push(variant(name, &file, variants.len()));
        }
    } else if let Some(list) = video.get("url_list").and_then(Value::as_array) {
        for item in list {
            let hls = item
                .pointer("/urls/0/hls")
                .and_then(Value::as_str)
                .and_then(hls_file);
            let Some(file) = hls else { continue };
            let name = match item.get("resolution") {
                Some(Value::Number(number)) => format!("{number}p"),
                _ => item
                    .get("description")
                    .and_then(Value::as_str)
                    .map(|text| text.trim().trim_end_matches(':').to_string())
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| format!("清晰度 {}", variants.len() + 1)),
            };
            variants.push(variant(name, &file, variants.len()));
        }
    }
    Ok(variants)
}

fn variant(name: String, file: &str, index: usize) -> StreamVariant {
    StreamVariant {
        name,
        quality: Quality::from_index(index),
        flv_url: None,
        hls_url: Some(format!("{HLS_PREFIX}{file}")),
    }
}

/// Turn the room data into `LiveInfo`.
pub fn build_live_info(room: RoomData, quality: Quality) -> LiveInfo {
    let selected = select_variant_index(room.variants.len(), quality)
        .and_then(|index| selected_stream(quality, &room.variants[index]));
    LiveInfo {
        platform: Platform::Baidu,
        live_url: Some(room_url(&room.room_id)),
        anchor_id: None,
        room_id: room.room_id,
        anchor_name: room.anchor_name,
        title: room.title,
        is_live: selected.is_some(),
        variants: room.variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Baidu streams: the User-Agent only.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("User-Agent".to_string(), USER_AGENT_VALUE.to_string())])
}

/// `BaiduLiveStream.pc_headers`: User-Agent and the user cookie (if any).
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    if let Some(cookie) = cookie {
        headers.insert(reqwest::header::COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// Baidu adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct BaiduAdapter {
    clients: ClientCache,
}

impl BaiduAdapter {
    pub fn new() -> Self {
        BaiduAdapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let room_id = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default();
        let api = api_url(&room_id, millis);
        let response = client
            .get(&api)
            .headers(pc_headers(user_cookie(options))?)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), API));
        }
        let room = parse_room(&response.text().await?, &room_id)?;
        Ok(build_live_info(room, options.quality))
    }
}

impl PlatformAdapter for BaiduAdapter {
    fn platform(&self) -> Platform {
        Platform::Baidu
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

    const LIVE: &str = include_str!("../../tests/fixtures/baidu/live.json");
    const LIVE_CLARITY: &str = include_str!("../../tests/fixtures/baidu/live_clarity.json");
    const REPLAY: &str = include_str!("../../tests/fixtures/baidu/replay.json");
    const MISSING: &str = include_str!("../../tests/fixtures/baidu/missing.json");

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let adapter = BaiduAdapter::new();
        for input in [
            "https://live.baidu.com/m/media/pclive/pchome/live.html?room_id=9175031377&tab_category",
            "live.baidu.com/m/media/pclive/pchome/live.html?room_id=9175031377",
            "http://LIVE.BAIDU.COM/m/media/multipage/liveshow/index/abc?source=x&room_id=9175031377",
        ] {
            assert_eq!(parse_link(input).unwrap(), "9175031377", "{input}");
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            adapter
                .normalize_url("https://live.baidu.com/m/media/pclive/pchome/live.html?room_id=9175031377&tab_category")
                .as_deref(),
            Some("https://live.baidu.com/m/media/pclive/pchome/live.html?room_id=9175031377")
        );
        let found = super::super::adapter_for("https://live.baidu.com/x?room_id=1").unwrap();
        assert_eq!(found.platform(), Platform::Baidu);
        assert_eq!(Platform::Baidu.key(), "baidu");
        assert_eq!(Platform::Baidu.display_name(), "百度直播");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        for input in [
            "https://live.baidu.com/",
            "https://live.baidu.com/live.html?room_id=",
            "https://live.baidu.com/live.html?room_id=abc",
            "https://live.baidu.com.evil.example/live.html?room_id=1",
            "https://www.baidu.com/live.html?room_id=1",
            "ftp://live.baidu.com/live.html?room_id=1",
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
    fn live_room_lists_hls_variants_best_first() {
        let room = parse_room(LIVE, "100").unwrap();
        assert!(room.is_live);
        assert_eq!(room.anchor_name, "测试主播🎤");
        assert_eq!(room.title.as_deref(), Some("测试直播间标题"));
        let urls: Vec<_> = room
            .variants
            .iter()
            .map(|v| v.hls_url.clone().unwrap())
            .collect();
        assert_eq!(
            urls,
            [
                "https://hls.liveshow.bdstatic.com/live/stream_bduid_1_100-L3.m3u8",
                "https://hls.liveshow.bdstatic.com/live/stream_bduid_1_100-L4.m3u8"
            ]
        );
        assert_eq!(room.variants[0].name, "720p");
        let info = build_live_info(room, Quality::Ld);
        // Padded list: the lowest tier falls back to the last entry.
        assert!(info.selected.unwrap().record_url.ends_with("-L4.m3u8"));
    }

    #[test]
    fn clarity_list_streams_become_hls_files() {
        let room = parse_room(LIVE_CLARITY, "200").unwrap();
        assert!(room.is_live);
        assert_eq!(room.variants[0].name, "超清");
        assert_eq!(
            room.variants[0].hls_url.as_deref(),
            Some("https://hls.liveshow.bdstatic.com/live/stream_bduid_2_200-L2.m3u8")
        );
    }

    #[test]
    fn replay_and_missing_rooms() {
        let replay = parse_room(REPLAY, "1").unwrap();
        assert!(!replay.is_live);
        assert_eq!(replay.anchor_name, "回放主播");
        let info = build_live_info(replay, Quality::Od);
        assert!(!info.is_live && info.selected.is_none());
        assert_eq!(parse_room(MISSING, "1").unwrap_err().kind(), "invalid_url");
        for (body, kind) in [
            ("", "rate_limited"),
            ("<html>", "rate_limited"),
            ("{\"data\":", "parse"),
            ("{}", "parse"),
            ("{\"data\":{}}", "parse"),
        ] {
            assert_eq!(parse_room(body, "1").unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn api_url_carries_room_id() {
        let url = api_url("123", 42);
        assert!(url.starts_with("https://mbd.baidu.com/searchbox?cmd=371&action=star"));
        assert!(url.contains("room_id%22%3A%22123%22"));
        assert!(url.ends_with("&_=42"));
    }
}
