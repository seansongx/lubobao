//! Xiaohongshu (小红书) adapter, following StreamCap's `RedNoteHandler` and
//! StreamGet v4.0.10 `RedNoteLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/rednote/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The app headers, the `__INITIAL_STATE__` fields read from the
//! room page and the stream URLs built from `flvUrl` follow that file; see
//! `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * A live room is addressed per broadcast: `www.xiaohongshu.com/livestream/
//!   {room_id}` (the app's share link may add a `dynpath…` segment and a
//!   `host_id` query parameter). `xhslink.com/…` share links redirect there.
//!   Once the broadcast ends the same link stays ended; a new broadcast gets a
//!   new room id (StreamCap's README: 一次性地址，暂不支持循环监控).
//! * The room page carries `window.__INITIAL_STATE__`; its `liveStream` holds
//!   the page status, the live status (`success` while live, `end` after),
//!   the host's nickname and the room info with an app `deeplink` whose
//!   `flvUrl` names the stream.
//! * The stream is the source FLV on `live-source-play.xhscdn.com`, with the
//!   matching `.m3u8` beside it. Xiaohongshu offers one quality (原画).
//! * No login is needed. A user cookie, when set, is sent with page requests
//!   but never to the CDN.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{COOKIE, HeaderMap, HeaderName, HeaderValue, REFERER, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header, user_cookie,
};

/// `RedNoteLiveStream` mobile `user-agent`, also used to pull streams.
pub const USER_AGENT_VALUE: &str = "ios/7.830 (ios 17.0; ; iPhone 15 (A2846/A3089/A3090/A3092))";
/// `RedNoteLiveStream` `xy-common-params`.
const COMMON_PARAMS: &str = "platform=iOS&sid=session.1722166379345546829388";
/// `RedNoteLiveStream` `referer`.
const APP_REFERER: &str = "https://app.xhs.cn/";
const ROOM_PAGE: &str = "https://www.xiaohongshu.com/livestream/";
const PROFILE_PAGE: &str = "https://www.xiaohongshu.com/user/profile/";
const SHARE_HOST: &str = "https://xhslink.com";
/// StreamGet rewrites the deeplink's `flvUrl` host to the source CDN.
const STREAM_BASE: &str = "http://live-source-play.xhscdn.com/live/";
/// Name of the one quality Xiaohongshu offers (`quality_type_name` in the
/// page's `pullConfig`).
const ORIGINAL_NAME: &str = "原画";

/// A recognized Xiaohongshu link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XiaohongshuLink {
    /// `www.xiaohongshu.com/livestream/[dynpath…/]{room_id}[?host_id=…]`.
    Room {
        room_id: String,
        /// Anchor's user id from the `host_id` query parameter, when present.
        host_id: Option<String>,
    },
    /// `xhslink.com/…` share link; only the redirect tells what it points to.
    ShareLink { path: String },
    /// `www.xiaohongshu.com/user/profile/{user_id}`. Recognized so the user
    /// gets an explanation instead of「暂不支持」: the profile page does not
    /// tell whether the user is live (StreamGet only reads the nickname from
    /// it, so such tasks never start recording).
    Profile { user_id: String },
}

