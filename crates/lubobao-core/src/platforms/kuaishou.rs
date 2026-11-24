//! Kuaishou adapter, following StreamCap's `KuaishouHandler` and StreamGet
//! v4.0.10 `KwaiLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, commit
//! `e57e548`), `streamget/platforms/kuaishou/live_stream.py`, MIT License,
//! Copyright (c) 2025 Hmily. The request flow, headers, page parsing and the
//! bitrate-based quality mapping follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * With a user cookie, StreamGet first asks `live_api/baseuser/userinfo/byid`
//!   whether the anchor is live and stops there when offline.
//! * Otherwise (or when live) it loads the room page `live.kuaishou.com/u/{id}`
//!   without any cookie and reads `window.__INITIAL_STATE__`:
//!   `liveroom.playList[0]` holds `liveStream`, `author` and, on failure,
//!   `errorType`.
//! * FLV variants come from `liveStream.playUrls.h264.adaptationSet.representation`.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, REFERER, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header,
    fallback_variant_index, neighbour_variant_index, select_variant_index, user_cookie,
};

/// `KwaiLiveStream._get_pc_headers` user agent, also used to pull streams.
pub const PC_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36 Edg/145.0.0.0";
const ACCEPT_LANGUAGE_VALUE: &str = "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6";
/// `_get_pc_headers` referer for page and API requests.
const PAGE_REFERER: &str = "https://live.kuaishou.com/profile/cym030000";
/// Referer sent when pulling streams.
const STREAM_REFERER: &str = "https://live.kuaishou.com/";

const ROOM_PAGE: &str = "https://live.kuaishou.com/u/";
const SHARE_PAGE: &str = "https://v.kuaishou.com/";
const USER_INFO_API: &str = "https://live.kuaishou.com/live_api/baseuser/userinfo/byid";

/// Start and end of the state script, as in StreamGet's regex
/// `<script>window.__INITIAL_STATE__=(.*?);\(function\(\)\{var s;`.
const STATE_START: &str = "<script>window.__INITIAL_STATE__=";
const STATE_END: &str = ";(function(){var s;";

/// StreamGet's `quality_mapping_bit` for the tiers StreamCap offers: the
/// chosen FLV is the best one whose bitrate does not exceed the value.
/// StreamGet's extra `BD: 4000` tier is not reachable from StreamCap.
fn max_bitrate(quality: Quality) -> u64 {
    match quality {
        Quality::Od => 99999,
        Quality::Uhd => 2000,
        Quality::Hd => 1000,
        Quality::Sd => 800,
        Quality::Ld => 600,
    }
}

/// A recognized Kuaishou room link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KuaishouLink {
    /// Path id after `/u/`: the anchor's 快手号 / custom id (`yall1102`) or
    /// encrypted id (`3x…`). Both forms open the same room.
    pub principal_id: String,
}

impl KuaishouLink {
    /// Canonical URL for the link.
    pub fn normalized_url(&self) -> String {
        format!("{ROOM_PAGE}{}", self.principal_id)
    }
}

fn is_principal_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn parse_url(input: &str) -> Option<Url> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = Url::parse(&with_scheme).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn path_segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default()
}

/// Room link in a parsed URL: `live.kuaishou.com/u/{id}` or `/profile/{id}`,
/// or the main site's `www.kuaishou.com/profile/{id}`.
fn room_in_url(url: &Url) -> Option<KuaishouLink> {
    let host = url.host_str()?.to_ascii_lowercase();
    let segments = path_segments(url);
    let id = match (host.as_str(), segments.as_slice()) {
        ("live.kuaishou.com", ["u" | "profile", id, ..]) => *id,
        ("www.kuaishou.com" | "kuaishou.com", ["profile", id]) => *id,
        _ => return None,
    };
    is_principal_id(id).then(|| KuaishouLink {
        principal_id: id.to_string(),
    })
}

/// Recognize and normalize a Kuaishou link without any network access.
///
/// StreamCap registers `https://live.kuaishou.com/` and StreamGet reads the
/// principal id after `https://live.kuaishou.com/u/`. The profile pages
/// `live.kuaishou.com/profile/{id}` and `www.kuaishou.com/profile/{id}` take
/// the same id (an intentional extension). A trailing live-stream id
/// (`/u/{id}/{liveStreamId}`) and query strings are dropped.
pub fn parse_link(input: &str) -> Result<KuaishouLink, PlatformError> {
    let trimmed = input.trim();
    parse_url(trimmed)
        .as_ref()
        .and_then(room_in_url)
        .ok_or_else(|| PlatformError::InvalidUrl(format!("不是可识别的快手直播链接: {trimmed}")))
}

/// Code of an app share link `v.kuaishou.com/{code}` (an intentional
/// extension over StreamCap; the room is only known after the redirect).
pub fn parse_share_link(input: &str) -> Option<String> {
    let url = parse_url(input)?;
    if !url.host_str()?.eq_ignore_ascii_case("v.kuaishou.com") {
        return None;
    }
    match path_segments(&url).as_slice() {
        [code] if code.chars().all(|c| c.is_ascii_alphanumeric()) => Some(code.to_string()),
        _ => None,
    }
}

