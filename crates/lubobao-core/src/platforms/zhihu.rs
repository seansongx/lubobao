//! Zhihu Live adapter, following StreamCap's `ZhihuHandler` and StreamGet
//! v4.0.10 `ZhihuLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/zhihu/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily; see `THIRD_PARTY_NOTICES.md`.
//!
//! The theater page `www.zhihu.com/theater/{id}` embeds its state as JSON in
//! `<script id="js-initialData" type="text/json">`. `theaters[{id}]` holds the
//! anchor (`actor.name`), the title (`theme`) and `drama.status` (`1` live);
//! while live `drama.playInfo` carries one FLV (`playUrl`) and one HLS
//! (`hlsUrl`) URL. StreamGet records the HLS stream.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, selected_stream, user_cookie,
};

/// `BaseLiveStream._get_mobile_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Linux; Android 11; SAMSUNG SM-G973U) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/14.2 Chrome/87.0.4280.141 Mobile Safari/537.36";
const ROOM_PAGE: &str = "https://www.zhihu.com/theater/";
const DATA_START: &str = "<script id=\"js-initialData\" type=\"text/json\">";

/// Recognize a room link and return the theater id, without any network
/// access.
///
/// StreamCap registers `zhihu.com/`; StreamGet needs `/theater/` in the link
/// and takes the last path segment as the id. Only
/// `zhihu.com/theater/{id}` is accepted; `drama_id` (one per broadcast) is
/// dropped.
pub fn parse_link(input: &str) -> Result<String, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的知乎直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "zhihu.com" && host != "www.zhihu.com" {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        ["theater", id] if is_theater_id(id) => Ok(id.to_string()),
        _ => Err(invalid()),
    }
}

fn is_theater_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 19 && text.bytes().all(|b| b.is_ascii_digit())
}

/// Canonical room URL for a theater id.
pub fn room_url(theater_id: &str) -> String {
    format!("{ROOM_PAGE}{theater_id}")
}

/// What the theater page says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theater {
    pub theater_id: String,
    pub anchor_name: String,
    pub title: Option<String>,
    pub flv_url: Option<String>,
    pub hls_url: Option<String>,
    pub is_live: bool,
}

/// Parse the theater page for `theater_id`.
pub fn parse_page(html: &str, theater_id: &str) -> Result<Theater, PlatformError> {
    let start = html.find(DATA_START).ok_or_else(|| {
        PlatformError::Parse("知乎直播页面缺少 js-initialData，页面结构可能变了".to_string())
    })? + DATA_START.len();
    let end = html[start..]
        .find("</script>")
        .ok_or_else(|| PlatformError::Parse("知乎直播页面的数据不完整".to_string()))?;
    let json: Value = serde_json::from_str(&html[start..start + end])
        .map_err(|_| PlatformError::Parse("知乎直播页面的数据无法解析".to_string()))?;
    // A theater that does not exist has no `theater` state at all.
    let theater = json
        .pointer(&format!("/initialState/theater/theaters/{theater_id}"))
        .filter(|theater| theater.is_object())
        .ok_or_else(|| {
            PlatformError::InvalidUrl(format!(
                "知乎直播没有找到这个直播间（{theater_id}），请检查链接"
            ))
        })?;
    let anchor_name = theater
        .pointer("/actor/name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("知乎直播 {theater_id}"));
    let title = theater
        .get("theme")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    // StreamGet: `drama.status == 1`.
    let live = theater.pointer("/drama/status").and_then(Value::as_i64) == Some(1);
    let play = |key: &str| {
        theater
            .pointer("/drama/playInfo")
            .and_then(|info| info.get(key))
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
    };
    let (flv_url, hls_url) = if live {
        (play("playUrl"), play("hlsUrl"))
    } else {
        (None, None)
    };
    Ok(Theater {
        theater_id: theater_id.to_string(),
        anchor_name,
        title,
        is_live: live && (flv_url.is_some() || hls_url.is_some()),
        flv_url,
        hls_url,
    })
}

/// Turn the page data into `LiveInfo`.
pub fn build_live_info(theater: Theater, quality: Quality) -> LiveInfo {
    let variants = if theater.is_live {
        vec![StreamVariant {
            name: "原画".to_string(),
            quality: Some(Quality::Od),
            flv_url: theater.flv_url,
            hls_url: theater.hls_url,
        }]
    } else {
        Vec::new()
    };
    // One stream only: every tier gets it.
    let selected = variants
        .first()
        .and_then(|variant| selected_stream(quality, variant));
    LiveInfo {
        platform: Platform::Zhihu,
        live_url: Some(room_url(&theater.theater_id)),
        anchor_id: None,
        room_id: theater.theater_id,
        anchor_name: theater.anchor_name,
        title: theater.title,
        is_live: selected.is_some(),
        variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Zhihu streams: the User-Agent only.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("User-Agent".to_string(), USER_AGENT_VALUE.to_string())])
}

