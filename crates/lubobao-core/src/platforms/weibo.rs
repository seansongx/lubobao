//! Weibo live adapter, following StreamCap's `WeiboHandler` and StreamGet
//! v4.0.10 `WeiboLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/weibo/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The request flow, headers and the default visitor cookie
//! follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * A room link `weibo.com/l/wblive/p/show/{live_id}` (StreamCap README
//!   example `…/show/1022:2321325026370190442592`) carries the live id.
//! * A profile link `weibo.com/u/{uid}` is resolved like StreamGet: the
//!   first page of the user's posts (`ajax/statuses/mymblog`) is searched
//!   for a post whose `page_info.object_type` is `live`. Weibo only answers
//!   that API for logged-in accounts, so profile links need the user's
//!   cookie; without one the check reports that login is required.
//! * `weibo.com/l/pc/anchor/live?live_id=…` answers with the anchor
//!   (`user_info`), the title (`item.desc`), the state (`item.status`, 1 =
//!   live) and one stream pair (`item.stream_info.pull`). Unknown rooms and
//!   deleted anchors come back as HTTP 400 with an `error_code`.
//!
//! Streams: the API URL usually names a transcode (`{stream}_wb720avc`);
//! StreamGet adds the bare `{stream}` as its second entry, which is the
//! source stream. Intentional difference: the source comes first here, so
//! 原画 records the source (1080p in tests) and lower tiers the transcode,
//! where StreamGet's `OD` records the 720p transcode and every lower tier
//! the source. When the API URL has no suffix there is one variant.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, REFERER, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

/// `WeiboLiveStream.pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:124.0) Gecko/20100101 Firefox/124.0";
const ACCEPT_LANGUAGE_VALUE: &str = "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6";
/// Referer StreamGet sends with every request.
const API_REFERER: &str = "https://weibo.com/u/5885340893";
const WEB_REFERER: &str = "https://weibo.com/";
/// Visitor cookie StreamGet sends when the user has none.
const DEFAULT_COOKIE: &str = "SUB=_2AkMRNMCwf8NxqwFRmfwWymPrbI9-zgzEieKnaDFrJRMxHRl-yT9kqmkhtRB6OrTuX5z9N_7qk9C3xxEmNR-8WLcyo2PM; SUBP=0033WrSXqPxfM72-Ws9jqgMF55529P9D9WWemwcqkukCduUO11o9sBqA;";
const ROOM_PAGE: &str = "https://weibo.com/l/wblive/p/show/";
const PROFILE_PAGE: &str = "https://weibo.com/u/";
const ROOM_API: &str = "https://weibo.com/l/pc/anchor/live";
const POSTS_API: &str = "https://weibo.com/ajax/statuses/mymblog";
/// Name of the source variant.
const SOURCE_VARIANT_NAME: &str = "原画";
/// Weibo `error_code` for a live id that does not exist.
const ERROR_ROOM_MISSING: i64 = 27401;
/// Weibo `error_code` for a deleted or unknown anchor.
const ERROR_USER_MISSING: i64 = 20003;

/// A recognized Weibo link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeiboLink {
    /// `weibo.com/l/wblive/p/show/{live_id}`.
    Room { live_id: String },
    /// `weibo.com/u/{uid}`.
    Profile { uid: String },
}

impl WeiboLink {
    /// Canonical URL for the link.
    pub fn normalized_url(&self) -> String {
        match self {
            WeiboLink::Room { live_id } => format!("{ROOM_PAGE}{live_id}"),
            WeiboLink::Profile { uid } => format!("{PROFILE_PAGE}{uid}"),
        }
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Live ids look like `1022:2321325368700659433556`.
fn is_live_id(text: &str) -> bool {
    text.split_once(':')
        .is_some_and(|(kind, id)| is_digits(kind) && is_digits(id))
}

/// Recognize and normalize a Weibo link without any network access.
///
/// StreamCap registers `weibo\.com/` and StreamGet takes the part after
/// `show/` as the live id, or the part after `/u/` as a user id. Only those
/// two page forms are accepted.
pub fn parse_link(input: &str) -> Result<WeiboLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的微博直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    let mobile = match host.as_str() {
        "weibo.com" | "www.weibo.com" => false,
        "m.weibo.cn" => true,
        _ => return Err(invalid()),
    };
    let segments: Vec<String> = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .map(|segment| segment.replace("%3A", ":").replace("%3a", ":"))
                .collect()
        })
        .unwrap_or_default();
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    // Profiles also come as `weibo.com/{uid}` and, on the mobile site,
    // `m.weibo.cn/u/{uid}` or `m.weibo.cn/profile/{uid}` (an intentional
    // extension over StreamCap).
    match (mobile, segments.as_slice()) {
        (false, ["l", "wblive", "p", "show", live_id]) if is_live_id(live_id) => {
            Ok(WeiboLink::Room {
                live_id: live_id.to_string(),
            })
        }
        (false, ["u", uid] | [uid]) | (true, ["u" | "profile", uid]) if is_digits(uid) => {
            Ok(WeiboLink::Profile {
                uid: uid.to_string(),
            })
        }
        _ => Err(invalid()),
    }
}

