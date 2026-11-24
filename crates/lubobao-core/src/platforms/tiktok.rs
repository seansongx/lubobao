//! TikTok adapter, following StreamCap's `TikTokHandler` and StreamGet
//! v4.0.10 `TikTokLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/tiktok/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The request headers and default cookies, reading the room from
//! the live page's `SIGI_STATE` script, the `api-live/user/room` request, the
//! `status == 2` live check and building the quality list from
//! `streamData.pull_data.stream_data` (sorted by video bitrate, then
//! resolution, with the codec appended to each URL) follow that file; see
//! `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * `www.tiktok.com/@{uniqueId}/live` carries the anchor and the current room
//!   in `<script id="SIGI_STATE">` (`LiveRoom.liveRoomUserInfo`). The page of
//!   a user that does not exist has no `LiveRoom`.
//! * When the page has no room data, the app API
//!   `www.tiktok.com/api-live/user/room?uniqueId=…` (StreamGet's
//!   `fetch_app_stream_data`) answers with the same `{user, liveRoom}` shape,
//!   or `statusCode` 19881007 for an unknown user.
//! * `user.status == 2` means live. A live room without `streamData` is age
//!   restricted and needs a logged-in cookie.
//!
//! TikTok is not reachable from mainland China; the proxy in `FetchOptions`
//! is used for every request, including short links and the stream probe.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::Url;
use reqwest::header::{COOKIE, HeaderMap, HeaderValue, REFERER, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, fallback_variant_index,
    select_variant_index, selected_stream, user_cookie,
};

/// `TikTokLiveStream._get_pc_headers` User-Agent, also used to pull streams.
pub const PC_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0";
/// `TikTokLiveStream._get_mobile_headers` User-Agent, used for the app API.
const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 13; SM-G981B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Mobile Safari/537.36 Edg/145.0.0.0";
/// StreamGet's default cookies when the user has not set one.
const PC_DEFAULT_COOKIE: &str = "ttwid=1%7Ctzsy_yRGZ2N8AI7luDz2s9H9a8CQI3ZisibOcuw5OHs%7C1761301927%7C484a25162facd523ee6e2997187c9fad4a512c031d9efc6eaddb2c4bae8ce3fb";
const MOBILE_DEFAULT_COOKIE: &str = "ttwid=1%7CyxuNXNlyQp_OPMecm9xbQwWNc6jLL9UppAoBoT4dxdo%7C1773305415%7C4f5ee0da3c5cd9c068fb21adb88106bb3d5df22463f58c60a023d066a8a3fdb0";
const REFERER_VALUE: &str = "https://www.tiktok.com/";
const ROOM_API: &str = "https://www.tiktok.com/api-live/user/room";
/// `api-live/user/room` status code for an unknown `uniqueId`.
const API_USER_NOT_FOUND: i64 = 19_881_007;
/// Page text TikTok shows where it no longer operates (StreamGet checks it to
/// report a blocked proxy region).
const REGION_BLOCKED: &str = "We regret to inform you that we have discontinued operating TikTok";
/// Hosts that only carry share short links.
const SHORT_LINK_HOSTS: [&str; 2] = ["vt.tiktok.com", "vm.tiktok.com"];

/// A recognized TikTok link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TiktokLink {
    /// `tiktok.com/@{uniqueId}` or `…/@{uniqueId}/live`; the id is lowercase.
    User { unique_id: String },
    /// `vt.tiktok.com/…`, `vm.tiktok.com/…` or `www.tiktok.com/t/…`, resolved
    /// by following the redirect.
    Share { url: String },
}

impl TiktokLink {
    /// Canonical URL used to detect duplicate tasks. Share links have none
    /// until they are resolved.
    pub fn normalized_url(&self) -> Option<String> {
        match self {
            TiktokLink::User { unique_id } => Some(room_url(unique_id)),
            TiktokLink::Share { .. } => None,
        }
    }
}

/// Live page of a user.
pub fn room_url(unique_id: &str) -> String {
    format!("https://www.tiktok.com/@{unique_id}/live")
}

