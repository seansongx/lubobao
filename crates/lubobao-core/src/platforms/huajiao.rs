//! Huajiao adapter, following StreamCap's `HuajiaoHandler` and StreamGet
//! v4.0.10 `HuajiaoLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/huajiao/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The request headers, the `getUserFeeds` live check, the
//! `getFeedInfo` lookup and the FLV URL (`pull_url` plus `&codec={encode}`)
//! follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * Every check goes through the anchor's uid. StreamCap calls
//!   `fetch_app_stream_data`, which reads
//!   `live.huajiao.com/feed/getUserFeeds?userid={uid}`: the anchor is live
//!   when a feed's `rtop` is `直播中`, and that feed's `pull_url` is the FLV.
//! * A `www.huajiao.com/l/{liveid}` link (the StreamCap README example)
//!   names one broadcast. Its web page now redirects to the homepage even
//!   while live, so the uid comes from the app API StreamGet's web flow uses,
//!   `feed/getFeedInfo?relateid={liveid}`. Once the broadcast is gone that
//!   API answers `errno` 1506 and the link can no longer be checked; the
//!   error asks for the anchor's page instead.
//! * When the anchor has no feeds at all, the nickname comes from the user
//!   page `www.huajiao.com/user/{uid}`, which redirects to the homepage for
//!   unknown users.
//!
//! Huajiao offers a single stream, so every tier records the same FLV, as
//! StreamGet does.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::USER_AGENT;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderName, HeaderValue, REFERER};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header, user_cookie,
};

/// `HuajiaoLiveStream.pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:109.0) Gecko/20100101 Firefox/115.0";
const PC_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6";
/// `BaseLiveStream.mobile_headers`, used by StreamGet for `getUserFeeds`.
const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 11; SAMSUNG SM-G973U) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/14.2 Chrome/87.0.4280.141 Mobile Safari/537.36";
const MOBILE_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.8,zh-TW;q=0.7,zh-HK;q=0.5,en-US;q=0.3,en;q=0.2";
/// App headers StreamGet sends to `getFeedInfo`.
const APP_USER_AGENT: &str =
    "living/9.4.0 (com.huajiao.seeding; build:2410231746; iOS 17.0.0) Alamofire/9.4.0";
const APP_ACCEPT_LANGUAGE: &str = "zh-Hans-US;q=1.0";
const WEB_REFERER: &str = "https://www.huajiao.com/";
const USER_PAGE: &str = "https://www.huajiao.com/user/";
const LIVE_PAGE: &str = "https://www.huajiao.com/l/";
const USER_FEEDS_API: &str = "https://live.huajiao.com/feed/getUserFeeds";
const FEED_INFO_API: &str = "https://live.huajiao.com/feed/getFeedInfo";
/// `rtop` of a feed that is live right now.
const LIVE_MARK: &str = "直播中";
/// `errno` of `getFeedInfo` for a feed that does not exist (any more).
const ERRNO_NOT_FOUND: i64 = 1506;
/// Name of the only variant.
const VARIANT_NAME: &str = "原画";

/// A recognized Huajiao link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HuajiaoLink {
    /// The anchor's page `www.huajiao.com/user/{uid}`, or a share link with
    /// `author={uid}`. Works for every broadcast.
    User(String),
    /// One broadcast, `www.huajiao.com/l/{liveid}` or a share link with
    /// `liveid={liveid}`. Only works while that broadcast exists.
    Live(String),
}

impl HuajiaoLink {
    pub fn normalized_url(&self) -> String {
        match self {
            HuajiaoLink::User(uid) => format!("{USER_PAGE}{uid}"),
            HuajiaoLink::Live(liveid) => format!("{LIVE_PAGE}{liveid}"),
        }
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit())
}

/// Recognize and normalize a Huajiao link without any network access.
///
/// StreamCap registers `.*\.huajiao\.com/`; its README example is
/// `www.huajiao.com/l/345096174`. Accepted on `huajiao.com` and its
/// subdomains: `/user/{uid}`, `/l/{liveid}`, and share links carrying
/// `author={uid}` (StreamGet's app flow) or `liveid={liveid}` (its web flow)
/// in the query. `author` wins when both are present, because it keeps
/// working after the broadcast ends.
pub fn parse_link(input: &str) -> Result<HuajiaoLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的花椒直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "huajiao.com" && !host.ends_with(".huajiao.com") {
        return Err(invalid());
    }
    let query = |key: &str| {
        url.query_pairs()
            .find(|(name, value)| name == key && is_digits(value))
            .map(|(_, value)| value.into_owned())
    };
    if let Some(uid) = query("author") {
        return Ok(HuajiaoLink::User(uid));
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        ["user", uid] if is_digits(uid) => return Ok(HuajiaoLink::User(uid.to_string())),
        ["l", liveid] if is_digits(liveid) => return Ok(HuajiaoLink::Live(liveid.to_string())),
        _ => {}
    }
    if let Some(liveid) = query("liveid") {
        return Ok(HuajiaoLink::Live(liveid));
    }
    Err(invalid())
}