impl XiaohongshuLink {
    /// Canonical URL for the link.
    pub fn normalized_url(&self) -> String {
        match self {
            XiaohongshuLink::Room { room_id, .. } => format!("{ROOM_PAGE}{room_id}"),
            XiaohongshuLink::ShareLink { path } => format!("{SHARE_HOST}{path}"),
            XiaohongshuLink::Profile { user_id } => format!("{PROFILE_PAGE}{user_id}"),
        }
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn is_user_id(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn with_scheme(input: &str) -> String {
    if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    }
}

/// Room or profile in a parsed `xiaohongshu.com` (or, after an overseas
/// redirect, `rednote.com`) URL. `None` for other pages.
fn parse_site_path(url: &Url) -> Option<XiaohongshuLink> {
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        ["livestream", .., room_id] if is_digits(room_id) => {
            let host_id = url
                .query_pairs()
                .find(|(key, _)| key == "host_id")
                .map(|(_, value)| value.into_owned())
                .filter(|value| is_user_id(value));
            Some(XiaohongshuLink::Room {
                room_id: (*room_id).to_string(),
                host_id,
            })
        }
        ["user", "profile", user_id] if is_user_id(user_id) => Some(XiaohongshuLink::Profile {
            user_id: (*user_id).to_string(),
        }),
        _ => None,
    }
}

/// Recognize and normalize a Xiaohongshu link without any network access.
///
/// StreamCap registers `www\.xiaohongshu\.com/` and `xhslink\.com/`.
pub fn parse_link(input: &str) -> Result<XiaohongshuLink, PlatformError> {
    let trimmed = input.trim();
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的小红书直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme(trimmed)).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    match host.as_str() {
        // `rednote.com` is where overseas visitors are redirected; StreamCap
        // only meets it as a redirect target.
        "www.xiaohongshu.com" | "xiaohongshu.com" | "www.rednote.com" | "rednote.com" => {
            parse_site_path(&url).ok_or_else(invalid)
        }
        "xhslink.com" | "www.xhslink.com" => {
            let path = url.path().trim_end_matches('/');
            if path.is_empty() {
                return Err(invalid());
            }
            Ok(XiaohongshuLink::ShareLink {
                path: path.to_string(),
            })
        }
        _ => Err(invalid()),
    }
}

/// Room behind a share link, from the URL the redirect ended at.
pub fn parse_share_redirect(final_url: &str) -> Result<XiaohongshuLink, PlatformError> {
    let expired = || {
        PlatformError::InvalidUrl(
            "小红书分享链接已失效或不是直播间（直播分享链接只在那一场直播内有效）".to_string(),
        )
    };
    let url = Url::parse(final_url).map_err(|_| expired())?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let site = host == "xiaohongshu.com"
        || host.ends_with(".xiaohongshu.com")
        || host == "rednote.com"
        || host.ends_with(".rednote.com");
    if !site {
        return Err(expired());
    }
    match parse_site_path(&url) {
        Some(room @ XiaohongshuLink::Room { .. }) => Ok(room),
        Some(XiaohongshuLink::Profile { .. }) => Err(profile_error()),
        _ => Err(expired()),
    }
}

fn profile_error() -> PlatformError {
    PlatformError::InvalidUrl(
        "小红书主页链接无法判断是否开播，请在直播间里复制直播间链接或分享链接（每场直播的链接不同）"
            .to_string(),
    )
}