/// TikTok usernames: letters, digits, `_` and `.`, at most 24 characters.
fn is_unique_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 24
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

/// Recognize a TikTok link without any network access.
///
/// StreamCap registers `https://.*\.tiktok\.com/` and StreamGet reads the
/// user from `tiktok.com/@{uniqueId}/live`. Profile links without `/live` and
/// share short links are also accepted.
pub fn parse_link(input: &str) -> Result<TiktokLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的 TikTok 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "tiktok.com" && !host.ends_with(".tiktok.com") {
        return Err(invalid());
    }
    let mut segments = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter();
    let first = segments.next();
    if SHORT_LINK_HOSTS.contains(&host.as_str()) {
        return match first {
            Some(_) => Ok(TiktokLink::Share {
                url: url.to_string(),
            }),
            None => Err(invalid()),
        };
    }
    match first {
        Some("t") if segments.next().is_some() => Ok(TiktokLink::Share {
            url: url.to_string(),
        }),
        Some(segment) if segment.starts_with('@') => {
            let unique_id = &segment[1..];
            if !is_unique_id(unique_id) {
                return Err(invalid());
            }
            Ok(TiktokLink::User {
                unique_id: unique_id.to_ascii_lowercase(),
            })
        }
        _ => Err(PlatformError::InvalidUrl(format!(
            "TikTok 链接里没有主播用户名，请复制形如 tiktok.com/@用户名/live 的链接: {trimmed}"
        ))),
    }
}

/// JSON text of `<script id="{id}" …>…</script>`.
pub fn script_json<'a>(html: &'a str, id: &str) -> Option<&'a str> {
    let marker = format!("id=\"{id}\"");
    let start = html.find(&marker)?;
    let open_end = start + html[start..].find('>')? + 1;
    let close = open_end + html[open_end..].find("</script>")?;
    Some(html[open_end..close].trim()).filter(|text| !text.is_empty())
}

/// What the live page says about the user.
#[derive(Debug, Clone, PartialEq)]
pub enum PageRoom {
    /// `LiveRoom.liveRoomUserInfo`: `{user, liveRoom}`.
    Found(Value),
    /// The page has no room data (unknown user, or TikTok left it out).
    Missing,
}

/// Parse the live page.
pub fn parse_page(html: &str) -> Result<PageRoom, PlatformError> {
    if html.contains(REGION_BLOCKED) {
        return Err(PlatformError::Config(
            "TikTok 在代理节点所在的地区停止服务（例如香港），请在代理软件里换其他地区的节点"
                .to_string(),
        ));
    }
    let Some(json) = script_json(html, "SIGI_STATE") else {
        return Err(PlatformError::RateLimited(
            "TikTok 返回的页面没有直播间数据，可能触发了风控或验证码，请检查代理能否正常打开 TikTok".to_string(),
        ));
    };
    let state: Value = serde_json::from_str(json)
        .map_err(|_| PlatformError::Parse("TikTok 页面数据无法解析".to_string()))?;
    let info = state
        .get("LiveRoom")
        .and_then(|room| room.get("liveRoomUserInfo"))
        .filter(|info| info.get("user").is_some_and(Value::is_object));
    Ok(match info {
        Some(info) => PageRoom::Found(info.clone()),
        None => PageRoom::Missing,
    })
}

/// Parse an `api-live/user/room` answer into `{user, liveRoom}`.
pub fn parse_api(body: &str) -> Result<Value, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited("TikTok 接口返回的不是 JSON，可能触发了风控".to_string())
        } else {
            PlatformError::Parse("TikTok 接口返回的数据无法解析".to_string())
        }
    })?;
    match json.get("statusCode").and_then(Value::as_i64) {
        Some(0) => {}
        Some(API_USER_NOT_FOUND) => {
            return Err(PlatformError::InvalidUrl(
                "TikTok 没有这个用户，请检查链接里的用户名".to_string(),
            ));
        }
        code => {
            let message = json
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(PlatformError::RateLimited(format!(
                "TikTok 接口返回错误 {}: {message}",
                code.map_or_else(|| "null".to_string(), |code| code.to_string())
            )));
        }
    }
    json.get("data")
        .filter(|data| data.get("user").is_some_and(Value::is_object))
        .cloned()
        .ok_or_else(|| PlatformError::Parse("TikTok 接口缺少用户信息".to_string()))
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// One entry of `stream_data.data` with a positive video bitrate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tier {
    name: String,
    bitrate: i64,
    width: i64,
    height: i64,
    flv: Option<String>,
    hls: Option<String>,
}