fn parse_json(body: &str, what: &str) -> Result<Value, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited(format!("花椒{what}返回的不是 JSON，可能触发风控"))
        } else {
            PlatformError::Parse(format!("花椒{what}返回的数据无法解析"))
        }
    })?;
    if !json.is_object() {
        return Err(PlatformError::Parse(format!(
            "花椒{what}返回的数据无法解析"
        )));
    }
    Ok(json)
}

fn errno(json: &Value) -> i64 {
    match json.get("errno") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(-1),
        Some(Value::String(text)) => text.parse().unwrap_or(-1),
        _ => 0,
    }
}

fn api_error(json: &Value, what: &str) -> PlatformError {
    let message = json
        .get("errmsg")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or("未知错误");
    PlatformError::Network(format!("花椒{what}出错（{}）：{message}", errno(json)))
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Uid of the anchor of a `getFeedInfo` answer.
pub fn parse_feed_info(body: &str, liveid: &str) -> Result<String, PlatformError> {
    let json = parse_json(body, "直播信息接口")?;
    let not_found = || {
        PlatformError::InvalidUrl(format!(
            "花椒没有找到直播 {liveid}，可能已经结束。花椒的 /l/ 链接只对应一场直播，请改用主播主页链接 www.huajiao.com/user/{{主播ID}}"
        ))
    };
    match errno(&json) {
        0 => {}
        ERRNO_NOT_FOUND => return Err(not_found()),
        _ => return Err(api_error(&json, "直播信息接口")),
    }
    // A broadcast that is gone sometimes comes back as a cached, empty
    // `feed`/`author` with `errno` 0 instead of 1506. StreamGet treats a
    // feed without `creatime` as missing; here the anchor's uid decides.
    text(json.pointer("/data/author/uid"))
        .filter(|uid| is_digits(uid))
        .ok_or_else(not_found)
}

/// What `getUserFeeds` says about an anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserFeeds {
    /// Nickname from the first feed; `None` when the anchor has no feeds.
    pub anchor_name: Option<String>,
    /// The live broadcast, when there is one.
    pub live: Option<LiveFeed>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveFeed {
    pub liveid: Option<String>,
    pub title: Option<String>,
    pub variant: StreamVariant,
}

/// Parse a `getUserFeeds` answer.
pub fn parse_user_feeds(body: &str) -> Result<UserFeeds, PlatformError> {
    let json = parse_json(body, "主播动态接口")?;
    if errno(&json) != 0 {
        return Err(api_error(&json, "主播动态接口"));
    }
    let feeds = json
        .pointer("/data/feeds")
        .and_then(Value::as_array)
        .ok_or_else(|| PlatformError::Parse("花椒主播动态接口缺少 feeds".to_string()))?;
    let anchor_name = feeds
        .iter()
        .find_map(|feed| text(feed.pointer("/author/nickname")));
    // StreamGet only looks at the first feed; a live broadcast is listed
    // first, so taking the first live one is the same in practice and does
    // not miss it if another feed is pinned above it.
    let live = feeds.iter().find_map(|feed| {
        let inner = feed.get("feed")?;
        if text(inner.get("rtop")).as_deref() != Some(LIVE_MARK) {
            return None;
        }
        let pull_url = text(inner.get("pull_url"))?;
        // StreamGet: `flv_url = pull_url + '&codec=' + encode`.
        let flv_url = match text(inner.get("encode")) {
            Some(encode) => format!("{pull_url}&codec={encode}"),
            None => pull_url,
        };
        Some(LiveFeed {
            liveid: text(inner.get("relateid")),
            title: text(inner.get("title")),
            variant: StreamVariant {
                name: VARIANT_NAME.to_string(),
                quality: Some(Quality::Od),
                flv_url: Some(flv_url),
                hls_url: None,
            },
        })
    });
    Ok(UserFeeds { anchor_name, live })
}

fn unescape_html(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#039;", "'")
        .replace("&amp;", "&")
}

/// Nickname from the user page title (`{昵称}的主页_直播回放_…`). Unknown
/// users are redirected to the homepage, which has no such title.
pub fn parse_user_page(html: &str) -> Result<String, PlatformError> {
    let title = html
        .find("<title>")
        .map(|start| &html[start + "<title>".len()..])
        .and_then(|rest| rest.find("</title>").map(|end| &rest[..end]));
    let Some(title) = title else {
        if html.trim().is_empty() {
            return Err(PlatformError::RateLimited(
                "花椒返回空页面，可能触发风控".to_string(),
            ));
        }
        return Err(PlatformError::Parse(
            "花椒主播主页缺少标题，页面结构可能已变化".to_string(),
        ));
    };
    match title.split_once("的主页_") {
        Some((name, _)) if !name.trim().is_empty() => Ok(unescape_html(name.trim())),
        _ => Err(PlatformError::InvalidUrl(
            "花椒没有找到这个主播，请检查链接".to_string(),
        )),
    }
}

/// Turn the anchor facts into `LiveInfo`.
pub fn build_live_info(
    uid: &str,
    anchor_name: String,
    live: Option<LiveFeed>,
    quality: Quality,
) -> LiveInfo {
    let selected = live.as_ref().and_then(|live| {
        // StreamGet: `record_url = flv_url`, whatever the requested quality.
        Some(SelectedStream {
            requested: quality,
            variant: live.variant.clone(),
            record_url: live.variant.flv_url.clone()?,
        })
    });
    LiveInfo {
        platform: Platform::Huajiao,
        room_id: uid.to_string(),
        anchor_id: Some(uid.to_string()),
        live_url: Some(format!("{USER_PAGE}{uid}")),
        anchor_name,
        title: live.as_ref().and_then(|live| live.title.clone()),
        is_live: selected.is_some(),
        variants: live.map(|live| live.variant).into_iter().collect(),
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Huajiao streams: User-Agent and the web Referer. User
/// cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Referer".to_string(), WEB_REFERER.to_string()),
    ])
}