/// Room behind a share link, from a redirect target: a room link or the
/// mobile share pages `…/fw/live/{id}` and `…/fw/user/{id}`.
pub fn room_from_share_target(target: &str) -> Option<KuaishouLink> {
    let url = Url::parse(target).ok()?;
    if let Some(room) = room_in_url(&url) {
        return Some(room);
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let site = ["kuaishou.com", "chenzhongtech.com", "gifshow.com"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")));
    if !site {
        return None;
    }
    match path_segments(&url).as_slice() {
        ["fw", "live" | "user", id] if is_principal_id(id) => Some(KuaishouLink {
            principal_id: id.to_string(),
        }),
        _ => None,
    }
}

/// Mobile user agent for following share links.
const SHARE_USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.0 Mobile/15E148 Safari/604.1";

/// Room state parsed from the page or the user-info API, before quality selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSnapshot {
    /// Anchor's principal id as the page reports it (`author.id`), falling
    /// back to the id in the link.
    pub room_id: String,
    /// Kuaishou numeric user id (`originUserId`), identical for every link form.
    pub anchor_id: Option<String>,
    pub live_url: Option<String>,
    pub anchor_name: String,
    pub is_live: bool,
    /// FLV variants, best first (highest bitrate first when bitrates are known).
    pub variants: Vec<StreamVariant>,
    /// Bitrate of each variant, when every representation reports one.
    /// `None` switches selection to StreamGet's padded-position rule.
    pub bitrates: Option<Vec<u64>>,
}

/// Index StreamGet picks in a best-first bitrate list: the first variant
/// whose bitrate does not exceed the tier's limit, else the last one.
pub fn bitrate_variant_index(bitrates: &[u64], quality: Quality) -> Option<usize> {
    if bitrates.is_empty() {
        return None;
    }
    let limit = max_bitrate(quality);
    Some(
        bitrates
            .iter()
            .position(|bitrate| *bitrate <= limit)
            .unwrap_or(bitrates.len() - 1),
    )
}

impl RoomSnapshot {
    /// Index of the variant for `quality`.
    pub fn select_index(&self, quality: Quality) -> Option<usize> {
        match &self.bitrates {
            Some(bitrates) => bitrate_variant_index(bitrates, quality),
            None => select_variant_index(self.variants.len(), quality),
        }
    }

    /// Index to use when the chosen variant cannot be reached: one variant
    /// down, or up from the lowest, as for Douyin.
    pub fn fallback_index(&self, quality: Quality) -> Option<usize> {
        match &self.bitrates {
            Some(bitrates) => bitrate_variant_index(bitrates, quality)
                .and_then(|index| neighbour_variant_index(self.variants.len(), index)),
            None => fallback_variant_index(self.variants.len(), quality),
        }
    }
}

/// Replace JavaScript `undefined` values (outside strings) with `null` so the
/// page state parses as JSON. The live page has e.g. `"authToken":undefined`.
fn undefined_to_null(state: &str) -> String {
    const TOKEN: &str = "undefined";
    let bytes = state.as_bytes();
    let mut out = String::with_capacity(state.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    let mut copied = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
        if bytes[index..].starts_with(TOKEN.as_bytes())
            && (index == 0 || !is_ident(bytes[index - 1]))
            && bytes
                .get(index + TOKEN.len())
                .is_none_or(|next| !is_ident(*next))
        {
            out.push_str(&state[copied..index]);
            out.push_str("null");
            index += TOKEN.len();
            copied = index;
            continue;
        }
        index += 1;
    }
    out.push_str(&state[copied..]);
    out
}

/// Classify a page that has no `__INITIAL_STATE__` script.
fn classify_page_without_state(html: &str) -> PlatformError {
    let trimmed = html.trim_start();
    if trimmed.is_empty() {
        return PlatformError::RateLimited("快手返回空页面，可能触发风控".to_string());
    }
    if html.contains("__INITIAL_STATE__") {
        return PlatformError::Parse("快手页面结构已变化，无法读取直播间数据".to_string());
    }
    let lower = html.to_ascii_lowercase();
    if lower.contains("captcha") || html.contains("验证") {
        return PlatformError::RateLimited("快手返回了验证页面，可能触发风控".to_string());
    }
    PlatformError::RateLimited("快手返回了非直播间页面，可能触发风控".to_string())
}

/// Classify `playList[0].errorType` (`{type, title, content}`).
fn classify_error_type(error: &Value) -> PlatformError {
    let text = |key: &str| error.get(key).and_then(Value::as_str).unwrap_or_default();
    let (title, content) = (text("title"), text("content"));
    let code = error.get("type").and_then(Value::as_i64);
    let detail = [title, content]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("：");
    let detail = format!("快手返回错误 {detail}");
    let all = format!("{title}{content}");
    if all.contains("登录") {
        return PlatformError::AuthRequired(detail);
    }
    if all.contains("验证") || all.contains("频繁") || all.contains("风控") {
        return PlatformError::RateLimited(detail);
    }
    if all.contains("不存在") || all.contains("封禁") || all.contains("注销") {
        return PlatformError::InvalidUrl(detail);
    }
    match code {
        // "错误代码22 / 浏览其他内容": observed for ids that do not exist, but
        // the page gives no reason, so it stays retryable instead of marking
        // the link as dead.
        Some(22) => {
            PlatformError::RateLimited(format!("{detail}（主播可能不存在，也可能是请求被限制）"))
        }
        _ => PlatformError::RateLimited(detail),
    }
}

/// `originUserId` as a string; the page uses a number.
fn origin_user_id(author: &Value) -> Option<String> {
    match author.get("originUserId")? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// Path stem of a stream URL (`…/gifshow/{stem}.flv?…` → `stem`), used to
/// pair the single HLS URL with its FLV variant.
fn stream_stem(url: &str) -> Option<&str> {
    let path = url.split(['?', '#']).next()?;
    let file = path.rsplit('/').next()?;
    file.rsplit_once('.').map(|(stem, _)| stem)
}

/// The `representation` list StreamGet reads: `playUrls.h264` when present,
/// otherwise the first entry of a list-shaped `playUrls`. `Ok(None)` means the
/// room is not live.
fn representations(play_urls: &Value) -> Result<Option<&Vec<Value>>, PlatformError> {
    let set = match play_urls {
        Value::Null => return Ok(None),
        Value::Object(map) if map.is_empty() => return Ok(None),
        Value::Array(list) if list.is_empty() => return Ok(None),
        Value::Object(map) => match map.get("h264") {
            Some(h264) => h264,
            None => {
                return Err(PlatformError::Parse(
                    "直播中但没有找到 H.264 流".to_string(),
                ));
            }
        },
        Value::Array(list) => &list[0],
        _ => return Err(PlatformError::Parse("playUrls 格式无法识别".to_string())),
    };
    let Some(adaptation) = set.get("adaptationSet") else {
        // StreamGet: `h264` without `adaptationSet` means not live.
        return Ok(None);
    };
    adaptation
        .get("representation")
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
        .map(Some)
        .ok_or_else(|| PlatformError::Parse("直播中但没有找到可用的流地址".to_string()))
}

/// Build best-first variants from `representation` entries.
fn build_variants(
    entries: &[Value],
    hls_url: Option<&str>,
) -> Result<(Vec<StreamVariant>, Option<Vec<u64>>), PlatformError> {
    struct Entry {
        name: String,
        url: String,
        bitrate: Option<u64>,
    }
    // Entries without a URL are skipped; the room fails only when none is usable.
    let mut parsed: Vec<Entry> = entries
        .iter()
        .filter_map(|entry| {
            let url = entry
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())?;
            let name = entry
                .get("qualityType")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(Entry {
                name: name.to_string(),
                url: url.to_string(),
                bitrate: entry.get("bitrate").and_then(Value::as_u64),
            })
        })
        .collect();
    if parsed.is_empty() {
        return Err(PlatformError::Parse(
            "直播中但所有档位都缺少流地址".to_string(),
        ));
    }

    // StreamGet checks `'bitrate' in flv_url_list[0]`; requiring it on every
    // entry keeps the sort well-defined.
    let bitrates = if parsed.iter().all(|entry| entry.bitrate.is_some()) {
        parsed.sort_by_key(|entry| std::cmp::Reverse(entry.bitrate));
        Some(
            parsed
                .iter()
                .map(|entry| entry.bitrate.unwrap_or_default())
                .collect::<Vec<_>>(),
        )
    } else {
        // Without bitrates StreamGet reverses the (worst-first) list.
        parsed.reverse();
        None
    };

    let hls_stem = hls_url.and_then(stream_stem);
    let mut variants: Vec<StreamVariant> = parsed
        .iter()
        .map(|entry| StreamVariant {
            name: entry.name.clone(),
            quality: None,
            flv_url: Some(entry.url.clone()),
            hls_url: hls_url
                .filter(|_| hls_stem.is_some() && stream_stem(&entry.url) == hls_stem)
                .map(str::to_string),
        })
        .collect();

    // Label each variant with the best tier that selects it.
    for quality in Quality::ALL {
        let index = match &bitrates {
            Some(bitrates) => bitrate_variant_index(bitrates, quality),
            None => (quality.index() < variants.len()).then_some(quality.index()),
        };
        if let Some(variant) = index.and_then(|index| variants.get_mut(index))
            && variant.quality.is_none()
        {
            variant.quality = Some(quality);
        }
    }
    Ok((variants, bitrates))
}

/// `liveroom.playList[0]` from the page state. The whole state is tried
/// first; when an unrelated module holds something JSON cannot express
/// (`NaN`, `new Date(…)`, a regex), only the first `playList` entry is read,
/// and as a last resort StreamGet's `{"liveStream"…` to `,"gameInfo"` slice.
fn play_list_entry(state: &str) -> Result<Value, PlatformError> {
    let whole_error = match serde_json::from_str::<Value>(state) {
        Ok(value) => {
            return value
                .pointer("/liveroom/playList/0")
                .cloned()
                .ok_or_else(|| PlatformError::Parse("快手页面数据缺少 playList".to_string()));
        }
        Err(error) => error,
    };

    const PLAY_LIST: &str = "\"playList\":[";
    if let Some(start) = state.find(PLAY_LIST)
        && let Some(Ok(entry)) =
            serde_json::Deserializer::from_str(&state[start + PLAY_LIST.len()..])
                .into_iter::<Value>()
                .next()
        && entry.is_object()
    {
        return Ok(entry);
    }

    const LIVE_STREAM: &str = "{\"liveStream\"";
    const GAME_INFO: &str = ",\"gameInfo\"";
    if let Some(start) = state.find(LIVE_STREAM)
        && let Some(end) = state[start..].find(GAME_INFO)
        && let Ok(entry) =
            serde_json::from_str::<Value>(&format!("{}}}", &state[start..start + end]))
    {
        return Ok(entry);
    }

    Err(PlatformError::Parse(format!(
        "快手页面数据无法解析: {whole_error}"
    )))
}

/// Parse a room page (`live.kuaishou.com/u/{id}`).
pub fn parse_room_page(html: &str, principal_id: &str) -> Result<RoomSnapshot, PlatformError> {
    let state = html
        .find(STATE_START)
        .map(|start| &html[start + STATE_START.len()..])
        .and_then(|rest| rest.find(STATE_END).map(|end| &rest[..end]))
        .ok_or_else(|| classify_page_without_state(html))?;
    let entry = play_list_entry(&undefined_to_null(state))?;
    let entry = &entry;

    if let Some(error) = entry.get("errorType").filter(|error| error.is_object()) {
        return Err(classify_error_type(error));
    }

    let author = entry.get("author").unwrap_or(&Value::Null);
    let banned = |key: &str| {
        author
            .pointer(&format!("/bannedStatus/{key}"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    if banned("banned") {
        return Err(PlatformError::InvalidUrl(
            "该主播因违反社区规定，账号封禁".to_string(),
        ));
    }
    if banned("isolate") {
        return Err(PlatformError::InvalidUrl(
            "该链接指向页面不存在".to_string(),
        ));
    }

    let live_stream = entry
        .get("liveStream")
        .filter(|stream| stream.as_object().is_some_and(|map| !map.is_empty()));
    let Some(live_stream) = live_stream else {
        // StreamGet: "IP banned. Please change device or network." The page
        // looks the same for ids that do not exist, so this stays retryable.
        return Err(PlatformError::RateLimited(
            "快手没有返回直播间数据，可能是 IP 被限制，也可能是主播不存在".to_string(),
        ));
    };

    let anchor_name = author
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| PlatformError::Parse("页面数据缺少主播昵称".to_string()))?
        .to_string();
    let room_id = author
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or(principal_id)
        .to_string();

    let (variants, bitrates) = match live_stream
        .get("playUrls")
        .map(representations)
        .transpose()?
        .flatten()
    {
        Some(entries) => {
            let hls_url = live_stream
                .get("hlsPlayUrl")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty());
            build_variants(entries, hls_url)?
        }
        None => (Vec::new(), None),
    };

    Ok(RoomSnapshot {
        live_url: Some(format!("{ROOM_PAGE}{room_id}")),
        room_id,
        anchor_id: origin_user_id(author),
        anchor_name,
        is_live: !variants.is_empty(),
        variants,
        bitrates,
    })
}

/// Result of StreamGet's cookie-only `get_user_info` check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInfoCheck {
    /// The anchor is offline; StreamGet returns without loading the page.
    Offline(RoomSnapshot),
    /// The anchor is live; load the room page for streams.
    Live,
    /// The API gave no usable answer (e.g. a captcha challenge); load the page.
    Unavailable(String),
}

/// Parse a `userinfo/byid` response.
pub fn parse_user_info(body: &str, principal_id: &str) -> Result<UserInfoCheck, PlatformError> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Ok(UserInfoCheck::Unavailable(
            "用户信息接口返回的不是 JSON".to_string(),
        ));
    };
    let data = json.get("data").unwrap_or(&Value::Null);
    let result = data.get("result").and_then(Value::as_i64);
    let user_info = data.get("userInfo").filter(|info| info.is_object());
    let name = user_info
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
    if result == Some(2) && name.is_some() {
        // StreamGet: "The account is abnormal, triggering risk control".
        return Err(PlatformError::RateLimited(
            "快手提示账号异常，触发风控，请更换 Cookie 或稍后重试".to_string(),
        ));
    }
    let (Some(user_info), Some(name)) = (user_info, name) else {
        return Ok(UserInfoCheck::Unavailable(format!(
            "用户信息接口结果 {}",
            result.map_or_else(|| "未知".to_string(), |code| code.to_string())
        )));
    };
    match user_info.get("living").and_then(Value::as_bool) {
        Some(true) => Ok(UserInfoCheck::Live),
        Some(false) => {
            let room_id = user_info
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .unwrap_or(principal_id)
                .to_string();
            Ok(UserInfoCheck::Offline(RoomSnapshot {
                live_url: Some(format!("{ROOM_PAGE}{room_id}")),
                room_id,
                anchor_id: origin_user_id(user_info),
                anchor_name: name.to_string(),
                is_live: false,
                variants: Vec::new(),
                bitrates: None,
            }))
        }
        None => Ok(UserInfoCheck::Unavailable(
            "用户信息接口缺少开播状态".to_string(),
        )),
    }
}