/// Replace bare `undefined` tokens (outside string literals) with `null`, so
/// the page state parses as JSON. StreamGet replaces every occurrence, which
/// would also change titles that contain the word.
fn undefined_to_null(state: &str) -> String {
    let mut out = String::with_capacity(state.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut rest = state;
    while let Some(c) = rest.chars().next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if rest.starts_with("undefined") {
            out.push_str("null");
            rest = &rest["undefined".len()..];
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// `window.__INITIAL_STATE__` of a page, as JSON.
fn initial_state(html: &str) -> Option<Value> {
    const START: &str = "<script>window.__INITIAL_STATE__=";
    let start = html.find(START)? + START.len();
    let end = html[start..].find("</script>")?;
    let state = html[start..start + end].trim().trim_end_matches(';');
    serde_json::from_str(&undefined_to_null(state)).ok()
}

/// One query parameter of a URL-like string (the deeplink uses a custom
/// scheme, so only its query part is parsed).
fn query_param(link: &str, key: &str) -> Option<String> {
    let (_, query) = link.split_once('?')?;
    let url = Url::parse(&format!("http://deeplink.invalid/?{query}")).ok()?;
    url.query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

/// Room state read from the room page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomPage {
    pub anchor_name: String,
    /// Anchor's user id (`host_id` in the deeplink), when the page has it.
    pub anchor_id: Option<String>,
    pub title: Option<String>,
    /// Stream name from `flvUrl` while live; `None` when not live.
    pub stream_name: Option<String>,
}

/// Parse a `livestream/{room_id}` page.
pub fn parse_room_page(html: &str, room_id: &str) -> Result<RoomPage, PlatformError> {
    let Some(state) = initial_state(html) else {
        if html.trim().is_empty() {
            return Err(PlatformError::RateLimited(
                "小红书返回空页面，可能触发风控".to_string(),
            ));
        }
        if !html.contains("__INITIAL_STATE__") {
            return Err(PlatformError::RateLimited(
                "小红书返回了非直播间页面，可能触发风控".to_string(),
            ));
        }
        return Err(PlatformError::Parse(
            "小红书直播间页面数据无法解析".to_string(),
        ));
    };
    let live = state
        .get("liveStream")
        .filter(|live| live.is_object())
        .ok_or_else(|| PlatformError::Parse("小红书直播间页面缺少 liveStream".to_string()))?;
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let page_status = text(live.get("pageStatus")).unwrap_or_default();
    if page_status == "error" {
        let message = text(live.get("errorMessage")).unwrap_or_else(|| "未找到直播间".to_string());
        return Err(PlatformError::InvalidUrl(format!("小红书提示：{message}")));
    }
    if page_status != "success" {
        return Err(PlatformError::Parse(format!(
            "小红书直播间页面状态异常：{page_status}"
        )));
    }
    let room_data = live.get("roomData").unwrap_or(&Value::Null);
    let room_info = room_data.get("roomInfo").unwrap_or(&Value::Null);
    let deeplink = text(room_info.get("deeplink")).unwrap_or_default();
    let anchor_name = text(
        room_data
            .get("hostInfo")
            .and_then(|host| host.get("nickName")),
    )
    .or_else(|| query_param(&deeplink, "host_nickname"))
    .ok_or_else(|| PlatformError::Parse("小红书直播间页面缺少主播昵称".to_string()))?;
    let title = text(room_info.get("roomTitle"));
    // StreamGet: live when `liveStatus == "success"` and the title is not a
    // replay (回放). It also needs a title; an empty one still counts here.
    let is_live = text(live.get("liveStatus")).as_deref() == Some("success")
        && !title.as_deref().unwrap_or_default().contains("回放");
    let stream_name = is_live.then(|| {
        // StreamGet: `flv_url.split('live/')[1].split('.')[0]`.
        query_param(&deeplink, "flvUrl")
            .and_then(|flv| {
                let name = flv.split_once("live/")?.1.split('.').next()?.to_string();
                (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
                    .then_some(name)
            })
            .unwrap_or_else(|| room_id.to_string())
    });
    Ok(RoomPage {
        anchor_name,
        anchor_id: query_param(&deeplink, "host_id").filter(|id| is_user_id(id)),
        title,
        stream_name,
    })
}

/// The one stream variant for a stream name: source FLV plus HLS.
pub fn stream_variant(stream_name: &str) -> StreamVariant {
    let flv_url = format!("{STREAM_BASE}{stream_name}.flv");
    StreamVariant {
        name: ORIGINAL_NAME.to_string(),
        quality: Some(Quality::Od),
        hls_url: Some(flv_url.replace(".flv", ".m3u8")),
        flv_url: Some(flv_url),
    }
}

/// Turn the room page into `LiveInfo`. Every quality records the one stream.
pub fn build_live_info(
    room_id: &str,
    link_host_id: Option<&str>,
    page: RoomPage,
    quality: Quality,
) -> LiveInfo {
    let (variants, selected) = match page.stream_name.as_deref() {
        Some(stream_name) => {
            let variant = stream_variant(stream_name);
            // StreamGet: `record_url = flv_url`.
            let selected = SelectedStream {
                requested: quality,
                record_url: variant.flv_url.clone().unwrap_or_default(),
                variant: variant.clone(),
            };
            (vec![variant], Some(selected))
        }
        None => (Vec::new(), None),
    };
    LiveInfo {
        platform: Platform::Xiaohongshu,
        room_id: room_id.to_string(),
        anchor_id: page.anchor_id.or_else(|| link_host_id.map(str::to_string)),
        live_url: Some(format!("{ROOM_PAGE}{room_id}")),
        anchor_name: page.anchor_name,
        title: page.title,
        is_live: selected.is_some(),
        variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling streams: the app User-Agent and Referer. User cookies
/// are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Referer".to_string(), APP_REFERER.to_string()),
    ])
}

/// `RedNoteLiveStream.mobile_headers`, plus the user cookie when set.
pub fn page_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(
        HeaderName::from_static("xy-common-params"),
        HeaderValue::from_static(COMMON_PARAMS),
    );
    headers.insert(REFERER, HeaderValue::from_static(APP_REFERER));
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// Xiaohongshu adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct XiaohongshuAdapter {
    clients: ClientCache,
}

impl XiaohongshuAdapter {
    pub fn new() -> Self {
        XiaohongshuAdapter::default()
    }

    /// GET a page, returning the URL the redirects ended at and the body.
    async fn get(
        client: &reqwest::Client,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(String, String), PlatformError> {
        let response = client.get(url).headers(headers).send().await?;
        let final_url = response.url().to_string();
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), url));
        }
        Ok((final_url, response.text().await?))
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        if matches!(link, XiaohongshuLink::Profile { .. }) {
            return Err(profile_error());
        }
        let client = self.clients.get(options.proxy.as_deref())?;
        let headers = page_headers(user_cookie(options))?;

        let link = match link {
            XiaohongshuLink::ShareLink { .. } => {
                // An expired share link redirects to the home page, which
                // answers 404 from some regions; judge by the final URL.
                let response = client
                    .get(link.normalized_url())
                    .headers(headers.clone())
                    .send()
                    .await?;
                parse_share_redirect(response.url().as_str())?
            }
            other => other,
        };
        let XiaohongshuLink::Room { room_id, host_id } = link else {
            return Err(profile_error());
        };
        let (_, html) = Self::get(&client, &format!("{ROOM_PAGE}{room_id}"), headers).await?;
        let page = parse_room_page(&html, &room_id)?;
        Ok(build_live_info(
            &room_id,
            host_id.as_deref(),
            page,
            options.quality,
        ))
    }
}