fn with_cookie(mut headers: HeaderMap, cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// `HuajiaoLiveStream.pc_headers`, for the user page.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(PC_ACCEPT_LANGUAGE),
    );
    headers.insert(REFERER, HeaderValue::from_static(WEB_REFERER));
    with_cookie(headers, cookie)
}

/// `BaseLiveStream.mobile_headers`, for `getUserFeeds`.
pub fn mobile_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(MOBILE_USER_AGENT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(MOBILE_ACCEPT_LANGUAGE),
    );
    with_cookie(headers, cookie)
}

/// The app headers StreamGet sends to `getFeedInfo`.
pub fn app_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(APP_USER_AGENT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(APP_ACCEPT_LANGUAGE),
    );
    headers.insert(
        HeaderName::from_static("sdk_version"),
        HeaderValue::from_static("1"),
    );
    with_cookie(headers, cookie)
}

/// Huajiao adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct HuajiaoAdapter {
    clients: ClientCache,
}

impl HuajiaoAdapter {
    pub fn new() -> Self {
        HuajiaoAdapter::default()
    }

    async fn get(
        client: &reqwest::Client,
        url: &str,
        headers: HeaderMap,
    ) -> Result<String, PlatformError> {
        let response = client.get(url).headers(headers).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), url));
        }
        Ok(response.text().await?)
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let uid = match link {
            HuajiaoLink::User(uid) => uid,
            HuajiaoLink::Live(liveid) => {
                let mut api = Url::parse(FEED_INFO_API).expect("valid feed info API URL");
                api.query_pairs_mut().append_pair("relateid", &liveid);
                let body = Self::get(&client, api.as_str(), app_headers(cookie)?).await?;
                parse_feed_info(&body, &liveid)?
            }
        };

        let mut api = Url::parse(USER_FEEDS_API).expect("valid user feeds API URL");
        api.query_pairs_mut()
            .append_pair("channel", "Apple")
            .append_pair("userid", &uid)
            .append_pair("uid", &uid);
        let body = Self::get(&client, api.as_str(), mobile_headers(cookie)?).await?;
        let feeds = parse_user_feeds(&body)?;

        let anchor_name = match feeds.anchor_name {
            Some(name) => name,
            None => {
                let page = format!("{USER_PAGE}{uid}");
                parse_user_page(&Self::get(&client, &page, pc_headers(cookie)?).await?)?
            }
        };
        Ok(build_live_info(
            &uid,
            anchor_name,
            feeds.live,
            options.quality,
        ))
    }
}

