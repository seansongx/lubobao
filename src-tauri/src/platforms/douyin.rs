//! Douyin adapter, following StreamCap's `DouyinHandler` and StreamGet
//! v4.0.10 `DouyinLiveStream` / `DouyinUtils`.
//!
//! Routing (StreamCap `handlers.py`): `v.douyin.com` and
//! `www.douyin.com/user` links use the app flow, everything else the web
//! flow.
//!
//! * Web flow: `live.douyin.com/{web_rid or 抖音号}` → `webcast/room/web/enter`.
//! * App flow: resolve the link; a `reflow/{room_id}?sec_user_id=` redirect
//!   goes to `webcast.amemv.com/webcast/room/reflow/info`, any other target is
//!   treated as a user profile whose `unique_id` (抖音号) feeds the web flow.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, REFERER, USER_AGENT};
use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::Value;

use super::sign::ab_sign;
use super::{
    BoxFuture, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError, Quality,
    StreamVariant, build_client, fallback_variant_index, select_variant_index, selected_stream,
};

const ACCEPT_LANGUAGE_VALUE: &str = "zh-CN,zh;q=0.8,zh-TW;q=0.7,zh-HK;q=0.5,en-US;q=0.3,en;q=0.2";

/// `DouyinLiveStream._get_pc_headers` user agent, also used to pull streams.
pub const PC_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
const PC_DEFAULT_COOKIE: &str = "__ac_nonce=064caded4009deafd8b89";
const LIVE_REFERER: &str = "https://live.douyin.com/";

/// `_get_web_stream_data` headers.
const WEB_API_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36";
const WEB_API_REFERER: &str = "https://live.douyin.com/335354047186";
const WEB_API_DEFAULT_COOKIE: &str = "ttwid=1%7CmDcInbJ7AJ-2PGtsgrG4xj7SOiNMzePqQBF1LMO2Qkg%7C1761107324%7Cbbf97c2cd9f8eae8e8c36db4ef50c323deaa4b161179170aaf659590867c162d";

/// `BaseLiveStream._get_mobile_headers` / `DouyinUtils.HEADERS` user agent.
const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 11; SAMSUNG SM-G973U) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/14.2 Chrome/87.0.4280.141 Mobile Safari/537.36";
/// `DouyinUtils.HEADERS` cookie used when resolving share links.
const RESOLVE_COOKIE: &str = "s_v_web_id=verify_lk07kv74_QZYCUApD_xhiB_405x_Ax51_GYO9bUIyZQVf";
/// `DouyinUtils.get_unique_id` cookie for the iesdouyin profile page.
const PROFILE_COOKIE: &str = "ttwid=1%7C4ejCkU2bKY76IySQENJwvGhg1IQZrgGEupSyTKKfuyk%7C1740470403%7Cbc9ad2ee341f1a162f9e27f4641778030d1ae91e31f9df6553a8f2efa3bdb7b4; __ac_nonce=0683e59f3009cc48fbab0; __ac_signature=_02B4Z6wo00f01mG6waQAAIDB9JUCzFb6.TZhmsUAAPBf34; __ac_referer=__ac_blank";

const WEB_ENTER_API: &str = "https://live.douyin.com/webcast/room/web/enter/";
const APP_REFLOW_API: &str = "https://webcast.amemv.com/webcast/room/reflow/info/";
const PROFILE_PAGE: &str = "https://www.iesdouyin.com/share/user/";

/// Douyin room status `2` means live; `4` means ended / not live.
const STATUS_LIVE: i64 = 2;

/// The five link shapes listed in StreamCap's README.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DouyinLink {
    /// `https://live.douyin.com/745964462470`
    WebRoom { web_rid: String },
    /// `https://live.douyin.com/yall1102` (live.douyin.com + 抖音号)
    DouyinId { id: String },
    /// `https://v.douyin.com/iQFeBnt/` — app share link to a room or a profile;
    /// which one is only known after following the redirect.
    ShortLink { code: String },
    /// `https://www.douyin.com/user/MS4wLjABAAAA...`
    WebProfile { sec_uid: String },
}

impl DouyinLink {
    /// Canonical URL for the link.
    pub fn normalized_url(&self) -> String {
        match self {
            DouyinLink::WebRoom { web_rid } => format!("https://live.douyin.com/{web_rid}"),
            DouyinLink::DouyinId { id } => format!("https://live.douyin.com/{id}"),
            DouyinLink::ShortLink { code } => format!("https://v.douyin.com/{code}/"),
            DouyinLink::WebProfile { sec_uid } => format!("https://www.douyin.com/user/{sec_uid}"),
        }
    }

    /// StreamCap sends short links and web profiles through `fetch_app_stream_data`.
    pub fn uses_app_flow(&self) -> bool {
        matches!(
            self,
            DouyinLink::ShortLink { .. } | DouyinLink::WebProfile { .. }
        )
    }
}

/// Recognize and normalize a Douyin link without any network access.
pub fn parse_link(input: &str) -> Result<DouyinLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的抖音直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();

    match host.as_str() {
        "live.douyin.com" => {
            // StreamGet takes the last path segment of the URL without its query.
            let id = segments.last().ok_or_else(invalid)?.to_string();
            if id.chars().all(|c| c.is_ascii_digit()) {
                Ok(DouyinLink::WebRoom { web_rid: id })
            } else {
                Ok(DouyinLink::DouyinId { id })
            }
        }
        "v.douyin.com" => {
            let code = segments.first().ok_or_else(invalid)?.to_string();
            Ok(DouyinLink::ShortLink { code })
        }
        "www.douyin.com" | "douyin.com" => match segments.as_slice() {
            ["user", sec_uid, ..] => Ok(DouyinLink::WebProfile {
                sec_uid: sec_uid.to_string(),
            }),
            _ => Err(invalid()),
        },
        _ => Err(invalid()),
    }
}