/// StreamGet appends the codec to every URL: `?codec=` after a bare `.flv` or
/// `.m3u8`, `&codec=` otherwise.
fn with_codec(url: &str, codec: &str) -> String {
    if codec.is_empty() {
        return url.to_string();
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let bare = !url.contains('?') && (path.ends_with(".flv") || path.ends_with(".m3u8"));
    if bare {
        format!("{url}?codec={codec}")
    } else {
        format!("{url}&codec={codec}")
    }
}

/// The tiers in StreamGet's order: video bitrate, then width and height, all
/// descending. Entries without bitrate or resolution (audio only) are left out.
fn parse_tiers(stream_data: &str) -> Result<Vec<Tier>, PlatformError> {
    let parse_error = || PlatformError::Parse("TikTok 直播流数据无法解析".to_string());
    let data: Value = serde_json::from_str(stream_data).map_err(|_| parse_error())?;
    let entries = data
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(parse_error)?;
    let mut tiers = Vec::new();
    for (name, entry) in entries {
        let Some(main) = entry.get("main") else {
            continue;
        };
        let params: Value = main
            .get("sdk_params")
            .and_then(Value::as_str)
            .and_then(|params| serde_json::from_str(params).ok())
            .unwrap_or(Value::Null);
        let bitrate = match params.get("vbitrate") {
            Some(Value::Number(number)) => number.as_i64().unwrap_or_default(),
            Some(Value::String(text)) => text.parse().unwrap_or_default(),
            _ => 0,
        };
        let resolution = params
            .get("resolution")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some((width, height)) = resolution.split_once('x') else {
            continue;
        };
        let (Ok(width), Ok(height)) = (width.trim().parse(), height.trim().parse()) else {
            continue;
        };
        if bitrate <= 0 {
            continue;
        }
        let codec = params
            .get("VCodec")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let url = |key: &str| text(main.get(key)).map(|url| with_codec(&url, codec));
        tiers.push(Tier {
            name: name.clone(),
            bitrate,
            width,
            height,
            flv: url("flv"),
            hls: url("hls"),
        });
    }
    tiers.sort_by(|a, b| {
        (b.bitrate, b.width, b.height)
            .cmp(&(a.bitrate, a.width, a.height))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(tiers)
}

/// Everything about the room except the chosen stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSnapshot {
    pub unique_id: String,
    pub user_id: Option<String>,
    pub anchor_name: String,
    pub title: Option<String>,
    pub is_live: bool,
    pub variants: Vec<StreamVariant>,
}

/// Read `{user, liveRoom}` into a snapshot. `link_id` names the room when
/// TikTok leaves the user name out.
pub fn parse_room(info: &Value, link_id: &str) -> Result<RoomSnapshot, PlatformError> {
    let user = info
        .get("user")
        .ok_or_else(|| PlatformError::Parse("TikTok 直播间缺少用户信息".to_string()))?;
    let unique_id = text(user.get("uniqueId"))
        .map(|id| id.to_ascii_lowercase())
        .unwrap_or_else(|| link_id.to_string());
    let anchor_name = text(user.get("nickname")).unwrap_or_else(|| unique_id.clone());
    let room = info.get("liveRoom").filter(|room| room.is_object());
    let is_live = user.get("status").and_then(Value::as_i64) == Some(2);
    let mut snapshot = RoomSnapshot {
        unique_id,
        user_id: text(user.get("id")),
        anchor_name,
        title: None,
        is_live,
        variants: Vec::new(),
    };
    if !is_live {
        return Ok(snapshot);
    }
    let room = room.ok_or_else(|| PlatformError::Parse("TikTok 直播间缺少房间信息".to_string()))?;
    snapshot.title = text(room.get("title"));
    let Some(stream_data) = room
        .get("streamData")
        .and_then(|data| data.get("pull_data"))
        .and_then(|data| data.get("stream_data"))
        .and_then(Value::as_str)
    else {
        return Err(PlatformError::AuthRequired(
            "这场 TikTok 直播可能不适合部分观众，需要登录确认年龄：请在设置里填写 TikTok Cookie"
                .to_string(),
        ));
    };
    snapshot.variants = parse_tiers(stream_data)?
        .into_iter()
        .enumerate()
        .map(|(index, tier)| StreamVariant {
            name: tier.name,
            quality: Quality::from_index(index),
            flv_url: tier.flv,
            hls_url: tier.hls,
        })
        .collect();
    if snapshot.variants.is_empty() {
        return Err(PlatformError::Parse(
            "TikTok 已开播，但没有返回可录制的视频流".to_string(),
        ));
    }
    Ok(snapshot)
}

/// Turn a snapshot into `LiveInfo`. `reachable` is false when StreamGet's
/// probe of the chosen tier failed; the neighbouring tier is used instead.
pub fn build_live_info(snapshot: RoomSnapshot, quality: Quality, reachable: bool) -> LiveInfo {
    let index = if reachable {
        select_variant_index(snapshot.variants.len(), quality)
    } else {
        fallback_variant_index(snapshot.variants.len(), quality)
    };
    let selected = index
        .and_then(|index| snapshot.variants.get(index))
        .filter(|_| snapshot.is_live)
        .and_then(|variant| selected_stream(quality, variant));
    LiveInfo {
        platform: Platform::Tiktok,
        live_url: Some(room_url(&snapshot.unique_id)),
        room_id: snapshot.unique_id,
        anchor_id: snapshot.user_id,
        anchor_name: snapshot.anchor_name,
        title: snapshot.title,
        is_live: snapshot.is_live && selected.is_some(),
        variants: snapshot.variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling TikTok streams: the PC User-Agent and Referer that
/// StreamGet probes with. User cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), PC_USER_AGENT.to_string()),
        ("Referer".to_string(), REFERER_VALUE.to_string()),
    ])
}

fn headers(user_agent: &'static str, cookie: &str) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(user_agent));
    headers.insert(REFERER, HeaderValue::from_static(REFERER_VALUE));
    headers.insert(COOKIE, cookie_header(cookie)?);
    Ok(headers)
}

/// `_get_pc_headers`: the user cookie, or StreamGet's default `ttwid`.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    headers(PC_USER_AGENT, cookie.unwrap_or(PC_DEFAULT_COOKIE))
}