/// URL of the room API for a live id.
pub fn room_api_url(live_id: &str) -> String {
    let mut url = Url::parse(ROOM_API).expect("valid room API URL");
    url.query_pairs_mut().append_pair("live_id", live_id);
    url.to_string()
}

/// URL of the posts API StreamGet reads for profile links.
pub fn posts_api_url(uid: &str) -> String {
    let mut url = Url::parse(POSTS_API).expect("valid posts API URL");
    url.query_pairs_mut()
        .append_pair("uid", uid)
        .append_pair("page", "1")
        .append_pair("feature", "0");
    url.to_string()
}

fn parse_json(body: &str, what: &str) -> Result<Value, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited(format!("微博{what}返回的不是 JSON，可能触发风控"))
        } else {
            PlatformError::Parse(format!("微博{what}返回的数据无法解析"))
        }
    })?;
    if json.is_object() {
        Ok(json)
    } else {
        Err(PlatformError::Parse(format!(
            "微博{what}返回的数据无法解析"
        )))
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Live room reported by the room API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomInfo {
    pub live_id: String,
    pub anchor_id: Option<String>,
    pub anchor_name: String,
    pub title: Option<String>,
    pub is_live: bool,
    /// Source first, then the transcode. Empty when offline.
    pub variants: Vec<StreamVariant>,
}

/// Parse a room API answer. `status` is the HTTP status, because Weibo
/// reports unknown rooms with HTTP 400 and a JSON body.
pub fn parse_room(status: u16, body: &str, live_id: &str) -> Result<RoomInfo, PlatformError> {
    let json = match parse_json(body, "直播间接口") {
        Ok(json) => json,
        Err(_) if status >= 400 => return Err(classify_http_status(status, ROOM_API)),
        Err(error) => return Err(error),
    };
    if let Some(code) = json.get("error_code").and_then(Value::as_i64)
        && code != 0
    {
        let message = text(json.get("error")).unwrap_or_default();
        return Err(match code {
            ERROR_ROOM_MISSING => {
                PlatformError::InvalidUrl("微博没有找到这个直播间，请检查链接".to_string())
            }
            ERROR_USER_MISSING => {
                PlatformError::InvalidUrl("这个微博直播间的主播账号不存在或已注销".to_string())
            }
            _ if status >= 400 => classify_http_status(status, &format!("{code} {message}")),
            _ => PlatformError::Parse(format!("微博直播间接口返回错误 {code}: {message}")),
        });
    }
    if status >= 400 {
        return Err(classify_http_status(status, ROOM_API));
    }
    let data = json
        .get("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| PlatformError::Parse("微博直播间接口缺少 data".to_string()))?;
    let item = data
        .get("item")
        .filter(|item| item.is_object())
        .ok_or_else(|| PlatformError::Parse("微博直播间接口缺少直播信息".to_string()))?;
    let user = data.get("user_info");
    let anchor_id = user
        .and_then(|user| text(user.get("idstr")).or_else(|| text(user.get("id"))))
        .or_else(|| text(item.get("anchor_uid")));
    let anchor_name = user
        .and_then(|user| text(user.get("name")).or_else(|| text(user.get("screen_name"))))
        .or_else(|| anchor_id.as_ref().map(|id| format!("微博 {id}")))
        .unwrap_or_else(|| format!("微博 {live_id}"));
    let title = text(item.get("desc"));
    let live = item.get("status").and_then(Value::as_i64) == Some(1);
    let variants = if live {
        let pull = item.get("stream_info").and_then(|info| info.get("pull"));
        let hls = text(pull.and_then(|pull| pull.get("live_origin_hls_url")));
        let flv = text(pull.and_then(|pull| pull.get("live_origin_flv_url")));
        if hls.is_none() && flv.is_none() {
            return Err(PlatformError::Parse(
                "微博直播间正在直播，但接口没有返回流地址".to_string(),
            ));
        }
        stream_variants(hls.as_deref(), flv.as_deref())
    } else {
        Vec::new()
    };
    Ok(RoomInfo {
        live_id: text(item.get("live_id")).unwrap_or_else(|| live_id.to_string()),
        anchor_id,
        anchor_name,
        title,
        is_live: live,
        variants,
    })
}

/// Split a stream URL's file name at its first `_`, StreamGet's
/// `url.split('_')[0] + ext`, limited to the last path segment so a host or
/// directory with `_` stays intact. Returns the bare URL and the suffix.
fn split_transcode(url: &str, ext: &str) -> Option<(String, String)> {
    let path_end = url.find(['?', '#']).unwrap_or(url.len());
    let name_start = url[..path_end].rfind('/')? + 1;
    let name = &url[name_start..path_end];
    let stem = name.strip_suffix(ext)?;
    let (base, suffix) = stem.split_once('_')?;
    if base.is_empty() || suffix.is_empty() {
        return None;
    }
    Some((
        format!("{}{base}{ext}", &url[..name_start]),
        suffix.to_string(),
    ))
}

/// Variants from the API's stream pair: the source (suffix removed) first,
/// then the transcode the API named, or a single variant when the API URL
/// already is the source.
pub fn stream_variants(hls: Option<&str>, flv: Option<&str>) -> Vec<StreamVariant> {
    let hls_split = hls.and_then(|url| split_transcode(url, ".m3u8"));
    let flv_split = flv.and_then(|url| split_transcode(url, ".flv"));
    if hls_split.is_none() && flv_split.is_none() {
        return vec![StreamVariant {
            name: SOURCE_VARIANT_NAME.to_string(),
            quality: Some(Quality::Od),
            flv_url: flv.map(str::to_string),
            hls_url: hls.map(str::to_string),
        }];
    }
    let suffix = hls_split
        .as_ref()
        .or(flv_split.as_ref())
        .map(|(_, suffix)| suffix.clone())
        .unwrap_or_default();
    vec![
        StreamVariant {
            name: SOURCE_VARIANT_NAME.to_string(),
            quality: Some(Quality::Od),
            flv_url: flv_split.as_ref().map(|(url, _)| url.clone()),
            hls_url: hls_split.as_ref().map(|(url, _)| url.clone()),
        },
        StreamVariant {
            name: suffix,
            quality: Some(Quality::Uhd),
            flv_url: flv.map(str::to_string),
            hls_url: hls.map(str::to_string),
        },
    ]
}

/// What the posts API says about a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileLive {
    /// Live id of the first live post, when any.
    pub live_id: Option<String>,
    /// `screen_name` of the first post's author.
    pub anchor_name: Option<String>,
}

/// Parse the posts API answer for a profile link. `has_user_cookie` picks
/// the message when Weibo refuses the request.
pub fn parse_posts(
    status: u16,
    body: &str,
    has_user_cookie: bool,
) -> Result<ProfileLive, PlatformError> {
    let login = || {
        PlatformError::AuthRequired(if has_user_cookie {
            "微博拒绝了主页请求，Cookie 可能已失效，请在设置里更新微博 Cookie".to_string()
        } else {
            "微博主页链接需要登录：请在设置里填写微博 Cookie，或改用直播间链接".to_string()
        })
    };
    if matches!(status, 401 | 403) {
        return Err(login());
    }
    if status >= 400 {
        return Err(classify_http_status(status, POSTS_API));
    }
    let json = parse_json(body, "主页接口")?;
    if json.get("ok").and_then(Value::as_i64) != Some(1) {
        return Err(login());
    }
    let list = json
        .get("data")
        .and_then(|data| data.get("list"))
        .and_then(Value::as_array)
        .ok_or_else(|| PlatformError::Parse("微博主页接口缺少微博列表".to_string()))?;
    let live_id = list.iter().find_map(|post| {
        let page = post.get("page_info")?;
        if page.get("object_type")?.as_str()? != "live" {
            return None;
        }
        text(page.get("object_id"))
    });
    let anchor_name = list
        .first()
        .and_then(|post| post.get("user"))
        .and_then(|user| text(user.get("screen_name")));
    Ok(ProfileLive {
        live_id,
        anchor_name,
    })
}

/// Turn a room answer into `LiveInfo`.
pub fn build_live_info(room: RoomInfo, quality: Quality) -> LiveInfo {
    let selected = select_variant_index(room.variants.len(), quality)
        .and_then(|index| selected_stream(quality, &room.variants[index]));
    LiveInfo {
        platform: Platform::Weibo,
        live_url: Some(format!("{ROOM_PAGE}{}", room.live_id)),
        room_id: room.live_id,
        anchor_id: room.anchor_id,
        anchor_name: room.anchor_name,
        title: room.title,
        is_live: room.is_live && selected.is_some(),
        variants: if selected.is_some() {
            room.variants
        } else {
            Vec::new()
        },
        selected,
        stream_headers: stream_headers(),
    }
}

/// `LiveInfo` for a profile without a live post: offline, keyed by the uid.
pub fn profile_offline_info(uid: &str, anchor_name: Option<String>) -> LiveInfo {
    LiveInfo {
        platform: Platform::Weibo,
        room_id: uid.to_string(),
        anchor_id: Some(uid.to_string()),
        live_url: Some(format!("{PROFILE_PAGE}{uid}")),
        anchor_name: anchor_name.unwrap_or_else(|| format!("微博 {uid}")),
        title: None,
        is_live: false,
        variants: Vec::new(),
        selected: None,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Weibo streams: User-Agent and the web Referer. User
/// cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Referer".to_string(), WEB_REFERER.to_string()),
    ])
}