impl PlatformAdapter for XiaohongshuAdapter {
    fn platform(&self) -> Platform {
        Platform::Xiaohongshu
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

    const ROOM_LIVE: &str = include_str!("../../tests/fixtures/xiaohongshu/room_live.html");
    const ROOM_ENDED: &str = include_str!("../../tests/fixtures/xiaohongshu/room_ended.html");
    const ROOM_MISSING: &str = include_str!("../../tests/fixtures/xiaohongshu/room_missing.html");

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let adapter = XiaohongshuAdapter::new();
        let cases = [
            (
                "https://www.xiaohongshu.com/livestream/570555131224313846?track_id=x&source=web_feed",
                "570555131224313846",
                None,
            ),
            (
                "https://www.xiaohongshu.com/livestream/dynpath5Gm0j8Vm/570555131224313846?timestamp=1&source=share_out_of_app&host_id=5dafa8390000000001007e35",
                "570555131224313846",
                Some("5dafa8390000000001007e35"),
            ),
            ("  xiaohongshu.com/livestream/123/ ", "123", None),
        ];
        for (input, room_id, host_id) in cases {
            assert_eq!(
                parse_link(input).unwrap(),
                XiaohongshuLink::Room {
                    room_id: room_id.to_string(),
                    host_id: host_id.map(str::to_string),
                },
                "{input}"
            );
            assert_eq!(
                adapter.normalize_url(input).unwrap(),
                format!("https://www.xiaohongshu.com/livestream/{room_id}"),
                "{input}"
            );
            assert!(adapter.matches(input), "{input}");
        }
        // StreamCap README example.
        assert_eq!(
            parse_link("http://xhslink.com/xpJpfM").unwrap(),
            XiaohongshuLink::ShareLink {
                path: "/xpJpfM".to_string()
            }
        );
        assert_eq!(
            adapter
                .normalize_url("https://XHSLINK.com/m/AbC12/?x=1")
                .as_deref(),
            Some("https://xhslink.com/m/AbC12")
        );
        assert_eq!(
            adapter
                .normalize_url(
                    "https://www.xiaohongshu.com/user/profile/5ff0e6410000000001008400?xsec_token=1"
                )
                .as_deref(),
            Some("https://www.xiaohongshu.com/user/profile/5ff0e6410000000001008400")
        );
        let found = super::super::adapter_for("https://www.xiaohongshu.com/livestream/1").unwrap();
        assert_eq!(found.platform(), Platform::Xiaohongshu);
        assert_eq!(Platform::Xiaohongshu.key(), "xiaohongshu");
        assert_eq!(Platform::Xiaohongshu.display_name(), "小红书");
    }