/// Turn a snapshot into `LiveInfo`, choosing the stream for `quality`.
/// `primary_reachable` is the result of probing the chosen FLV URL; when it
/// failed, the neighbouring variant is used instead.
pub fn build_live_info(
    snapshot: RoomSnapshot,
    quality: Quality,
    primary_reachable: bool,
) -> LiveInfo {
    let index = if primary_reachable {
        snapshot.select_index(quality)
    } else {
        snapshot.fallback_index(quality)
    };
    let selected = index
        .and_then(|index| snapshot.variants.get(index))
        .filter(|_| snapshot.is_live)
        .and_then(|variant| {
            // StreamGet: `record_url = flv_url`.
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
        platform: Platform::Kuaishou,
        room_id: snapshot.room_id,
        anchor_id: snapshot.anchor_id,
        live_url: snapshot.live_url,
        anchor_name: snapshot.anchor_name,
        title: None,
        is_live: snapshot.is_live && selected.is_some(),
        variants: snapshot.variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Kuaishou streams. The CDN needs none; the PC
/// User-Agent and live Referer match a browser. User cookies are never
/// forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), PC_USER_AGENT.to_string()),
        ("Referer".to_string(), STREAM_REFERER.to_string()),
    ])
}

/// `KwaiLiveStream.pc_headers`, used for the room page. StreamGet does not
/// send the user cookie with the page request.
pub fn page_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(PC_USER_AGENT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
    );
    headers.insert(REFERER, HeaderValue::from_static(PAGE_REFERER));
    headers
}