/// `WeiboLiveStream.pc_headers`: the user cookie, or StreamGet's visitor
/// cookie when none is set.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
    );
    headers.insert(REFERER, HeaderValue::from_static(API_REFERER));
    headers.insert(COOKIE, cookie_header(cookie.unwrap_or(DEFAULT_COOKIE))?);
    Ok(headers)
}

/// Weibo adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct WeiboAdapter {
    clients: ClientCache,
}

impl WeiboAdapter {
    pub fn new() -> Self {
        WeiboAdapter::default()
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
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let live_id = match link {
            WeiboLink::Room { live_id } => live_id,
            WeiboLink::Profile { uid } => {
                let (status, body) = Self::get(&client, &posts_api_url(&uid), cookie).await?;
                let profile = parse_posts(status, &body, cookie.is_some())?;
                match profile.live_id {
                    Some(live_id) => live_id,
                    None => return Ok(profile_offline_info(&uid, profile.anchor_name)),
                }
            }
        };
        let (status, body) = Self::get(&client, &room_api_url(&live_id), cookie).await?;
        let room = parse_room(status, &body, &live_id)?;
        Ok(build_live_info(room, options.quality))
    }
}

impl PlatformAdapter for WeiboAdapter {
    fn platform(&self) -> Platform {
        Platform::Weibo
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

    const ROOM_LIVE: &str = include_str!("../../tests/fixtures/weibo/room_live.json");
    const ROOM_LIVE_SOURCE_ONLY: &str =
        include_str!("../../tests/fixtures/weibo/room_live_source_only.json");
    const ROOM_OFFLINE: &str = include_str!("../../tests/fixtures/weibo/room_offline.json");
    const ROOM_MISSING: &str = include_str!("../../tests/fixtures/weibo/room_missing.json");
    const USER_MISSING: &str = include_str!("../../tests/fixtures/weibo/user_missing.json");
    const POSTS_LIVE: &str = include_str!("../../tests/fixtures/weibo/posts_live.json");
    const POSTS_NO_LIVE: &str = include_str!("../../tests/fixtures/weibo/posts_no_live.json");

    const LIVE_ID: &str = "1022:2321320000000000000001";

    #[test]
    fn recognizes_and_normalizes_links() {
        let adapter = WeiboAdapter::new();
        let room = |id: &str| WeiboLink::Room {
            live_id: id.to_string(),
        };
        let cases = [
            (
                "https://weibo.com/l/wblive/p/show/1022:2321325026370190442592",
                room("1022:2321325026370190442592"),
            ),
            (
                "  weibo.com/l/wblive/p/show/1022:2321325368700659433556?from=x#y ",
                room("1022:2321325368700659433556"),
            ),
            (
                "https://www.weibo.com/l/wblive/p/show/1022%3A123/",
                room("1022:123"),
            ),
            (
                "http://WEIBO.COM/u/5509110125",
                WeiboLink::Profile {
                    uid: "5509110125".to_string(),
                },
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_link(input).unwrap(), expected, "{input}");
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            adapter
                .normalize_url("www.weibo.com/l/wblive/p/show/1022:5?a=1")
                .as_deref(),
            Some("https://weibo.com/l/wblive/p/show/1022:5")
        );
        assert_eq!(
            adapter.normalize_url("weibo.com/u/123/").as_deref(),
            Some("https://weibo.com/u/123")
        );
        let weibo = super::super::adapter_for(
            "https://weibo.com/l/wblive/p/show/1022:2321325026370190442592",
        )
        .unwrap();
        assert_eq!(weibo.platform(), Platform::Weibo);
        assert_eq!(Platform::Weibo.key(), "weibo");
        assert_eq!(Platform::Weibo.display_name(), "微博直播");
    }

    #[test]
    fn profile_links_without_u_and_on_the_mobile_site_are_recognized() {
        let adapter = WeiboAdapter::new();
        for input in [
            "https://weibo.com/5509110125",
            "https://www.weibo.com/5509110125?refer_flag=1001030103_",
            "https://m.weibo.cn/u/5509110125",
            "m.weibo.cn/profile/5509110125/",
        ] {
            assert_eq!(
                parse_link(input).unwrap(),
                WeiboLink::Profile {
                    uid: "5509110125".into()
                },
                "{input}"
            );
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some("https://weibo.com/u/5509110125"),
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        let adapter = WeiboAdapter::new();
        for input in [
            "https://weibo.com/",
            "https://weibo.com/l/wblive/p/show/",
            "https://weibo.com/l/wblive/p/show/abc",
            "https://weibo.com/l/wblive/p/show/1022",
            "https://weibo.com/u/abc",
            "https://weibo.com/1669879400/Abcdef",
            "https://weibo.com/abc",
            "https://m.weibo.cn/status/5509110125",
            "https://m.weibo.cn/l/wblive/p/show/1022:5",
            "https://weibo.com.evil.example/u/1",
            "ftp://weibo.com/u/1",
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
    fn live_room_puts_the_source_before_the_transcode() {
        let room = parse_room(200, ROOM_LIVE, LIVE_ID).unwrap();
        assert!(room.is_live);
        assert_eq!(room.anchor_id.as_deref(), Some("100001"));
        assert_eq!(room.anchor_name, "测试主播🎤");
        assert_eq!(room.title.as_deref(), Some("测试直播间 标题"));
        assert_eq!(
            room.variants,
            vec![
                StreamVariant {
                    name: "原画".to_string(),
                    quality: Some(Quality::Od),
                    flv_url: Some("https://plwb00.live.weibo.com/alicdn/500001.flv".to_string()),
                    hls_url: Some("https://plwb00.live.weibo.com/alicdn/500001.m3u8".to_string()),
                },
                StreamVariant {
                    name: "wb720avc".to_string(),
                    quality: Some(Quality::Uhd),
                    flv_url: Some(
                        "https://plwb00.live.weibo.com/alicdn/500001_wb720avc.flv".to_string()
                    ),
                    hls_url: Some(
                        "https://plwb00.live.weibo.com/alicdn/500001_wb720avc.m3u8".to_string()
                    ),
                },
            ]
        );
        for (quality, url) in [
            (
                Quality::Od,
                "https://plwb00.live.weibo.com/alicdn/500001.m3u8",
            ),
            (
                Quality::Uhd,
                "https://plwb00.live.weibo.com/alicdn/500001_wb720avc.m3u8",
            ),
            (
                Quality::Ld,
                "https://plwb00.live.weibo.com/alicdn/500001_wb720avc.m3u8",
            ),
        ] {
            let info = build_live_info(room.clone(), quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Weibo);
            assert_eq!(info.room_id, LIVE_ID);
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://weibo.com/l/wblive/p/show/1022:2321320000000000000001")
            );
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            // StreamGet `record_url` is the m3u8, and Weibo is not FLV-preferred.
            assert_eq!(
                selected.source_url(false, false),
                (super::super::SourceFormat::Hls, url)
            );
            assert!(
                info.stream_headers
                    .keys()
                    .all(|key| !key.eq_ignore_ascii_case("cookie"))
            );
        }
    }

    #[test]
    fn a_source_only_room_has_one_variant() {
        let room = parse_room(200, ROOM_LIVE_SOURCE_ONLY, LIVE_ID).unwrap();
        assert_eq!(room.variants.len(), 1);
        assert_eq!(room.variants[0].name, "原画");
        assert_eq!(
            room.variants[0].hls_url.as_deref(),
            Some("https://plwb20.live.weibo.com/alicdn/500002.m3u8")
        );
        let info = build_live_info(room, Quality::Ld);
        assert_eq!(
            info.selected.unwrap().record_url,
            "https://plwb20.live.weibo.com/alicdn/500002.m3u8"
        );
    }

    #[test]
    fn transcode_split_only_touches_the_file_name() {
        assert_eq!(
            split_transcode("https://a_b.example/x_y/123_wb720avc.m3u8?t=1", ".m3u8"),
            Some((
                "https://a_b.example/x_y/123.m3u8".to_string(),
                "wb720avc".to_string()
            ))
        );
        assert_eq!(split_transcode("https://a.example/123.m3u8", ".m3u8"), None);
        assert_eq!(
            split_transcode("https://a.example/123_x.flv", ".m3u8"),
            None
        );
        assert_eq!(split_transcode("https://a.example/_x.m3u8", ".m3u8"), None);
    }

    #[test]
    fn offline_and_missing_rooms() {
        let room = parse_room(200, ROOM_OFFLINE, LIVE_ID).unwrap();
        assert!(!room.is_live);
        assert!(room.variants.is_empty());
        let info = build_live_info(room, Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert_eq!(info.anchor_name, "测试主播🎤");

        let missing = parse_room(400, ROOM_MISSING, LIVE_ID).unwrap_err();
        assert_eq!(missing.kind(), "invalid_url", "{missing}");
        assert!(missing.message().contains("没有找到"), "{missing}");
        let gone = parse_room(400, USER_MISSING, LIVE_ID).unwrap_err();
        assert_eq!(gone.kind(), "invalid_url", "{gone}");
        assert!(gone.message().contains("不存在"), "{gone}");

        for (status, body, kind) in [
            (200, "", "rate_limited"),
            (200, "<html>", "rate_limited"),
            (403, "<h2>403 Forbidden</h2>", "rate_limited"),
            (502, "", "network"),
            (200, "[]", "parse"),
            (200, r#"{"error_code":0,"data":{}}"#, "parse"),
            (
                200,
                r#"{"error_code":0,"data":{"item":{"status":1,"stream_info":{"pull":{}}}}}"#,
                "parse",
            ),
            (400, r#"{"error_code":10001,"error":"x"}"#, "network"),
        ] {
            assert_eq!(
                parse_room(status, body, LIVE_ID).unwrap_err().kind(),
                kind,
                "{status} {body}"
            );
        }
    }

    #[test]
    fn profile_links_find_the_live_post_or_ask_for_login() {
        assert_eq!(
            parse_posts(200, POSTS_LIVE, true).unwrap(),
            ProfileLive {
                live_id: Some(LIVE_ID.to_string()),
                anchor_name: Some("测试主播🎤".to_string()),
            }
        );
        let none = parse_posts(200, POSTS_NO_LIVE, true).unwrap();
        assert_eq!(none.live_id, None);
        let info = profile_offline_info("100001", none.anchor_name);
        assert!(!info.is_live);
        assert_eq!(info.anchor_name, "测试主播🎤");
        assert_eq!(info.live_url.as_deref(), Some("https://weibo.com/u/100001"));

        let anonymous = parse_posts(403, r#"{"error":"Forbidden"}"#, false).unwrap_err();
        assert_eq!(anonymous.kind(), "auth_required");
        assert!(anonymous.message().contains("需要登录"), "{anonymous}");
        let expired = parse_posts(200, r#"{"ok":-100,"url":"login"}"#, true).unwrap_err();
        assert_eq!(expired.kind(), "auth_required");
        assert!(expired.message().contains("失效"), "{expired}");
        assert_eq!(
            parse_posts(200, "<html>", true).unwrap_err().kind(),
            "rate_limited"
        );
    }

    #[test]
    fn requests_send_the_visitor_cookie_unless_the_user_has_one() {
        assert_eq!(
            pc_headers(None).unwrap().get(COOKIE).unwrap(),
            DEFAULT_COOKIE
        );
        assert_eq!(
            pc_headers(Some("SUB=1")).unwrap().get(COOKIE).unwrap(),
            "SUB=1"
        );
        assert_eq!(pc_headers(Some("a=1\nb=2")).unwrap_err().kind(), "config");
        assert_eq!(
            room_api_url("1022:5"),
            "https://weibo.com/l/pc/anchor/live?live_id=1022%3A5"
        );
        assert_eq!(
            posts_api_url("123"),
            "https://weibo.com/ajax/statuses/mymblog?uid=123&page=1&feature=0"
        );
    }
}