/// `_get_mobile_headers`, used for the app API.
pub fn mobile_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    headers(MOBILE_USER_AGENT, cookie.unwrap_or(MOBILE_DEFAULT_COOKIE))
}

/// URL of the `api-live/user/room` request, with StreamGet's parameters.
pub fn api_url(unique_id: &str) -> String {
    let mut url = Url::parse(ROOM_API).expect("valid room API URL");
    url.query_pairs_mut()
        .append_pair("aid", "1988")
        .append_pair("app_language", "en")
        .append_pair("os", "android")
        .append_pair("referer", REFERER_VALUE)
        .append_pair("sourceType", "54")
        .append_pair("uniqueId", unique_id);
    url.to_string()
}

/// TikTok adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct TiktokAdapter {
    clients: ClientCache,
}

impl TiktokAdapter {
    pub fn new() -> Self {
        TiktokAdapter::default()
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

    /// The user behind a share short link.
    async fn resolve_share_link(
        url: &str,
        options: &FetchOptions,
    ) -> Result<String, PlatformError> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(PC_USER_AGENT));
        let target = super::follow_short_link(options.proxy.as_deref(), url, headers, |url| {
            matches!(parse_link(url.as_str()), Ok(TiktokLink::User { .. }))
        })
        .await?;
        match parse_link(&target) {
            Ok(TiktokLink::User { unique_id }) => Ok(unique_id),
            _ => Err(PlatformError::InvalidUrl(format!(
                "TikTok 分享链接没有指向直播间，请检查链接是否失效: {target}"
            ))),
        }
    }

    /// StreamGet's `get_response_status` probe: only transport failures
    /// count, any HTTP status is "reachable".
    async fn probe(client: &reqwest::Client, url: &str) -> bool {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(PC_USER_AGENT));
        headers.insert(REFERER, HeaderValue::from_static(REFERER_VALUE));
        client
            .head(url)
            .headers(headers)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .is_ok()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let unique_id = match parse_link(url)? {
            TiktokLink::User { unique_id } => unique_id,
            TiktokLink::Share { url } => Self::resolve_share_link(&url, options).await?,
        };
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let page = Self::get(&client, &room_url(&unique_id), pc_headers(cookie)?).await?;
        let info = match parse_page(&page)? {
            PageRoom::Found(info) => info,
            // StreamGet's app API answers for users the page leaves out and
            // tells an unknown user apart.
            PageRoom::Missing => {
                let body =
                    Self::get(&client, &api_url(&unique_id), mobile_headers(cookie)?).await?;
                parse_api(&body)?
            }
        };
        let snapshot = parse_room(&info, &unique_id)?;

        let mut reachable = true;
        if snapshot.is_live {
            let primary = select_variant_index(snapshot.variants.len(), options.quality)
                .and_then(|index| snapshot.variants.get(index))
                .and_then(|variant| variant.hls_url.clone().or(variant.flv_url.clone()));
            if let Some(primary) = primary {
                reachable = Self::probe(&client, &primary).await;
            }
        }
        Ok(build_live_info(snapshot, options.quality, reachable))
    }
}