/// Kuaishou adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct KuaishouAdapter {
    clients: ClientCache,
}

impl KuaishouAdapter {
    pub fn new() -> Self {
        KuaishouAdapter::default()
    }

    async fn get_text(
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

    async fn check_user_info(
        client: &reqwest::Client,
        principal_id: &str,
        cookie: &str,
    ) -> Result<UserInfoCheck, PlatformError> {
        let mut api = Url::parse(USER_INFO_API).expect("valid user info API URL");
        api.query_pairs_mut()
            .append_pair("__NS_hxfalcon", "")
            .append_pair("caver", "2")
            .append_pair("principalId", principal_id);
        let mut headers = page_headers();
        headers.insert(COOKIE, cookie_header(cookie)?);
        match Self::get_text(client, api.as_str(), headers).await {
            Ok(body) => parse_user_info(&body, principal_id),
            // The page needs no cookie, so a failing API is not fatal.
            Err(error) => Ok(UserInfoCheck::Unavailable(error.to_string())),
        }
    }

    /// HEAD probe on the chosen stream, counting only transport failures.
    async fn probe(client: &reqwest::Client, url: &str) -> bool {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(PC_USER_AGENT));
        headers.insert(REFERER, HeaderValue::from_static(STREAM_REFERER));
        client
            .head(url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .is_ok()
    }

    /// Room behind a `v.kuaishou.com` share link.
    async fn resolve_share_link(
        code: &str,
        options: &FetchOptions,
    ) -> Result<KuaishouLink, PlatformError> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(SHARE_USER_AGENT));
        let target = super::follow_short_link(
            options.proxy.as_deref(),
            &format!("{SHARE_PAGE}{code}"),
            headers,
            |url| room_from_share_target(url.as_str()).is_some(),
        )
        .await?;
        room_from_share_target(&target).ok_or_else(|| {
            PlatformError::InvalidUrl(format!(
                "快手分享链接没有指向直播间或主播主页，请检查链接是否失效: {target}"
            ))
        })
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = match parse_share_link(url) {
            Some(code) => Self::resolve_share_link(&code, options).await?,
            None => parse_link(url)?,
        };
        let client = self.clients.get(options.proxy.as_deref())?;