/// Room state parsed from one API response, before quality selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSnapshot {
    /// See `LiveInfo::room_id`.
    pub room_id: String,
    /// Anchor `sec_uid`, stable across link forms.
    pub anchor_id: Option<String>,
    pub live_url: Option<String>,
    pub anchor_name: String,
    pub title: Option<String>,
    pub is_live: bool,
    /// Variants best first: `ORIGIN`, then the platform's FLV map order.
    pub variants: Vec<StreamVariant>,
}

/// Map of URLs that keeps the response's key order (StreamGet relies on dict order).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct OrderedUrls(Vec<(String, String)>);

impl<'de> Deserialize<'de> for OrderedUrls {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor;

        impl<'de> Visitor<'de> for OrderedVisitor {
            type Value = OrderedUrls;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a map of stream URLs")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if let Value::String(url) = value {
                        entries.push((key, url));
                    }
                }
                Ok(OrderedUrls(entries))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedUrls::default())
            }
        }

        deserializer.deserialize_any(OrderedVisitor)
    }
}

#[derive(Debug, Default, Deserialize)]
struct StreamUrlRaw {
    #[serde(default, alias = "flvPullUrl")]
    flv_pull_url: OrderedUrls,
    #[serde(default, alias = "hlsPullUrlMap")]
    hls_pull_url_map: OrderedUrls,
    #[serde(default, alias = "liveCoreSdkData")]
    live_core_sdk_data: Option<LiveCoreSdkData>,
}

#[derive(Debug, Default, Deserialize)]
struct LiveCoreSdkData {
    #[serde(default, alias = "pullData")]
    pull_data: Option<PullData>,
}