    #[test]
    fn rednote_room_links_are_recognized_directly() {
        let adapter = XiaohongshuAdapter::new();
        let input = "https://www.rednote.com/livestream/570000000000000001?host_id=5c1234567890abcdef012345";
        assert_eq!(
            parse_link(input).unwrap(),
            parse_link("https://www.xiaohongshu.com/livestream/570000000000000001?host_id=5c1234567890abcdef012345")
                .unwrap()
        );
        assert!(adapter.matches(input));
        assert_eq!(
            adapter.normalize_url(input),
            adapter.normalize_url("https://www.xiaohongshu.com/livestream/570000000000000001")
        );
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        let adapter = XiaohongshuAdapter::new();
        for input in [
            "https://www.xiaohongshu.com/",
            "https://www.xiaohongshu.com/livelist",
            "https://www.xiaohongshu.com/livestream/abc",
            "https://www.xiaohongshu.com/explore/6789abcdef0123456789abcd",
            "https://xhslink.com/",
            "https://www.xiaohongshu.com.evil.example/livestream/1",
            "https://www.rednote.com/explore/1",
            "https://www.rednote.com.evil.example/livestream/1",
            "ftp://www.xiaohongshu.com/livestream/1",
            "https://www.douyu.com/3637778",
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

    #[tokio::test]
    async fn profile_links_are_recognized_but_explained() {
        let adapter = XiaohongshuAdapter::new();
        let url = "https://www.xiaohongshu.com/user/profile/5ff0e6410000000001008400";
        assert!(adapter.matches(url));
        let error = adapter
            .fetch(url, &FetchOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), "invalid_url");
        assert!(error.message().contains("直播间链接"), "{error}");
    }

    #[test]
    fn share_redirects_lead_to_rooms_or_explain_expiry() {
        assert_eq!(
            parse_share_redirect(
                "https://www.xiaohongshu.com/livestream/dynpathX/570555131224313846?host_id=5dafa8390000000001007e35&xhsshare=WeixinSession"
            )
            .unwrap(),
            XiaohongshuLink::Room {
                room_id: "570555131224313846".to_string(),
                host_id: Some("5dafa8390000000001007e35".to_string()),
            }
        );
        assert!(matches!(
            parse_share_redirect("https://www.rednote.com/livestream/42").unwrap(),
            XiaohongshuLink::Room { .. }
        ));
        for final_url in [
            "http://www.xiaohongshu.com/",
            "https://www.xiaohongshu.com/explore/abc",
            "https://xhslink.com/xpJpfM",
            "https://example.com/livestream/1",
        ] {
            let error = parse_share_redirect(final_url).unwrap_err();
            assert_eq!(error.kind(), "invalid_url", "{final_url}");
            assert!(error.message().contains("已失效"), "{final_url}");
        }
        let profile =
            parse_share_redirect("https://www.xiaohongshu.com/user/profile/5ff0e641").unwrap_err();
        assert!(profile.message().contains("主页"), "{profile}");
    }

    #[test]
    fn undefined_is_replaced_only_outside_strings() {
        assert_eq!(
            undefined_to_null(r#"{"a":undefined,"b":"undefined \"undefined\"","c":[undefined]}"#),
            r#"{"a":null,"b":"undefined \"undefined\"","c":[null]}"#
        );
    }

    #[test]
    fn live_room_records_the_source_flv() {
        let page = parse_room_page(ROOM_LIVE, "100001").unwrap();
        assert_eq!(page.anchor_name, "测试主播");
        assert_eq!(page.anchor_id.as_deref(), Some("5f0000000000000000000001"));
        assert_eq!(page.title.as_deref(), Some("测试直播 undefined"));
        assert_eq!(page.stream_name.as_deref(), Some("100001"));

        let info = build_live_info("100001", None, page, Quality::Hd);
        assert!(info.is_live);
        assert_eq!(info.platform, Platform::Xiaohongshu);
        assert_eq!(info.room_id, "100001");
        assert_eq!(
            info.live_url.as_deref(),
            Some("https://www.xiaohongshu.com/livestream/100001")
        );
        assert_eq!(info.variants.len(), 1);
        assert_eq!(info.variants[0].name, "原画");
        assert_eq!(
            info.variants[0].hls_url.as_deref(),
            Some("http://live-source-play.xhscdn.com/live/100001.m3u8")
        );
        let selected = info.selected.as_ref().unwrap();
        assert_eq!(selected.requested, Quality::Hd);
        assert_eq!(
            selected.record_url,
            "http://live-source-play.xhscdn.com/live/100001.flv"
        );
        assert_eq!(
            selected.source_url(false, false),
            (
                super::super::SourceFormat::Flv,
                "http://live-source-play.xhscdn.com/live/100001.flv"
            )
        );
        assert!(
            info.stream_headers
                .keys()
                .all(|key| !key.eq_ignore_ascii_case("cookie"))
        );
    }

    #[test]
    fn replays_and_ended_rooms_are_offline() {
        let replay =
            parse_room_page(&ROOM_LIVE.replace("测试直播", "回放 测试"), "100001").unwrap();
        assert_eq!(replay.stream_name, None);

        // An ended room keeps its host and title; `nextRoomInfo` (another
        // anchor's room) must not be read.
        let ended = parse_room_page(ROOM_ENDED, "100002").unwrap();
        assert_eq!(ended.anchor_name, "测试主播");
        assert_eq!(ended.anchor_id, None);
        assert_eq!(ended.stream_name, None);
        let info = build_live_info(
            "100002",
            Some("5f0000000000000000000001"),
            ended,
            Quality::Od,
        );
        assert!(!info.is_live);
        assert!(info.variants.is_empty());
        assert!(info.selected.is_none());
        assert_eq!(info.anchor_id.as_deref(), Some("5f0000000000000000000001"));
        assert_eq!(info.title.as_deref(), Some("已结束的直播"));
    }

    #[test]
    fn page_errors_map_to_error_kinds() {
        let missing = parse_room_page(ROOM_MISSING, "1").unwrap_err();
        assert_eq!(missing.kind(), "invalid_url");
        assert_eq!(missing.message(), "小红书提示：未找到直播间，请稍后再试");
        for (body, kind) in [
            ("", "rate_limited"),
            ("<html><body>请完成验证</body></html>", "rate_limited"),
            ("<script>window.__INITIAL_STATE__={</script>", "parse"),
            (
                "<script>window.__INITIAL_STATE__={\"global\":{}}</script>",
                "parse",
            ),
            (
                "<script>window.__INITIAL_STATE__={\"liveStream\":{\"pageStatus\":\"pending\"}}</script>",
                "parse",
            ),
            (
                "<script>window.__INITIAL_STATE__={\"liveStream\":{\"pageStatus\":\"success\",\"liveStatus\":\"success\",\"roomData\":{}}}</script>",
                "parse",
            ),
        ] {
            assert_eq!(
                parse_room_page(body, "1").unwrap_err().kind(),
                kind,
                "{body}"
            );
        }
    }

    #[test]
    fn stream_name_falls_back_to_the_room_id() {
        let body = ROOM_LIVE.replace("flvUrl=", "noFlv=");
        let page = parse_room_page(&body, "100001").unwrap();
        assert_eq!(page.stream_name.as_deref(), Some("100001"));
    }

    #[test]
    fn page_headers_carry_the_user_cookie_only_when_set() {
        let headers = page_headers(None).unwrap();
        assert!(headers.get(COOKIE).is_none());
        assert_eq!(headers.get("xy-common-params").unwrap(), COMMON_PARAMS);
        assert_eq!(
            page_headers(Some("a1=1")).unwrap().get(COOKIE).unwrap(),
            "a1=1"
        );
        assert_eq!(
            page_headers(Some("a=1\r\nb=2")).unwrap_err().kind(),
            "config"
        );
    }
}