        if let Some(cookie) = user_cookie(options)
            && let UserInfoCheck::Offline(snapshot) =
                Self::check_user_info(&client, &link.principal_id, cookie).await?
        {
            return Ok(build_live_info(snapshot, options.quality, true));
        }

        let html = Self::get_text(&client, &link.normalized_url(), page_headers()).await?;
        let snapshot = parse_room_page(&html, &link.principal_id)?;

        let mut reachable = true;
        if snapshot.is_live
            && let Some(flv_url) = snapshot
                .select_index(options.quality)
                .and_then(|index| snapshot.variants.get(index))
                .and_then(|variant| variant.flv_url.clone())
        {
            reachable = Self::probe(&client, &flv_url).await;
        }
        Ok(build_live_info(snapshot, options.quality, reachable))
    }
}

impl PlatformAdapter for KuaishouAdapter {
    fn platform(&self) -> Platform {
        Platform::Kuaishou
    }

    fn matches(&self, url: &str) -> bool {
        parse_link(url).is_ok() || parse_share_link(url).is_some()
    }

    fn normalize_url(&self, url: &str) -> Option<String> {
        if let Some(code) = parse_share_link(url) {
            return Some(format!("{SHARE_PAGE}{code}"));
        }
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

    const ROOM_LIVE: &str = include_str!("../../tests/fixtures/kuaishou/room_live.html");
    const ROOM_OFFLINE: &str = include_str!("../../tests/fixtures/kuaishou/room_offline.html");
    const ROOM_ERROR_22: &str = include_str!("../../tests/fixtures/kuaishou/room_error_22.html");
    const ROOM_EMPTY: &str =
        include_str!("../../tests/fixtures/kuaishou/room_empty_live_stream.html");
    const USERINFO_OFFLINE: &str =
        include_str!("../../tests/fixtures/kuaishou/userinfo_offline.json");
    const USERINFO_LIVE: &str = include_str!("../../tests/fixtures/kuaishou/userinfo_live.json");
    const USERINFO_RISK: &str = include_str!("../../tests/fixtures/kuaishou/userinfo_risk.json");
    const USERINFO_CAPTCHA: &str =
        include_str!("../../tests/fixtures/kuaishou/userinfo_captcha.json");

    /// Wrap one `playList` entry in a minimal room page.
    fn page_with_entry(entry: &str) -> String {
        format!(
            "<html><body><script>window.__INITIAL_STATE__={{\"liveroom\":{{\"playList\":[{entry}]}}}}{STATE_END}(s=document.currentScript).parentNode.removeChild(s);}}());</script></body></html>"
        )
    }

    fn names(snapshot: &RoomSnapshot) -> Vec<&str> {
        snapshot
            .variants
            .iter()
            .map(|variant| variant.name.as_str())
            .collect()
    }

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let cases = [
            ("https://live.kuaishou.com/u/yall1102", "yall1102"),
            ("  live.kuaishou.com/u/yall1102?from=share ", "yall1102"),
            (
                "http://live.kuaishou.com/u/3xxy7fxvxdex7ua/",
                "3xxy7fxvxdex7ua",
            ),
            (
                "https://live.kuaishou.com/u/beimu0315/5klZWxw7sNY",
                "beimu0315",
            ),
            ("https://LIVE.kuaishou.com/u/a_b-c.d#top", "a_b-c.d"),
            ("https://live.kuaishou.com/profile/yall1102", "yall1102"),
            (
                "https://www.kuaishou.com/profile/3xxy7fxvxdex7ua?source=x",
                "3xxy7fxvxdex7ua",
            ),
        ];
        let adapter = KuaishouAdapter::new();
        for (input, id) in cases {
            let link = parse_link(input).unwrap();
            assert_eq!(link.principal_id, id, "{input}");
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some(format!("https://live.kuaishou.com/u/{id}").as_str()),
                "{input}"
            );
            assert!(adapter.matches(input), "{input}");
        }
    }

    #[test]
    fn share_links_are_recognized_offline_and_resolved_from_the_redirect() {
        let adapter = KuaishouAdapter::new();
        assert_eq!(
            parse_share_link("https://v.kuaishou.com/mX4VwG").as_deref(),
            Some("mX4VwG")
        );
        assert!(adapter.matches("v.kuaishou.com/mX4VwG/"));
        assert_eq!(
            adapter
                .normalize_url("http://v.kuaishou.com/mX4VwG?x=1")
                .as_deref(),
            Some("https://v.kuaishou.com/mX4VwG")
        );
        let cases = [
            (
                "https://c.kuaishou.com/fw/user/mi2018520?fid=1&cc=share_copylink",
                Some("mi2018520"),
            ),
            (
                "https://v.m.chenzhongtech.com/fw/live/3xxy7fxvxdex7ua?cc=share_copylink",
                Some("3xxy7fxvxdex7ua"),
            ),
            ("https://live.kuaishou.com/u/yall1102", Some("yall1102")),
            ("https://c.kuaishou.com/fw/photo/3xabc", None),
            ("https://www.example.com/fw/live/yall1102", None),
        ];
        for (target, id) in cases {
            assert_eq!(
                room_from_share_target(target).map(|link| link.principal_id),
                id.map(str::to_string),
                "{target}"
            );
        }
    }

    #[test]
    fn rejects_other_sites_and_non_room_links() {
        let adapter = KuaishouAdapter::new();
        for input in [
            "https://live.kuaishou.com/",
            "https://live.kuaishou.com/u/",
            "https://www.kuaishou.com/u/yall1102",
            "https://www.kuaishou.com/profile/",
            "https://v.kuaishou.com/",
            "https://v.kuaishou.com/a/b",
            "https://live.kuaishou.com/u/bad%20id",
            "ftp://live.kuaishou.com/u/yall1102",
            "https://live.douyin.com/yall1102",
            "",
        ] {
            assert_eq!(
                parse_link(input).unwrap_err().kind(),
                "invalid_url",
                "{input}"
            );
            assert!(!adapter.matches(input), "{input}");
        }
        let kuaishou = super::super::adapter_for("https://live.kuaishou.com/u/yall1102").unwrap();
        assert_eq!(kuaishou.platform(), Platform::Kuaishou);
        let douyin = super::super::adapter_for("https://live.douyin.com/yall1102").unwrap();
        assert_eq!(douyin.platform(), Platform::Douyin);
    }

    #[test]
    fn live_room_lists_h264_flv_by_bitrate_and_pairs_the_hls_url() {
        let snapshot = parse_room_page(ROOM_LIVE, "test_anchor").unwrap();
        assert!(snapshot.is_live);
        assert_eq!(snapshot.anchor_name, "测试主播");
        assert_eq!(snapshot.room_id, "test_anchor");
        assert_eq!(snapshot.anchor_id.as_deref(), Some("100000001"));
        assert_eq!(
            snapshot.live_url.as_deref(),
            Some("https://live.kuaishou.com/u/test_anchor")
        );
        assert_eq!(names(&snapshot), ["BLUE_RAY", "SUPER", "HIGH", "STANDARD"]);
        assert_eq!(
            snapshot.bitrates.as_deref(),
            Some(&[8000, 4000, 2000, 1000][..])
        );
        for variant in &snapshot.variants {
            let flv = variant.flv_url.as_deref().unwrap();
            assert!(
                flv.starts_with("https://hw-origin.pull.yximgs.com/gifshow/testStream01_GameAvc")
            );
            assert!(!flv.contains("Hevc"));
        }
        // The page's single HLS URL belongs to the 1000 kbps (SdL0) stream.
        let with_hls: Vec<&str> = snapshot
            .variants
            .iter()
            .filter(|variant| variant.hls_url.is_some())
            .map(|variant| variant.name.as_str())
            .collect();
        assert_eq!(with_hls, ["STANDARD"]);
        assert_eq!(
            snapshot
                .variants
                .iter()
                .map(|v| v.quality)
                .collect::<Vec<_>>(),
            [
                Some(Quality::Od),
                None,
                Some(Quality::Uhd),
                Some(Quality::Hd)
            ]
        );
    }

    #[test]
    fn quality_follows_streamget_bitrate_mapping_and_records_flv() {
        let snapshot = parse_room_page(ROOM_LIVE, "test_anchor").unwrap();
        let expected = [
            (Quality::Od, "BLUE_RAY"),
            (Quality::Uhd, "HIGH"),
            (Quality::Hd, "STANDARD"),
            (Quality::Sd, "STANDARD"),
            (Quality::Ld, "STANDARD"),
        ];
        for (quality, name) in expected {
            let info = build_live_info(snapshot.clone(), quality, true);
            assert!(info.is_live);
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert_eq!(selected.variant.name, name, "{quality}");
            assert_eq!(
                Some(&selected.record_url),
                selected.variant.flv_url.as_ref()
            );
            // FLV even when HLS is preferred: record_url is the FLV stream.
            assert_eq!(
                selected.source_url(false, true).0,
                super::super::SourceFormat::Flv
            );
        }
    }

    #[test]
    fn unreachable_stream_falls_back_one_variant_down_or_up_from_the_lowest() {
        let snapshot = parse_room_page(ROOM_LIVE, "test_anchor").unwrap();
        let expected = [
            (Quality::Od, "SUPER"),
            (Quality::Uhd, "STANDARD"),
            (Quality::Hd, "HIGH"),
            (Quality::Ld, "HIGH"),
        ];
        for (quality, name) in expected {
            let selected = build_live_info(snapshot.clone(), quality, false)
                .selected
                .unwrap();
            assert_eq!(selected.variant.name, name, "{quality}");
        }
    }

    #[test]
    fn representations_without_bitrate_use_reversed_padded_positions() {
        let entry = r#"{"liveStream":{"playUrls":[{"adaptationSet":{"representation":[
            {"url":"https://cdn.example/low.flv","name":"标清"},
            {"url":"https://cdn.example/mid.flv","name":"高清"},
            {"url":"https://cdn.example/high.flv","name":"超清"}]}}]},
            "author":{"id":"a","name":"主播","originUserId":"42"}}"#;
        let snapshot = parse_room_page(&page_with_entry(entry), "a").unwrap();
        assert!(snapshot.is_live);
        assert_eq!(snapshot.bitrates, None);
        assert_eq!(names(&snapshot), ["超清", "高清", "标清"]);
        assert_eq!(snapshot.anchor_id.as_deref(), Some("42"));
        let pick = |quality, reachable| {
            build_live_info(snapshot.clone(), quality, reachable)
                .selected
                .unwrap()
                .variant
                .name
        };
        assert_eq!(pick(Quality::Od, true), "超清");
        assert_eq!(pick(Quality::Hd, true), "标清");
        assert_eq!(pick(Quality::Ld, true), "标清");
        assert_eq!(pick(Quality::Od, false), "高清");
    }

    #[test]
    fn offline_room_has_anchor_but_no_streams() {
        let snapshot = parse_room_page(ROOM_OFFLINE, "test_anchor").unwrap();
        assert!(!snapshot.is_live);
        assert_eq!(snapshot.anchor_name, "测试主播");
        assert_eq!(snapshot.anchor_id.as_deref(), Some("100000001"));
        let info = build_live_info(snapshot, Quality::Od, true);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        assert_eq!(info.platform, Platform::Kuaishou);

        // A missing or empty `playUrls` is also offline.
        for entry in [
            r#"{"liveStream":{"type":"live"},"author":{"id":"a","name":"主播"}}"#,
            r#"{"liveStream":{"playUrls":[]},"author":{"id":"a","name":"主播"}}"#,
        ] {
            assert!(
                !parse_room_page(&page_with_entry(entry), "a")
                    .unwrap()
                    .is_live
            );
        }
    }

    #[test]
    fn page_errors_map_to_error_kinds() {
        let entry = |error: &str| {
            page_with_entry(&format!(
                r#"{{"liveStream":{{}},"author":{{}},"errorType":{error}}}"#
            ))
        };
        let cases = [
            (ROOM_ERROR_22.to_string(), "rate_limited"),
            (ROOM_EMPTY.to_string(), "rate_limited"),
            (
                entry(r#"{"type":1,"title":"请先登录","content":"登录后查看"}"#),
                "auth_required",
            ),
            (
                entry(r#"{"type":2,"title":"操作太频繁","content":"请稍后再试"}"#),
                "rate_limited",
            ),
            (
                entry(r#"{"type":3,"title":"用户不存在","content":""}"#),
                "invalid_url",
            ),
            (
                page_with_entry(
                    r#"{"liveStream":{"playUrls":{}},"author":{"id":"a","name":"主播","bannedStatus":{"banned":true}}}"#,
                ),
                "invalid_url",
            ),
            (
                "<html><div id=\"captcha\">请完成安全验证</div></html>".to_string(),
                "rate_limited",
            ),
            ("   ".to_string(), "rate_limited"),
            ("<html><body>欢迎</body></html>".to_string(), "rate_limited"),
            (
                "<script>window.__INITIAL_STATE__={\"liveroom\":".to_string(),
                "parse",
            ),
            (page_with_entry("{\"liveStream\":"), "parse"),
            (
                page_with_entry(r#"{"liveStream":{"playUrls":{"hevc":{}}},"author":{"name":"x"}}"#),
                "parse",
            ),
        ];
        for (html, kind) in cases {
            let error = parse_room_page(&html, "a").unwrap_err();
            assert_eq!(error.kind(), kind, "{html}: {error}");
        }
        let error = parse_room_page(ROOM_ERROR_22, "a").unwrap_err();
        assert!(error.message().contains("错误代码22"), "{error}");
    }

    #[test]
    fn user_info_api_reports_offline_live_risk_and_captcha() {
        match parse_user_info(USERINFO_OFFLINE, "test_anchor").unwrap() {
            UserInfoCheck::Offline(snapshot) => {
                assert_eq!(snapshot.anchor_name, "测试主播");
                assert_eq!(snapshot.anchor_id.as_deref(), Some("100000001"));
                assert_eq!(snapshot.room_id, "test_anchor");
                assert!(!snapshot.is_live);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            parse_user_info(USERINFO_LIVE, "test_anchor").unwrap(),
            UserInfoCheck::Live
        );
        assert_eq!(
            parse_user_info(USERINFO_RISK, "test_anchor")
                .unwrap_err()
                .kind(),
            "rate_limited"
        );
        assert!(matches!(
            parse_user_info(USERINFO_CAPTCHA, "test_anchor").unwrap(),
            UserInfoCheck::Unavailable(_)
        ));
        assert!(matches!(
            parse_user_info("<html>", "test_anchor").unwrap(),
            UserInfoCheck::Unavailable(_)
        ));
    }

    #[test]
    fn stream_and_page_headers_never_carry_cookies() {
        let snapshot = parse_room_page(ROOM_LIVE, "test_anchor").unwrap();
        let info = build_live_info(snapshot, Quality::Od, true);
        assert!(
            info.stream_headers
                .keys()
                .all(|key| !key.eq_ignore_ascii_case("cookie"))
        );
        assert_eq!(
            info.stream_headers.get("Referer").map(String::as_str),
            Some("https://live.kuaishou.com/")
        );
        assert_eq!(
            info.stream_headers.get("User-Agent").map(String::as_str),
            Some(PC_USER_AGENT)
        );
        assert!(page_headers().get(COOKIE).is_none());
    }

    #[test]
    fn undefined_values_become_null_but_strings_are_kept() {
        assert_eq!(
            undefined_to_null(
                r#"{"a":undefined,"b":[undefined],"c":"x/undefined","d":"\"undefined"}"#
            ),
            r#"{"a":null,"b":[null],"c":"x/undefined","d":"\"undefined"}"#
        );
        assert_eq!(
            undefined_to_null(r#"{"undefinedKey":1}"#),
            r#"{"undefinedKey":1}"#
        );
    }

    #[test]
    fn non_ascii_outside_strings_does_not_panic() {
        assert_eq!(
            undefined_to_null("{\"a\":'主播',b:undefined} // 注释 undefined"),
            "{\"a\":'主播',b:null} // 注释 null"
        );
        // Only ASCII identifier bytes join a token; this must not panic.
        assert_eq!(undefined_to_null("主undefined"), "主null");
        assert_eq!(undefined_to_null("：undefined"), "：null");
        // A page whose state has a single-quoted string and a comment outside
        // the room entry still parses, without panicking.
        let html = ROOM_LIVE.replacen(
            "{\"user\":",
            "{\"misc\":{\"tip\":'快手主播'/* 注释 */},\"user\":",
            1,
        );
        let snapshot = parse_room_page(&html, "test_anchor").unwrap();
        assert!(snapshot.is_live);
        assert_eq!(snapshot.anchor_name, "测试主播");
    }

    #[test]
    fn non_json_values_in_other_modules_fall_back_to_the_room_entry() {
        let html = ROOM_LIVE.replacen(
            "{\"user\":",
            "{\"misc\":{\"n\":NaN,\"i\":Infinity,\"d\":new Date(1),\"r\":/a+b/g},\"user\":",
            1,
        );
        let snapshot = parse_room_page(&html, "test_anchor").unwrap();
        assert!(snapshot.is_live);
        assert_eq!(snapshot.variants.len(), 4);
        assert_eq!(snapshot.anchor_id.as_deref(), Some("100000001"));

        // Offline page and error page through the same fallback.
        let offline = ROOM_OFFLINE.replacen("{\"user\":", "{\"misc\":NaN,\"user\":", 1);
        assert!(!parse_room_page(&offline, "test_anchor").unwrap().is_live);
        let error = ROOM_ERROR_22.replacen("{\"user\":", "{\"misc\":NaN,\"user\":", 1);
        assert_eq!(
            parse_room_page(&error, "a").unwrap_err().kind(),
            "rate_limited"
        );

        // Bad values inside the entry after `gameInfo`: StreamGet's slice.
        let entry = r#"{"liveStream":{"playUrls":{"h264":{"adaptationSet":{"representation":[
            {"bitrate":2000,"qualityType":"HIGH","url":"https://cdn.example/hd.flv"}]}}}},
            "author":{"id":"a","name":"主播"},"gameInfo":{},"config":{"t":NaN}}"#;
        let snapshot = parse_room_page(&page_with_entry(entry), "a").unwrap();
        assert!(snapshot.is_live);
        assert_eq!(names(&snapshot), ["HIGH"]);
    }

    #[test]
    fn representations_without_url_are_skipped() {
        let entry = r#"{"liveStream":{"playUrls":{"h264":{"adaptationSet":{"representation":[
            {"bitrate":8000,"qualityType":"BLUE_RAY","url":""},
            {"bitrate":4000,"qualityType":"SUPER"},
            {"bitrate":2000,"qualityType":"HIGH","url":"https://cdn.example/hd.flv"},
            {"bitrate":1000,"qualityType":"STANDARD","url":"https://cdn.example/sd.flv"}]}}}},
            "author":{"id":"a","name":"主播"}}"#;
        let snapshot = parse_room_page(&page_with_entry(entry), "a").unwrap();
        assert_eq!(names(&snapshot), ["HIGH", "STANDARD"]);
        let od = build_live_info(snapshot, Quality::Od, true)
            .selected
            .unwrap();
        assert_eq!(od.variant.name, "HIGH");

        let none = r#"{"liveStream":{"playUrls":{"h264":{"adaptationSet":{"representation":[
            {"bitrate":2000,"qualityType":"HIGH"}]}}}},"author":{"id":"a","name":"主播"}}"#;
        assert_eq!(
            parse_room_page(&page_with_entry(none), "a")
                .unwrap_err()
                .kind(),
            "parse"
        );
    }

    #[test]
    fn cookie_with_line_breaks_is_a_config_error() {
        assert_eq!(cookie_header("a=1\r\nb=2").unwrap_err().kind(), "config");
    }
}