impl PlatformAdapter for TiktokAdapter {
    fn platform(&self) -> Platform {
        Platform::Tiktok
    }

    fn matches(&self, url: &str) -> bool {
        parse_link(url).is_ok()
    }

    fn normalize_url(&self, url: &str) -> Option<String> {
        parse_link(url).ok().and_then(|link| link.normalized_url())
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

    const PAGE_LIVE: &str = include_str!("../../tests/fixtures/tiktok/page_live.html");
    const PAGE_OFFLINE: &str = include_str!("../../tests/fixtures/tiktok/page_offline.html");
    const PAGE_MISSING: &str = include_str!("../../tests/fixtures/tiktok/page_missing.html");
    const API_LIVE_MULTI: &str = include_str!("../../tests/fixtures/tiktok/api_live_multi.json");
    const API_MISSING: &str = include_str!("../../tests/fixtures/tiktok/api_missing.json");

    fn user(id: &str) -> TiktokLink {
        TiktokLink::User {
            unique_id: id.to_string(),
        }
    }

    fn found(html: &str) -> Value {
        match parse_page(html).unwrap() {
            PageRoom::Found(info) => info,
            PageRoom::Missing => panic!("page has no room"),
        }
    }

    #[test]
    fn recognizes_and_normalizes_user_links() {
        let adapter = TiktokAdapter::new();
        for (input, id) in [
            ("https://www.tiktok.com/@nbcnews/live", "nbcnews"),
            (
                "https://www.tiktok.com/@NBCNews/live?lang=en&enter_from=x",
                "nbcnews",
            ),
            ("www.tiktok.com/@some.user_1", "some.user_1"),
            ("https://m.tiktok.com/@abc/live#x", "abc"),
            ("tiktok.com/@abc/video/123", "abc"),
        ] {
            assert_eq!(parse_link(input).unwrap(), user(id), "{input}");
            assert!(adapter.matches(input), "{input}");
            assert_eq!(
                adapter.normalize_url(input),
                Some(format!("https://www.tiktok.com/@{id}/live")),
                "{input}"
            );
        }
        let tiktok = super::super::adapter_for("https://www.tiktok.com/@nbcnews/live").unwrap();
        assert_eq!(tiktok.platform(), Platform::Tiktok);
        assert_eq!(Platform::Tiktok.key(), "tiktok");
        assert_eq!(Platform::Tiktok.display_name(), "TikTok");
    }

    #[test]
    fn share_links_are_resolved_later() {
        for input in [
            "https://vt.tiktok.com/ZSabc123/",
            "https://vm.tiktok.com/ZMabc/",
            "https://www.tiktok.com/t/ZTabc/",
        ] {
            assert!(
                matches!(parse_link(input).unwrap(), TiktokLink::Share { .. }),
                "{input}"
            );
            assert_eq!(TiktokAdapter::new().normalize_url(input), None, "{input}");
        }
    }

    #[test]
    fn rejects_links_without_a_user_and_other_sites() {
        for input in [
            "https://www.tiktok.com/",
            "https://www.tiktok.com/live",
            "https://www.tiktok.com/@/live",
            "https://www.tiktok.com/@bad-name!/live",
            "https://www.tiktok.com/@abcdefghijklmnopqrstuvwxyz/live",
            "https://vt.tiktok.com/",
            "https://www.tiktok.com.evil.example/@abc/live",
            "https://nottiktok.com/@abc/live",
            "ftp://www.tiktok.com/@abc/live",
            "https://live.douyin.com/745964462470",
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
    fn live_page_records_the_video_tier_with_codec() {
        let snapshot = parse_room(&found(PAGE_LIVE), "nbcnews").unwrap();
        assert!(snapshot.is_live);
        assert_eq!(snapshot.unique_id, "nbcnews");
        assert_eq!(snapshot.user_id.as_deref(), Some("6652774777768427525"));
        assert_eq!(snapshot.anchor_name, "nbcnews");
        assert_eq!(
            snapshot.title.as_deref(),
            Some("Iowa detonation press conference")
        );
        // The audio-only entry has no bitrate or resolution and is left out.
        assert_eq!(snapshot.variants.len(), 1);
        for quality in Quality::ALL {
            let info = build_live_info(snapshot.clone(), quality, true);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Tiktok);
            assert_eq!(info.room_id, "nbcnews");
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://www.tiktok.com/@nbcnews/live")
            );
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert_eq!(selected.variant.name, "hd");
            let flv = selected.variant.flv_url.as_deref().unwrap();
            assert!(flv.starts_with("https://pull-f5-tt01.tiktokcdn-us.com/game/stream-"));
            assert!(flv.ends_with("&codec=h264"), "{flv}");
            assert!(selected.record_url.contains("/index.m3u8?"));
            assert_eq!(selected.flv_codec().as_deref(), Some("h264"));
            // TikTok is FLV-preferred like Douyin.
            assert_eq!(
                selected.source_url(true, false).0,
                super::super::SourceFormat::Flv
            );
            assert!(
                info.stream_headers
                    .keys()
                    .all(|key| !key.eq_ignore_ascii_case("cookie"))
            );
        }
    }

    #[test]
    fn tiers_follow_bitrate_then_resolution_and_pad_like_streamget() {
        let info = parse_api(API_LIVE_MULTI).unwrap();
        let snapshot = parse_room(&info, "tiktok").unwrap();
        let names: Vec<&str> = snapshot.variants.iter().map(|v| v.name.as_str()).collect();
        // vbitrate: uhd_60 5M, hd_60 3M, uhd 1.8M, hd 1.2M, origin 1M, sd 0.8M, ld 0.6M.
        assert_eq!(
            names,
            ["uhd_60", "hd_60", "uhd", "hd", "origin", "sd", "ld"]
        );
        assert_eq!(snapshot.variants[0].quality, Some(Quality::Od));
        assert_eq!(snapshot.variants[4].quality, Some(Quality::Ld));
        assert_eq!(snapshot.variants[5].quality, None);
        let expected = [
            (Quality::Od, "uhd_60"),
            (Quality::Uhd, "hd_60"),
            (Quality::Hd, "uhd"),
            (Quality::Sd, "hd"),
            (Quality::Ld, "origin"),
        ];
        for (quality, name) in expected {
            let info = build_live_info(snapshot.clone(), quality, true);
            assert_eq!(info.selected.unwrap().variant.name, name, "{quality}");
        }
        // An unreachable tier falls back one step down (one up from LD).
        let info = build_live_info(snapshot.clone(), Quality::Od, false);
        assert_eq!(info.selected.unwrap().variant.name, "hd_60");
        let info = build_live_info(snapshot, Quality::Ld, false);
        assert_eq!(info.selected.unwrap().variant.name, "hd");
    }

    #[test]
    fn offline_user_has_no_stream() {
        let snapshot = parse_room(&found(PAGE_OFFLINE), "tiktok").unwrap();
        assert!(!snapshot.is_live);
        assert_eq!(snapshot.anchor_name, "TikTok");
        assert_eq!(snapshot.user_id.as_deref(), Some("107955"));
        let info = build_live_info(snapshot, Quality::Od, true);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        assert_eq!(info.title, None);
    }

    #[test]
    fn unknown_user_page_falls_back_to_the_api_which_rejects_it() {
        assert_eq!(parse_page(PAGE_MISSING).unwrap(), PageRoom::Missing);
        let error = parse_api(API_MISSING).unwrap_err();
        assert_eq!(error.kind(), "invalid_url", "{error}");
    }

    #[test]
    fn blocked_regions_risk_control_and_age_gates_are_explained() {
        let blocked = parse_page(
            "<p>\n   We regret to inform you that we have discontinued operating TikTok in Hong Kong.\n  </p>",
        )
        .unwrap_err();
        assert_eq!(blocked.kind(), "config", "{blocked}");
        assert!(blocked.message().contains("换其他地区"), "{blocked}");
        for html in ["", "<html><body>captcha</body></html>"] {
            assert_eq!(parse_page(html).unwrap_err().kind(), "rate_limited");
        }
        let gated = PAGE_LIVE.replacen("\"streamData\":", "\"noStreamData\":", 1);
        let error = parse_room(&found(&gated), "nbcnews").unwrap_err();
        assert_eq!(error.kind(), "auth_required", "{error}");
        for (body, kind) in [
            ("<html>", "rate_limited"),
            ("{", "parse"),
            (r#"{"statusCode":10000,"message":"x"}"#, "rate_limited"),
            (r#"{"statusCode":0,"data":null}"#, "parse"),
        ] {
            assert_eq!(parse_api(body).unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn codec_is_appended_like_streamget() {
        assert_eq!(
            with_codec("https://a/b.flv", "h264"),
            "https://a/b.flv?codec=h264"
        );
        assert_eq!(
            with_codec("https://a/b.flv?expire=1", "h265"),
            "https://a/b.flv?expire=1&codec=h265"
        );
        assert_eq!(with_codec("https://a/b.flv", ""), "https://a/b.flv");
    }

    #[test]
    fn requests_send_the_user_cookie_or_streamget_defaults() {
        let headers = pc_headers(None).unwrap();
        assert!(
            headers
                .get(COOKIE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("ttwid=")
        );
        assert_eq!(headers.get(REFERER).unwrap(), "https://www.tiktok.com/");
        assert_eq!(
            mobile_headers(Some("sessionid=1"))
                .unwrap()
                .get(COOKIE)
                .unwrap(),
            "sessionid=1"
        );
        assert_eq!(pc_headers(Some("a=1\nb=2")).unwrap_err().kind(), "config");
        assert_eq!(
            api_url("nbcnews"),
            "https://www.tiktok.com/api-live/user/room?aid=1988&app_language=en&os=android&referer=https%3A%2F%2Fwww.tiktok.com%2F&sourceType=54&uniqueId=nbcnews"
        );
    }
}