impl PlatformAdapter for HuajiaoAdapter {
    fn platform(&self) -> Platform {
        Platform::Huajiao
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

    const USER_FEEDS_LIVE: &str = include_str!("../../tests/fixtures/huajiao/user_feeds_live.json");
    const USER_FEEDS_OFFLINE: &str =
        include_str!("../../tests/fixtures/huajiao/user_feeds_offline.json");
    const USER_FEEDS_EMPTY: &str =
        include_str!("../../tests/fixtures/huajiao/user_feeds_empty.json");
    const FEED_INFO_LIVE: &str = include_str!("../../tests/fixtures/huajiao/feed_info_live.json");
    const FEED_INFO_MISSING: &str =
        include_str!("../../tests/fixtures/huajiao/feed_info_missing.json");
    const FEED_INFO_CACHED_MISSING: &str =
        include_str!("../../tests/fixtures/huajiao/feed_info_cached_missing.json");
    const USER_PAGE_HTML: &str = include_str!("../../tests/fixtures/huajiao/user_page.html");
    const HOME_PAGE_HTML: &str = include_str!("../../tests/fixtures/huajiao/home_page.html");

    #[test]
    fn recognizes_and_normalizes_links() {
        let adapter = HuajiaoAdapter::new();
        let cases = [
            (
                "https://www.huajiao.com/l/345096174",
                HuajiaoLink::Live("345096174".into()),
                "https://www.huajiao.com/l/345096174",
            ),
            (
                "  www.huajiao.com/user/243575475/?from=pc ",
                HuajiaoLink::User("243575475".into()),
                "https://www.huajiao.com/user/243575475",
            ),
            (
                "http://HUAJIAO.COM/l/350718917#x",
                HuajiaoLink::Live("350718917".into()),
                "https://www.huajiao.com/l/350718917",
            ),
            (
                "https://h.huajiao.com/l/index?liveid=350718917&author=243575475&reference=share",
                HuajiaoLink::User("243575475".into()),
                "https://www.huajiao.com/user/243575475",
            ),
            (
                "https://h.huajiao.com/l/index?liveid=350718917&qd=wx",
                HuajiaoLink::Live("350718917".into()),
                "https://www.huajiao.com/l/350718917",
            ),
        ];
        for (input, link, normalized) in cases {
            assert_eq!(parse_link(input).unwrap(), link, "{input}");
            assert!(adapter.matches(input), "{input}");
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some(normalized),
                "{input}"
            );
        }
        let huajiao = super::super::adapter_for("https://www.huajiao.com/l/345096174").unwrap();
        assert_eq!(huajiao.platform(), Platform::Huajiao);
        assert_eq!(Platform::Huajiao.key(), "huajiao");
        assert_eq!(Platform::Huajiao.display_name(), "花椒直播");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        let adapter = HuajiaoAdapter::new();
        for input in [
            "https://www.huajiao.com/",
            "https://www.huajiao.com/category/1000",
            "https://www.huajiao.com/l/abc",
            "https://www.huajiao.com/user/1/2",
            "https://h.huajiao.com/l/index?liveid=abc",
            "https://www.huajiao.com.evil.example/l/345096174",
            "https://nothuajiao.com/l/345096174",
            "ftp://www.huajiao.com/l/345096174",
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
    fn feed_info_gives_the_anchor_or_explains_a_finished_broadcast() {
        assert_eq!(
            parse_feed_info(FEED_INFO_LIVE, "300000001").unwrap(),
            "100001"
        );
        for body in [FEED_INFO_MISSING, FEED_INFO_CACHED_MISSING] {
            let missing = parse_feed_info(body, "345096174").unwrap_err();
            assert_eq!(missing.kind(), "invalid_url", "{missing}");
            assert!(missing.message().contains("345096174"), "{missing}");
            assert!(missing.message().contains("/user/"), "{missing}");
        }
        let other = parse_feed_info(r#"{"errno":1001,"errmsg":"系统繁忙"}"#, "1").unwrap_err();
        assert_eq!(other.kind(), "network");
        assert!(other.message().contains("系统繁忙"), "{other}");
        assert_eq!(parse_feed_info("[]", "1").unwrap_err().kind(), "parse");
    }

    #[test]
    fn live_anchor_records_the_pull_url_with_its_codec() {
        let feeds = parse_user_feeds(USER_FEEDS_LIVE).unwrap();
        assert_eq!(feeds.anchor_name.as_deref(), Some("测试主播ღ⸝⸝"));
        let live = feeds.live.clone().unwrap();
        assert_eq!(live.liveid.as_deref(), Some("300000001"));
        for quality in Quality::ALL {
            let info = build_live_info(
                "100001",
                feeds.anchor_name.clone().unwrap(),
                feeds.live.clone(),
                quality,
            );
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Huajiao);
            assert_eq!(info.room_id, "100001");
            assert_eq!(info.anchor_id.as_deref(), Some("100001"));
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://www.huajiao.com/user/100001")
            );
            assert_eq!(info.title.as_deref(), Some("测试🎤直播间"));
            assert_eq!(info.variants.len(), 1);
            assert_eq!(info.variants[0].name, "原画");
            assert_eq!(info.variants[0].quality, Some(Quality::Od));
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert_eq!(
                selected.record_url,
                "https://live-pull-3.huajiao.com/main/HJ_0_tc_3_main__h265_100001_1795483092705_5502_O.flv?txSecret=0123456789abcdef0123456789abcdef&txTime=6B063CCC&codec=h265"
            );
            // Only FLV exists, so even an `h265` mark or a HLS preference
            // records it.
            for (flv_preferred, prefer_hls) in [(false, false), (true, false), (true, true)] {
                assert_eq!(
                    selected.source_url(flv_preferred, prefer_hls),
                    (
                        super::super::SourceFormat::Flv,
                        selected.record_url.as_str()
                    )
                );
            }
            assert!(
                info.stream_headers
                    .keys()
                    .all(|key| !key.eq_ignore_ascii_case("cookie"))
            );
        }
    }

    #[test]
    fn a_live_feed_below_other_feeds_is_still_found() {
        let json: Value = serde_json::from_str(USER_FEEDS_LIVE).unwrap();
        let mut feeds = json["data"]["feeds"].as_array().unwrap().clone();
        feeds.reverse();
        let mut json = json;
        json["data"]["feeds"] = Value::Array(feeds);
        let parsed = parse_user_feeds(&json.to_string()).unwrap();
        assert!(parsed.live.is_some());
        // Without `encode` the pull URL is used as is.
        let body = USER_FEEDS_LIVE.replace(r#""encode":"h265","#, "");
        let parsed = parse_user_feeds(&body).unwrap();
        assert!(
            parsed
                .live
                .unwrap()
                .variant
                .flv_url
                .unwrap()
                .ends_with("txTime=6B063CCC")
        );
    }

    #[test]
    fn offline_and_unknown_anchors() {
        let offline = parse_user_feeds(USER_FEEDS_OFFLINE).unwrap();
        assert_eq!(offline.anchor_name.as_deref(), Some("测试主播ღ⸝⸝"));
        assert!(offline.live.is_none());
        let info = build_live_info("100001", "测试主播ღ⸝⸝".into(), None, Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        assert_eq!(info.title, None);

        let empty = parse_user_feeds(USER_FEEDS_EMPTY).unwrap();
        assert_eq!(
            empty,
            UserFeeds {
                anchor_name: None,
                live: None
            }
        );
        assert_eq!(parse_user_page(USER_PAGE_HTML).unwrap(), "测试主播ღ⸝⸝");
        let escaped = USER_PAGE_HTML.replace("测试主播ღ⸝⸝的主页", "A&amp;B的主页");
        assert_eq!(parse_user_page(&escaped).unwrap(), "A&B");
        let unknown = parse_user_page(HOME_PAGE_HTML).unwrap_err();
        assert_eq!(unknown.kind(), "invalid_url", "{unknown}");
        assert_eq!(parse_user_page(" ").unwrap_err().kind(), "rate_limited");
        assert_eq!(
            parse_user_page("<html></html>").unwrap_err().kind(),
            "parse"
        );
    }

    #[test]
    fn bad_feed_answers_map_to_error_kinds() {
        for (body, kind) in [
            ("", "rate_limited"),
            ("<html>", "rate_limited"),
            ("{\"errno\":", "parse"),
            ("[]", "parse"),
            (r#"{"errno":0,"data":{}}"#, "parse"),
            (r#"{"errno":"1001","errmsg":"busy"}"#, "network"),
        ] {
            assert_eq!(parse_user_feeds(body).unwrap_err().kind(), kind, "{body}");
        }
        // A live mark without a pull URL is not a stream.
        let body = USER_FEEDS_LIVE.replace("\"pull_url\"", "\"pull_x\"");
        assert!(parse_user_feeds(&body).unwrap().live.is_none());
    }

    #[test]
    fn request_headers_carry_the_user_cookie_only_when_set() {
        for headers in [pc_headers, mobile_headers, app_headers] {
            assert!(headers(None).unwrap().get(COOKIE).is_none());
            assert_eq!(headers(Some("a=1")).unwrap().get(COOKIE).unwrap(), "a=1");
            assert_eq!(headers(Some("a=1\nb=2")).unwrap_err().kind(), "config");
        }
        assert_eq!(app_headers(None).unwrap().get("sdk_version").unwrap(), "1");
    }
}