#[derive(Debug, Default, Deserialize)]
struct PullData {
    #[serde(default, alias = "streamData")]
    stream_data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RoomRaw {
    #[serde(default)]
    status: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    owner: Option<UserRaw>,
    #[serde(default, alias = "streamUrl")]
    stream_url: Option<StreamUrlRaw>,
}

#[derive(Debug, Deserialize)]
struct UserRaw {
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    web_rid: Option<String>,
    #[serde(default)]
    sec_uid: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiData {
    /// Web enter API: list of rooms.
    #[serde(default)]
    data: Option<Vec<RoomRaw>>,
    /// Web enter API: anchor.
    #[serde(default)]
    user: Option<UserRaw>,
    /// App reflow API: the room.
    #[serde(default)]
    room: Option<RoomRaw>,
    #[serde(default)]
    prompts: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope {
    #[serde(default)]
    status_code: Option<i64>,
    #[serde(default)]
    data: Option<ApiData>,
}

/// Classify a body that is not the expected JSON.
fn classify_unparsable(body: &str, error: &serde_json::Error) -> PlatformError {
    let trimmed = body.trim_start();
    if trimmed.is_empty() {
        // StreamGet: "it triggered risk control".
        return PlatformError::RateLimited("抖音接口返回空内容，可能触发风控".to_string());
    }
    if trimmed.starts_with('<') {
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("captcha") || lower.contains("verify") || lower.contains("验证") {
            return PlatformError::RateLimited("抖音返回了验证页面，可能触发风控".to_string());
        }
        return PlatformError::RateLimited(
            "抖音返回了网页而不是接口数据，可能触发风控".to_string(),
        );
    }
    PlatformError::Parse(format!("抖音接口数据无法解析: {error}"))
}

/// Classify a JSON response whose `status_code` is not zero.
fn classify_api_status(
    status_code: i64,
    prompts: Option<&str>,
    message: Option<&str>,
) -> PlatformError {
    let prompts = prompts.unwrap_or_default();
    let message = message.unwrap_or_default();
    let detail = [prompts, message]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" / ");
    let detail = format!("抖音接口状态 {status_code}: {detail}");
    if prompts.contains("登录") || message.to_ascii_lowercase().contains("login") {
        return PlatformError::AuthRequired(detail);
    }
    match status_code {
        // "该内容暂时无法无法查看": unknown or removed room.
        4001038 => PlatformError::InvalidUrl(detail),
        // "Request params error" + "当前服务繁忙，请稍后重试". Observed for 抖音号
        // that do not exist, but the response is identical to a busy service,
        // so it stays retryable instead of marking the link as dead.
        10011 => PlatformError::RateLimited(format!(
            "抖音返回参数错误或服务繁忙（抖音号可能不存在）: {detail}"
        )),
        _ if prompts.contains("繁忙") || prompts.contains("频繁") => {
            PlatformError::RateLimited(detail)
        }
        _ => PlatformError::Parse(detail),
    }
}

fn parse_envelope(body: &str) -> Result<ApiData, PlatformError> {
    let envelope: ApiEnvelope =
        serde_json::from_str(body).map_err(|error| classify_unparsable(body, &error))?;
    let data = envelope.data.unwrap_or_default();
    match envelope.status_code {
        Some(0) | None => Ok(data),
        Some(code) => Err(classify_api_status(
            code,
            data.prompts.as_deref(),
            data.message.as_deref(),
        )),
    }
}

/// Build variants from a room's `stream_url`, following StreamGet: an
/// `ORIGIN` entry from `live_core_sdk_data` (with `&codec=` appended), then
/// the FLV/HLS maps; FLV and HLS entries are paired by position.
fn build_variants(stream_url: &StreamUrlRaw) -> Result<Vec<StreamVariant>, PlatformError> {
    let mut flv = stream_url.flv_pull_url.0.clone();
    let mut hls = stream_url.hls_pull_url_map.0.clone();

    if let Some(stream_data) = stream_url
        .live_core_sdk_data
        .as_ref()
        .and_then(|sdk| sdk.pull_data.as_ref())
        .and_then(|pull| pull.stream_data.as_deref())
        .filter(|data| !data.starts_with('$') && !data.is_empty())
    {
        let parsed: Value = serde_json::from_str(stream_data)
            .map_err(|error| PlatformError::Parse(format!("stream_data 无法解析: {error}")))?;
        if let Some(main) = parsed.pointer("/data/origin/main") {
            let codec = sdk_codec(main.get("sdk_params"));
            let origin_flv = main.get("flv").and_then(Value::as_str).unwrap_or_default();
            let origin_hls = main.get("hls").and_then(Value::as_str).unwrap_or_default();
            // An empty origin URL stays empty (filtered out below) so FLV and
            // HLS positions stay aligned without inventing a bare "&codec=" URL.
            let with_codec = |url: &str| {
                if url.is_empty() {
                    String::new()
                } else {
                    format!("{url}&codec={codec}")
                }
            };
            merge_origin(&mut flv, with_codec(origin_flv));
            merge_origin(&mut hls, with_codec(origin_hls));
        }
    }

    if flv.is_empty() && hls.is_empty() {
        return Err(PlatformError::Parse(
            "直播中但没有找到可用的流地址".to_string(),
        ));
    }

    // StreamGet indexes the FLV and HLS value lists independently.
    let count = flv.len().max(hls.len());
    let variants = (0..count)
        .map(|index| {
            let flv_entry = flv.get(index);
            let hls_entry = hls.get(index);
            let name = flv_entry
                .or(hls_entry)
                .map(|(name, _)| name.clone())
                .unwrap_or_default();
            StreamVariant {
                name,
                quality: Quality::from_index(index),
                flv_url: flv_entry
                    .map(|(_, url)| url.clone())
                    .filter(|url| !url.is_empty()),
                hls_url: hls_entry
                    .map(|(_, url)| url.clone())
                    .filter(|url| !url.is_empty()),
            }
        })
        .collect();
    Ok(variants)
}

/// `{'ORIGIN': url, **map}`: ORIGIN goes first unless the map already has it,
/// in which case the map's value wins at the first position.
fn merge_origin(entries: &mut Vec<(String, String)>, origin_url: String) {
    if let Some(position) = entries.iter().position(|(name, _)| name == "ORIGIN") {
        let existing = entries.remove(position);
        entries.insert(0, existing);
    } else {
        entries.insert(0, ("ORIGIN".to_string(), origin_url));
    }
}

/// `VCodec` from `sdk_params`, which is a JSON string in the APIs and an object in some pages.
fn sdk_codec(sdk_params: Option<&Value>) -> String {
    let parsed;
    let object = match sdk_params {
        Some(Value::String(text)) => {
            parsed = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
            &parsed
        }
        Some(value) => value,
        None => return String::new(),
    };
    object
        .get("VCodec")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn room_snapshot(
    room: &RoomRaw,
    room_id: String,
    anchor_id: Option<String>,
    live_url: Option<String>,
    anchor_name: String,
) -> Result<RoomSnapshot, PlatformError> {
    let is_live = room.status == Some(STATUS_LIVE);
    let variants = if is_live {
        let stream_url = room
            .stream_url
            .as_ref()
            .ok_or_else(|| PlatformError::Parse("直播中但缺少 stream_url".to_string()))?;
        build_variants(stream_url)?
    } else {
        Vec::new()
    };
    Ok(RoomSnapshot {
        room_id,
        anchor_id,
        live_url,
        anchor_name,
        title: room.title.clone().filter(|title| !title.is_empty()),
        is_live,
        variants,
    })
}

/// Parse a `webcast/room/web/enter` response for `web_rid`.
pub fn parse_web_enter(body: &str, web_rid: &str) -> Result<RoomSnapshot, PlatformError> {
    let data = parse_envelope(body)?;
    let room = match data.data.as_deref() {
        Some([room, ..]) => room,
        _ => {
            return Err(match data.prompts {
                Some(prompts) if prompts.contains("登录") => PlatformError::AuthRequired(prompts),
                Some(prompts) => PlatformError::InvalidUrl(prompts),
                // StreamGet: "VR live is not supported".
                None => PlatformError::InvalidUrl("直播间不存在，或为不支持的 VR 直播".to_string()),
            });
        }
    };
    let user = data.user.as_ref();
    let anchor_name = user
        .and_then(|user| user.nickname.clone())
        .ok_or_else(|| PlatformError::Parse("接口数据缺少主播昵称".to_string()))?;
    let anchor_id = user
        .and_then(|user| user.sec_uid.clone())
        .filter(|id| !id.is_empty());
    room_snapshot(
        room,
        web_rid.to_string(),
        anchor_id,
        Some(format!("https://live.douyin.com/{web_rid}")),
        anchor_name,
    )
}

/// Parse a `webcast/room/reflow/info` response.
pub fn parse_app_reflow(body: &str) -> Result<RoomSnapshot, PlatformError> {
    let data = parse_envelope(body)?;
    let room = data.room.as_ref().ok_or_else(|| {
        PlatformError::InvalidUrl("直播间不存在，或为不支持的 VR 直播".to_string())
    })?;
    let owner = room
        .owner
        .as_ref()
        .ok_or_else(|| PlatformError::Parse("接口数据缺少主播信息".to_string()))?;
    let anchor_name = owner
        .nickname
        .clone()
        .ok_or_else(|| PlatformError::Parse("接口数据缺少主播昵称".to_string()))?;
    let web_rid = owner.web_rid.clone().filter(|rid| !rid.is_empty());
    let anchor_id = owner.sec_uid.clone().filter(|id| !id.is_empty());
    // Never fall back to `id_str`: it changes with every broadcast.
    let room_id = web_rid
        .clone()
        .or_else(|| anchor_id.clone())
        .ok_or_else(|| PlatformError::Parse("接口数据缺少 web_rid 和 sec_uid".to_string()))?;
    let live_url = web_rid.map(|rid| format!("https://live.douyin.com/{rid}"));
    room_snapshot(room, room_id, anchor_id, live_url, anchor_name)
}

/// After following a share link: `Some((room_id, sec_user_id))` when it
/// landed on a `reflow/` room page (`DouyinUtils.get_sec_user_id`).
pub fn parse_reflow_redirect(final_url: &str) -> Result<Option<(String, String)>, PlatformError> {
    if !final_url.contains("reflow/") {
        return Ok(None);
    }
    let url = Url::parse(final_url)
        .map_err(|error| PlatformError::Parse(format!("跳转地址无效: {error}")))?;
    let room_id = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map(str::to_string)
        .ok_or_else(|| PlatformError::Parse("跳转地址中没有直播间 ID".to_string()))?;
    let sec_user_id = url
        .query_pairs()
        .find(|(key, _)| key == "sec_user_id")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PlatformError::Parse("跳转地址中没有 sec_user_id".to_string()))?;
    Ok(Some((room_id, sec_user_id)))
}

/// Last path segment of a profile URL (`DouyinUtils.get_unique_id`).
pub fn profile_sec_uid(final_url: &str) -> Option<String> {
    let without_query = final_url.split(['?', '#']).next()?;
    without_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
}

/// Last `unique_id":"…","verification_type` match in the iesdouyin profile page.
pub fn parse_unique_id(html: &str) -> Option<String> {
    const START: &str = "unique_id\":\"";
    const END: &str = "\",\"verification_type";
    let mut found = None;
    let mut rest = html;
    while let Some(start) = rest.find(START) {
        let after = &rest[start + START.len()..];
        match after.find(END) {
            // The Python regex is non-greedy and does not cross quotes in practice.
            Some(end) if !after[..end].contains('"') => {
                found = Some(after[..end].to_string());
                rest = &after[end..];
            }
            _ => rest = after,
        }
    }
    found
}

/// Turn a snapshot into `LiveInfo`, choosing the stream for `quality`.
/// `primary_reachable` is the result of StreamGet's HEAD probe on the chosen
/// HLS URL; when it failed, the neighbouring tier is used instead.
pub fn build_live_info(
    snapshot: RoomSnapshot,
    quality: Quality,
    primary_reachable: bool,
) -> LiveInfo {
    let index = if primary_reachable {
        select_variant_index(snapshot.variants.len(), quality)
    } else {
        fallback_variant_index(snapshot.variants.len(), quality)
    };
    let selected = index
        .and_then(|index| snapshot.variants.get(index))
        .filter(|_| snapshot.is_live)
        .and_then(|variant| selected_stream(quality, variant));
    LiveInfo {
        platform: Platform::Douyin,
        room_id: snapshot.room_id,
        anchor_id: snapshot.anchor_id,
        live_url: snapshot.live_url,
        anchor_name: snapshot.anchor_name,
        title: snapshot.title,
        is_live: snapshot.is_live && selected.is_some(),
        variants: snapshot.variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Douyin streams. StreamCap sends no extra headers to
/// FFmpeg for Douyin; the PC User-Agent and live Referer match what StreamGet
/// uses when probing the stream. User cookies are deliberately not forwarded
/// to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), PC_USER_AGENT.to_string()),
        ("Referer".to_string(), LIVE_REFERER.to_string()),
    ])
}

fn cookie_header(value: &str) -> Result<HeaderValue, PlatformError> {
    HeaderValue::from_str(value).map_err(|_| {
        PlatformError::Config("Cookie 含有换行等无法发送的字符，请检查平台 Cookie 设置".to_string())
    })
}

/// Append the signature verbatim, as StreamGet does (`api += "&a_bogus=" + a_bogus`).
fn append_signature(api: &Url, signature: &str) -> String {
    format!("{}&a_bogus={signature}", api.as_str())
}

/// Headers for StreamGet's HEAD probe on the chosen stream. The probe goes to
/// CDN hosts, so it always uses the built-in cookie, never the user's.
fn probe_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(PC_USER_AGENT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
    );
    headers.insert(REFERER, HeaderValue::from_static(LIVE_REFERER));
    headers.insert(COOKIE, HeaderValue::from_static(PC_DEFAULT_COOKIE));
    headers
}