/// `BaseLiveStream._get_mobile_headers`: User-Agent, Accept-Language and the
/// user cookie (if any).
pub fn mobile_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
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

/// Zhihu adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct ZhihuAdapter {
    clients: ClientCache,
}

impl ZhihuAdapter {
    pub fn new() -> Self {
        ZhihuAdapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let theater_id = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let page_url = room_url(&theater_id);
        let response = client
            .get(&page_url)
            .headers(mobile_headers(user_cookie(options))?)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), &page_url));
        }
        let theater = parse_page(&response.text().await?, &theater_id)?;
        Ok(build_live_info(theater, options.quality))
    }
}

impl PlatformAdapter for ZhihuAdapter {
    fn platform(&self) -> Platform {
        Platform::Zhihu
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

    const LIVE: &str = include_str!("../../tests/fixtures/zhihu/page_live.html");
    const OFFLINE: &str = include_str!("../../tests/fixtures/zhihu/page_offline.html");
    const MISSING: &str = include_str!("../../tests/fixtures/zhihu/page_missing.html");
    const NODATA: &str = include_str!("../../tests/fixtures/zhihu/page_nodata.html");

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let adapter = ZhihuAdapter::new();
        for input in [
            "https://www.zhihu.com/theater/10687?drama_id=2037464603865642996",
            "zhihu.com/theater/10687",
            "http://WWW.ZHIHU.COM/theater/10687/#x",
        ] {
            assert_eq!(parse_link(input).unwrap(), "10687", "{input}");
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            adapter
                .normalize_url("https://www.zhihu.com/theater/10687?drama_id=1")
                .as_deref(),
            Some("https://www.zhihu.com/theater/10687")
        );
        let found = super::super::adapter_for("https://www.zhihu.com/theater/1").unwrap();
        assert_eq!(found.platform(), Platform::Zhihu);
        assert_eq!(Platform::Zhihu.key(), "zhihu");
        assert_eq!(Platform::Zhihu.display_name(), "知乎直播");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        for input in [
            "https://www.zhihu.com/",
            "https://www.zhihu.com/theater",
            "https://www.zhihu.com/theater/abc",
            "https://www.zhihu.com/theater/1/forecast/2",
            "https://www.zhihu.com/question/123",
            "https://zhuanlan.zhihu.com/theater/1",
            "https://zhihu.com.evil.example/theater/1",
            "ftp://www.zhihu.com/theater/1",
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
    fn live_page_gives_hls_and_flv() {
        let theater = parse_page(LIVE, "55785").unwrap();
        assert!(theater.is_live);
        assert_eq!(theater.anchor_name, "测试主播🎤");
        let info = build_live_info(theater, Quality::Ld);
        assert_eq!(info.title.as_deref(), Some("测试直播间标题"));
        assert!(info.is_live);
        let selected = info.selected.unwrap();
        assert_eq!(
            selected.record_url,
            "https://live.vzuu.com/live/aaaa_origin.m3u8?bu=drama&auth_key=x&f=m3u8"
        );
        assert!(
            selected
                .variant
                .flv_url
                .unwrap()
                .starts_with("https://live.vzuu.com/live/aaaa_origin.flv")
        );
    }

    #[test]
    fn offline_missing_and_broken_pages() {
        let offline = parse_page(OFFLINE, "10687").unwrap();
        assert!(!offline.is_live);
        assert_eq!(offline.anchor_name, "闪电新闻");
        let info = build_live_info(offline, Quality::Od);
        assert!(!info.is_live && info.selected.is_none() && info.variants.is_empty());
        assert_eq!(parse_page(MISSING, "1").unwrap_err().kind(), "invalid_url");
        assert_eq!(
            parse_page(OFFLINE, "999").unwrap_err().kind(),
            "invalid_url"
        );
        assert_eq!(parse_page(NODATA, "1").unwrap_err().kind(), "parse");
        let broken = format!("{DATA_START}{{\"a\":");
        assert_eq!(parse_page(&broken, "1").unwrap_err().kind(), "parse");
        let garbled = format!("{DATA_START}nope</script>");
        assert_eq!(parse_page(&garbled, "1").unwrap_err().kind(), "parse");
    }
}