fn user_cookie(options: &FetchOptions) -> Option<&str> {
    options
        .cookies
        .as_deref()
        .map(str::trim)
        .filter(|cookie| !cookie.is_empty())
}

/// Douyin adapter. Holds one HTTP client per proxy setting so repeated polls
/// reuse connections; the client is rebuilt when the proxy changes.
#[derive(Debug, Default)]
pub struct DouyinAdapter {
    client: Mutex<Option<(Option<String>, reqwest::Client)>>,
}

impl DouyinAdapter {
    pub fn new() -> Self {
        DouyinAdapter::default()
    }

    fn client(&self, proxy: Option<&str>) -> Result<reqwest::Client, PlatformError> {
        let key = proxy
            .map(str::trim)
            .filter(|proxy| !proxy.is_empty())
            .map(str::to_string);
        let mut cached = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached_key, client)) = cached.as_ref()
            && *cached_key == key
        {
            return Ok(client.clone());
        }
        let client = build_client(key.as_deref())?;
        *cached = Some((key, client.clone()));
        Ok(client)
    }

    /// Follow a share link and return where it lands. Like StreamGet, only the
    /// final URL matters: the status code is ignored and the body is not read.
    async fn resolve_final_url(
        client: &reqwest::Client,
        url: &str,
    ) -> Result<String, PlatformError> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(MOBILE_USER_AGENT));
        headers.insert(
            ACCEPT_LANGUAGE,
            HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
        );
        headers.insert(COOKIE, HeaderValue::from_static(RESOLVE_COOKIE));
        let response = client.get(url).headers(headers).send().await?;
        Ok(response.url().to_string())
    }

    async fn get_text(
        client: &reqwest::Client,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(String, String), PlatformError> {
        let response = client.get(url).headers(headers).send().await?;
        let final_url = response.url().to_string();
        let status = response.status();
        if !status.is_success() && !status.is_redirection() {
            return Err(super::classify_http_status(status.as_u16(), url));
        }
        let body = response.text().await?;
        Ok((final_url, body))
    }

    async fn fetch_web(
        &self,
        client: &reqwest::Client,
        web_rid: &str,
        options: &FetchOptions,
    ) -> Result<RoomSnapshot, PlatformError> {
        let mut api = Url::parse(WEB_ENTER_API).expect("valid web enter API URL");
        api.query_pairs_mut()
            .append_pair("aid", "6383")
            .append_pair("app_name", "douyin_web")
            .append_pair("live_id", "1")
            .append_pair("device_platform", "web")
            .append_pair("language", "zh-CN")
            .append_pair("browser_language", "zh-CN")
            .append_pair("browser_platform", "Win32")
            .append_pair("browser_name", "Chrome")
            .append_pair("browser_version", "116.0.0.0")
            .append_pair("web_rid", web_rid)
            .append_pair("is_need_double_stream", "false")
            .append_pair("msToken", "");
        let signature = ab_sign(api.query().unwrap_or_default(), WEB_API_USER_AGENT);
        let api = append_signature(&api, &signature);

        let cookie = match user_cookie(options) {
            Some(cookie) if cookie.contains("ttwid=") => cookie,
            _ => WEB_API_DEFAULT_COOKIE,
        };
        let mut headers = HeaderMap::new();
        headers.insert(REFERER, HeaderValue::from_static(WEB_API_REFERER));
        headers.insert(USER_AGENT, HeaderValue::from_static(WEB_API_USER_AGENT));
        headers.insert(COOKIE, cookie_header(cookie)?);

        let (_, body) = Self::get_text(client, &api, headers).await?;
        parse_web_enter(&body, web_rid)
    }

    async fn fetch_app(
        &self,
        client: &reqwest::Client,
        link: &DouyinLink,
        options: &FetchOptions,
    ) -> Result<RoomSnapshot, PlatformError> {
        let resolve_headers = || {
            let mut headers = HeaderMap::new();
            headers.insert(USER_AGENT, HeaderValue::from_static(MOBILE_USER_AGENT));
            headers.insert(
                ACCEPT_LANGUAGE,
                HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
            );
            headers
        };

        let final_url = match link {
            // The profile URL already holds the sec_uid StreamGet would read
            // from the (non-redirecting) response URL.
            DouyinLink::WebProfile { .. } => link.normalized_url(),
            _ => Self::resolve_final_url(client, &link.normalized_url()).await?,
        };

        if let Some((room_id, sec_user_id)) = parse_reflow_redirect(&final_url)? {
            let mut api = Url::parse(APP_REFLOW_API).expect("valid reflow API URL");
            api.query_pairs_mut()
                .append_pair(
                    "verifyFp",
                    "verify_lxj5zv70_7szNlAB7_pxNY_48Vh_ALKF_GA1Uf3yteoOY",
                )
                .append_pair("type_id", "0")
                .append_pair("live_id", "1")
                .append_pair("room_id", &room_id)
                .append_pair("sec_user_id", &sec_user_id)
                .append_pair("version_code", "99.99.99")
                .append_pair("app_id", "1128")
                .append_pair("is_need_double_stream", "True");
            let signature = ab_sign(api.query().unwrap_or_default(), MOBILE_USER_AGENT);
            let api = append_signature(&api, &signature);

            let mut headers = resolve_headers();
            headers.insert(
                COOKIE,
                cookie_header(user_cookie(options).unwrap_or_default())?,
            );
            let (_, body) = Self::get_text(client, &api, headers).await?;
            return parse_app_reflow(&body);
        }

        // Profile link: find the 抖音号 and use the web flow.
        let sec_uid = profile_sec_uid(&final_url).ok_or_else(|| {
            PlatformError::InvalidUrl(format!("无法从链接中识别主播: {final_url}"))
        })?;
        let mut headers = resolve_headers();
        headers.insert(COOKIE, HeaderValue::from_static(PROFILE_COOKIE));
        let (_, html) =
            Self::get_text(client, &format!("{PROFILE_PAGE}{sec_uid}"), headers).await?;
        let unique_id = parse_unique_id(&html)
            .ok_or_else(|| PlatformError::Parse("主页中没有找到抖音号 (unique_id)".to_string()))?;
        if unique_id.is_empty() {
            return Err(PlatformError::InvalidUrl(
                "该主播没有抖音号，无法通过主页链接定位直播间".to_string(),
            ));
        }
        self.fetch_web(client, &unique_id, options).await
    }

    /// StreamGet's `get_response_status` probe: a HEAD request that only
    /// counts transport failures (any HTTP status is "reachable").
    async fn probe(client: &reqwest::Client, url: &str) -> bool {
        client
            .head(url)
            .headers(probe_headers())
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .is_ok()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.client(options.proxy.as_deref())?;
        let snapshot = match &link {
            DouyinLink::WebRoom { web_rid } => self.fetch_web(&client, web_rid, options).await?,
            DouyinLink::DouyinId { id } => self.fetch_web(&client, id, options).await?,
            _ => self.fetch_app(&client, &link, options).await?,
        };

        let mut reachable = true;
        if snapshot.is_live {
            // StreamGet probes the HLS URL of the chosen tier.
            let primary_hls = select_variant_index(snapshot.variants.len(), options.quality)
                .and_then(|index| snapshot.variants.get(index))
                .and_then(|variant| variant.hls_url.clone());
            if let Some(hls_url) = primary_hls {
                reachable = Self::probe(&client, &hls_url).await;
            }
        }
        Ok(build_live_info(snapshot, options.quality, reachable))
    }
}

impl PlatformAdapter for DouyinAdapter {
    fn platform(&self) -> Platform {
        Platform::Douyin
    }

    fn matches(&self, url: &str) -> bool {
        parse_link(url).is_ok()
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

    const WEB_LIVE: &str = include_str!("../../tests/fixtures/douyin/web_enter_live.json");
    const WEB_OFFLINE: &str = include_str!("../../tests/fixtures/douyin/web_enter_offline.json");
    const WEB_NOT_FOUND: &str =
        include_str!("../../tests/fixtures/douyin/web_enter_not_found.json");
    const WEB_10011: &str =
        include_str!("../../tests/fixtures/douyin/web_enter_10011_params_error.json");
    const APP_LIVE: &str = include_str!("../../tests/fixtures/douyin/app_reflow_live.json");
    const APP_OFFLINE: &str = include_str!("../../tests/fixtures/douyin/app_reflow_offline.json");
    const PROFILE_HTML: &str = include_str!("../../tests/fixtures/douyin/share_user_snippet.html");

    #[test]
    fn recognizes_and_normalizes_the_five_readme_links() {
        let cases = [
            (
                "https://live.douyin.com/745964462470",
                DouyinLink::WebRoom {
                    web_rid: "745964462470".into(),
                },
                "https://live.douyin.com/745964462470",
                false,
            ),
            (
                "https://v.douyin.com/iQFeBnt/",
                DouyinLink::ShortLink {
                    code: "iQFeBnt".into(),
                },
                "https://v.douyin.com/iQFeBnt/",
                true,
            ),
            (
                "https://live.douyin.com/yall1102",
                DouyinLink::DouyinId {
                    id: "yall1102".into(),
                },
                "https://live.douyin.com/yall1102",
                false,
            ),
            (
                "https://v.douyin.com/CeiU5cbX",
                DouyinLink::ShortLink {
                    code: "CeiU5cbX".into(),
                },
                "https://v.douyin.com/CeiU5cbX/",
                true,
            ),
            (
                "https://www.douyin.com/user/MS4wLjABAAAA3kr2yA4aRD-sjf9cx8xkOH8Di3RjktpKcAvqIetpsF0",
                DouyinLink::WebProfile {
                    sec_uid: "MS4wLjABAAAA3kr2yA4aRD-sjf9cx8xkOH8Di3RjktpKcAvqIetpsF0".into(),
                },
                "https://www.douyin.com/user/MS4wLjABAAAA3kr2yA4aRD-sjf9cx8xkOH8Di3RjktpKcAvqIetpsF0",
                true,
            ),
        ];
        for (input, expected, normalized, app_flow) in cases {
            let link = parse_link(input).unwrap();
            assert_eq!(link, expected, "{input}");
            assert_eq!(link.normalized_url(), normalized, "{input}");
            assert_eq!(link.uses_app_flow(), app_flow, "{input}");
            assert!(DouyinAdapter::new().matches(input));
        }
    }

    #[test]
    fn normalizes_query_strings_missing_scheme_and_whitespace() {
        assert_eq!(
            parse_link("  live.douyin.com/745964462470?anchor_id=1&from=share ").unwrap(),
            DouyinLink::WebRoom {
                web_rid: "745964462470".into()
            }
        );
        assert_eq!(
            parse_link("https://www.douyin.com/user/MS4wLjABAAAAx?from_tab_name=live").unwrap(),
            DouyinLink::WebProfile {
                sec_uid: "MS4wLjABAAAAx".into()
            }
        );
        assert_eq!(
            parse_link("http://v.douyin.com/iQFeBnt")
                .unwrap()
                .normalized_url(),
            "https://v.douyin.com/iQFeBnt/"
        );
    }

    #[test]
    fn rejects_links_of_other_sites_and_incomplete_links() {
        for input in [
            "https://www.huya.com/123",
            "https://live.douyin.com/",
            "https://www.douyin.com/video/123",
            "ftp://live.douyin.com/1",
            "",
        ] {
            let error = parse_link(input).unwrap_err();
            assert_eq!(error.kind(), "invalid_url", "{input}");
            assert!(!DouyinAdapter::new().matches(input));
        }
        assert!(super::super::adapter_for("https://live.douyin.com/1").is_some());
        assert!(super::super::adapter_for("https://www.huya.com/1").is_none());
    }

    #[test]
    fn web_live_room_lists_origin_first_and_selects_requested_quality() {
        let snapshot = parse_web_enter(WEB_LIVE, "100000000001").unwrap();
        assert!(snapshot.is_live);
        assert_eq!(snapshot.anchor_name, "测试主播");
        assert_eq!(snapshot.title.as_deref(), Some("测试直播间"));
        assert_eq!(
            snapshot.live_url.as_deref(),
            Some("https://live.douyin.com/100000000001")
        );
        let names: Vec<&str> = snapshot
            .variants
            .iter()
            .map(|variant| variant.name.as_str())
            .collect();
        assert_eq!(names, ["ORIGIN", "FULL_HD1", "HD1", "SD1", "SD2"]);
        let origin = &snapshot.variants[0];
        assert_eq!(origin.quality, Some(Quality::Od));
        assert!(origin.flv_url.as_deref().unwrap().ends_with("&codec=h264"));
        assert!(origin.hls_url.as_deref().unwrap().ends_with("&codec=h264"));

        let info = build_live_info(snapshot.clone(), Quality::Hd, true);
        assert!(info.is_live);
        let selected = info.selected.unwrap();
        assert_eq!(selected.requested, Quality::Hd);
        assert_eq!(selected.variant.name, "HD1");
        assert_eq!(
            selected.record_url,
            selected.variant.hls_url.clone().unwrap()
        );
        assert_eq!(
            info.stream_headers.get("Referer").map(String::as_str),
            Some("https://live.douyin.com/")
        );

        let od = build_live_info(snapshot, Quality::Od, true)
            .selected
            .unwrap();
        assert_eq!(od.variant.name, "ORIGIN");
        assert_eq!(
            od.source_url(true, false).1,
            od.variant.flv_url.as_deref().unwrap()
        );
    }

    #[test]
    fn unreachable_primary_stream_falls_back_like_streamget() {
        let snapshot = parse_web_enter(WEB_LIVE, "1").unwrap();
        assert_eq!(
            build_live_info(snapshot.clone(), Quality::Od, false)
                .selected
                .unwrap()
                .variant
                .name,
            "FULL_HD1"
        );
        assert_eq!(
            build_live_info(snapshot.clone(), Quality::Sd, false)
                .selected
                .unwrap()
                .variant
                .name,
            "SD2"
        );
        assert_eq!(
            build_live_info(snapshot, Quality::Ld, false)
                .selected
                .unwrap()
                .variant
                .name,
            "SD1"
        );
    }

    #[test]
    fn missing_tiers_use_the_lowest_available_variant() {
        let mut snapshot = parse_web_enter(WEB_LIVE, "1").unwrap();
        snapshot.variants.truncate(3); // ORIGIN, FULL_HD1, HD1
        for quality in [Quality::Hd, Quality::Sd, Quality::Ld] {
            assert_eq!(
                build_live_info(snapshot.clone(), quality, true)
                    .selected
                    .unwrap()
                    .variant
                    .name,
                "HD1"
            );
        }
        assert_eq!(
            build_live_info(snapshot.clone(), Quality::Uhd, true)
                .selected
                .unwrap()
                .variant
                .name,
            "FULL_HD1"
        );
        // Fallback from LD on a padded list goes to index 3, which is padding for HD1 too.
        assert_eq!(
            build_live_info(snapshot, Quality::Ld, false)
                .selected
                .unwrap()
                .variant
                .name,
            "HD1"
        );
    }

    #[test]
    fn h265_origin_is_recorded_from_hls() {
        // VCodec sits in doubly-escaped JSON inside stream_data; swap every occurrence.
        let body = WEB_LIVE.replace("h264", "h265");
        assert_ne!(body, WEB_LIVE);
        let info = build_live_info(parse_web_enter(&body, "1").unwrap(), Quality::Od, true);
        let selected = info.selected.unwrap();
        assert_eq!(selected.flv_codec().as_deref(), Some("h265"));
        let (format, url) = selected.source_url(true, false);
        assert_eq!(format, super::super::SourceFormat::Hls);
        assert!(url.ends_with("&codec=h265"));
    }

    #[test]
    fn web_offline_room_reports_anchor_without_streams() {
        let snapshot = parse_web_enter(WEB_OFFLINE, "100000000002").unwrap();
        assert!(!snapshot.is_live);
        assert_eq!(snapshot.anchor_name, "离线主播");
        assert!(snapshot.variants.is_empty());
        let info = build_live_info(snapshot, Quality::Od, true);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert_eq!(info.platform.display_name(), "抖音");
    }

    #[test]
    fn app_reflow_live_and_offline_rooms_parse() {
        let live = parse_app_reflow(APP_LIVE).unwrap();
        assert!(live.is_live);
        assert_eq!(live.room_id, "100000000001");
        assert_eq!(
            live.live_url.as_deref(),
            Some("https://live.douyin.com/100000000001")
        );
        assert_eq!(live.variants.len(), 5);
        assert_eq!(live.variants[0].name, "ORIGIN");
        let info = build_live_info(live, Quality::Ld, true);
        assert_eq!(info.selected.unwrap().variant.name, "SD2");

        let offline = parse_app_reflow(APP_OFFLINE).unwrap();
        assert!(!offline.is_live);
        assert_eq!(offline.anchor_name, "离线主播");
        assert_eq!(offline.title.as_deref(), Some("测试直播间（未开播）"));
    }

    #[test]
    fn room_id_never_uses_the_per_broadcast_id() {
        let web = parse_web_enter(WEB_LIVE, "100000000001").unwrap();
        assert_eq!(web.anchor_id.as_deref(), Some("MS4wLjABAAAAtest_sec_uid"));

        let app = parse_app_reflow(APP_LIVE).unwrap();
        assert_eq!(app.anchor_id.as_deref(), Some("MS4wLjABAAAAtest_sec_uid"));

        // Without web_rid the room falls back to the anchor sec_uid, not id_str.
        let no_web_rid = APP_OFFLINE.replace("\"web_rid\": \"100000000001\",", "");
        assert_ne!(no_web_rid, APP_OFFLINE);
        let snapshot = parse_app_reflow(&no_web_rid).unwrap();
        assert_eq!(snapshot.room_id, "MS4wLjABAAAAtest_sec_uid");
        assert_eq!(snapshot.live_url, None);

        let neither = r#"{"status_code":0,"data":{"room":{"id_str":"7000000000000000009","status":4,"owner":{"nickname":"a"}}}}"#;
        assert_eq!(parse_app_reflow(neither).unwrap_err().kind(), "parse");
    }

    #[test]
    fn probe_and_stream_headers_never_carry_user_cookies() {
        let headers = probe_headers();
        assert_eq!(headers.get(COOKIE).unwrap(), PC_DEFAULT_COOKIE);
        assert_eq!(headers.get(REFERER).unwrap(), LIVE_REFERER);
        let stream = stream_headers();
        assert!(stream.keys().all(|key| !key.eq_ignore_ascii_case("cookie")));
    }

    #[test]
    fn code_10011_is_retryable_and_4001038_is_invalid() {
        let error = parse_web_enter(WEB_10011, "zz_no_such_user").unwrap_err();
        assert_eq!(error.kind(), "rate_limited");
        assert!(error.message().contains("参数错误或服务繁忙"));
        assert!(error.message().contains("Request params error"));
        assert_eq!(
            parse_web_enter(WEB_NOT_FOUND, "1").unwrap_err().kind(),
            "invalid_url"
        );
    }

    #[test]
    fn empty_origin_url_is_skipped_instead_of_becoming_a_codec_only_url() {
        let snapshot = parse_web_enter(WEB_LIVE, "1").unwrap();
        let origin_hls = snapshot.variants[0].hls_url.clone().unwrap();
        let raw_origin_hls = origin_hls.trim_end_matches("&codec=h264");
        let body = WEB_LIVE.replace(raw_origin_hls, "");
        assert_ne!(body, WEB_LIVE);

        let snapshot = parse_web_enter(&body, "1").unwrap();
        let origin = &snapshot.variants[0];
        assert_eq!(origin.name, "ORIGIN");
        assert_eq!(origin.hls_url, None);
        assert!(origin.flv_url.as_deref().unwrap().starts_with("http://"));
        assert_eq!(snapshot.variants[1].name, "FULL_HD1");
        assert!(snapshot.variants[1].hls_url.is_some());
        for variant in &snapshot.variants {
            for url in [&variant.flv_url, &variant.hls_url].into_iter().flatten() {
                assert!(!url.starts_with('&'), "{url}");
            }
        }
        let selected = build_live_info(snapshot, Quality::Od, true)
            .selected
            .unwrap();
        assert_eq!(
            selected.record_url,
            selected.variant.flv_url.clone().unwrap()
        );
    }

    #[tokio::test]
    async fn cookie_with_line_break_is_a_config_error() {
        let adapter = DouyinAdapter::new();
        let options = FetchOptions {
            cookies: Some("ttwid=abc\nsessionid=def".to_string()),
            ..FetchOptions::default()
        };
        let error = adapter
            .fetch("https://live.douyin.com/100000000001", &options)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), "config");
    }

    #[test]
    fn signature_is_appended_without_percent_encoding() {
        let api = Url::parse("https://live.douyin.com/webcast/room/web/enter/?aid=6383").unwrap();
        assert_eq!(
            append_signature(&api, "Ab/c-d=="),
            "https://live.douyin.com/webcast/room/web/enter/?aid=6383&a_bogus=Ab/c-d=="
        );
    }

    #[test]
    fn client_is_reused_until_the_proxy_changes() {
        let adapter = DouyinAdapter::new();
        adapter.client(None).unwrap();
        adapter.client(Some("  ")).unwrap();
        assert_eq!(adapter.client.lock().unwrap().as_ref().unwrap().0, None);
        adapter.client(Some("127.0.0.1:7890")).unwrap();
        assert_eq!(
            adapter
                .client
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .0
                .as_deref(),
            Some("127.0.0.1:7890")
        );
        assert_eq!(
            adapter
                .client(Some("ftp://127.0.0.1:21"))
                .unwrap_err()
                .kind(),
            "config"
        );
    }

    #[test]
    fn share_link_redirects_split_rooms_from_profiles() {
        let room = "https://webcast.amemv.com/douyin/webcast/reflow/7000000000000000003?u_code=x&sec_user_id=MS4wLjABAAAA7C6m-x_y&utm_source=copy";
        assert_eq!(
            parse_reflow_redirect(room).unwrap(),
            Some(("7000000000000000003".into(), "MS4wLjABAAAA7C6m-x_y".into()))
        );
        let profile = "https://www.iesdouyin.com/share/user/MS4wLjABAAAAfJdQ?sec_uid=MS4wLjABAAAAfJdQ&from_ssr=1";
        assert_eq!(parse_reflow_redirect(profile).unwrap(), None);
        assert_eq!(
            profile_sec_uid(profile).as_deref(),
            Some("MS4wLjABAAAAfJdQ")
        );
        assert_eq!(
            parse_reflow_redirect("https://webcast.amemv.com/douyin/webcast/reflow/1?u_code=x")
                .unwrap_err()
                .kind(),
            "parse"
        );
        assert_eq!(
            parse_unique_id(PROFILE_HTML).as_deref(),
            Some("test_unique_id")
        );
        assert_eq!(parse_unique_id("<html></html>"), None);
    }

    #[test]
    fn errors_are_classified_for_the_ui() {
        assert_eq!(
            parse_web_enter(WEB_NOT_FOUND, "1").unwrap_err().kind(),
            "invalid_url"
        );
        assert_eq!(parse_web_enter("", "1").unwrap_err().kind(), "rate_limited");
        assert_eq!(
            parse_web_enter("<html>验证码</html>", "1")
                .unwrap_err()
                .kind(),
            "rate_limited"
        );
        assert_eq!(
            parse_web_enter("{\"data\": [1,2", "1").unwrap_err().kind(),
            "parse"
        );
        assert_eq!(
            parse_web_enter(
                r#"{"status_code":0,"data":{"data":[{"status":2}],"user":{"nickname":"a"}}}"#,
                "1"
            )
            .unwrap_err()
            .kind(),
            "parse"
        );
        assert_eq!(
            parse_web_enter(
                r#"{"status_code":20003,"data":{"prompts":"请先登录"}}"#,
                "1"
            )
            .unwrap_err()
            .kind(),
            "auth_required"
        );
        assert_eq!(
            parse_web_enter(
                r#"{"status_code":10011,"data":{"prompts":"当前服务繁忙，请稍后重试"}}"#,
                "1"
            )
            .unwrap_err()
            .kind(),
            "rate_limited"
        );
        assert_eq!(
            parse_web_enter(r#"{"status_code":0,"data":{"data":[]}}"#, "1")
                .unwrap_err()
                .kind(),
            "invalid_url"
        );
        assert_eq!(
            parse_app_reflow(r#"{"status_code":0,"data":{}}"#)
                .unwrap_err()
                .kind(),
            "invalid_url"
        );
    }
}
