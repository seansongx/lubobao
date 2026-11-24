//! Huya adapter, following StreamCap's `HuyaHandler` and StreamGet v4.0.10
//! `HuyaLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/huya/live_stream.py`, MIT License, Copyright (c) 2025
//! Hmily. The request flow, headers, CDN choice and the anti-leech parameter
//! algorithm (`get_anti_code`) follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow (StreamCap calls `fetch_app_stream_data`):
//!
//! * A link whose id has letters (`www.huya.com/lpl`) is first resolved to the
//!   numeric room by loading the page with the mini-program headers and
//!   reading `ProfileRoom":{id},"sPrivateHost`.
//! * `mp.huya.com/cache.php?m=Live&do=profileRoom&roomid={id}&showSecret=1`
//!   gives the anchor, `realLiveStatus` and, when live, the CDN lines in
//!   `stream.baseSteamInfoList`. The Tencent line (`TX`) is preferred; for
//!   `TX` and `HW` the `ctype`/`fs` parameters are switched to the web ones.
//! * Rooms whose `gameHostName` is `lol` read the streams from the web room
//!   page instead (`stream: {"data"…,"iWebDefaultBitRate"`) and build fresh
//!   anti-leech parameters with [`anti_code`].
//!
//! Quality tiers come from the room's bit-rate list (`liveData.bitRateInfo`,
//! or `vMultiStreamInfo` on the web page): the original stream first, then the
//! others by bit rate, each requested with `ratio={bitrate}`.

use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::header::{
    ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderName, HeaderValue, REFERER, USER_AGENT,
};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header,
    fallback_variant_index, select_variant_index, user_cookie,
};

/// `BaseLiveStream._get_pc_headers` user agent, also used to pull streams.
pub const PC_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36 Edg/121.0.0.0";
const PC_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.8,zh-TW;q=0.7,zh-HK;q=0.5,en-US;q=0.3,en;q=0.2";
/// `HuyaLiveStream._get_mobile_headers` (WeChat mini-program), used to
/// resolve letter ids.
const MOBILE_USER_AGENT: &str = "ios/7.830 (ios 17.0; ; iPhone 15 (A2846/A3089/A3090/A3092))";
const MOBILE_REFERER: &str = "https://servicewechat.com/wx74767bf0b684f7d3/301/page-frame.html";
const MOBILE_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9";
/// Referer sent when pulling streams.
const STREAM_REFERER: &str = "https://www.huya.com/";

const ROOM_PAGE: &str = "https://www.huya.com/";
const PROFILE_ROOM_API: &str = "https://mp.huya.com/cache.php";

/// Web-page stream data, as in StreamGet's regex
/// `stream: (\{"data".*?),"iWebDefaultBitRate"`.
const WEB_STREAM_START: &str = "stream: {\"data\"";
const WEB_STREAM_END: &str = ",\"iWebDefaultBitRate\"";

/// `get_anti_code` constants from `hysdk-m-202402211431.js`.
const ANTI_CODE_T: u64 = 100;
const ANTI_CODE_SDK_VERSION: u64 = 2403051612;

/// A recognized Huya room link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuyaLink {
    /// Path id: the numeric room number (`52333`) or the anchor's letter id
    /// (`lpl`), which the page resolves to a room number.
    pub id: String,
}

impl HuyaLink {
    /// Canonical URL for the link.
    pub fn normalized_url(&self) -> String {
        format!("{ROOM_PAGE}{}", self.id)
    }

    /// Whether the id is already a room number (no page lookup needed).
    pub fn is_room_number(&self) -> bool {
        self.id.chars().all(|c| c.is_ascii_digit())
    }
}

/// Recognize and normalize a Huya link without any network access.
///
/// StreamCap registers `https://.*\.huya\.com/` and StreamGet takes the last
/// path segment as the room id. Room pages live on `www.huya.com`,
/// `huya.com` and `m.huya.com` with a single path segment; other hosts
/// (`v.huya.com` videos and so on) are not rooms. Query strings are dropped.
pub fn parse_link(input: &str) -> Result<HuyaLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的虎牙直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if !matches!(host.as_str(), "www.huya.com" | "huya.com" | "m.huya.com") {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        [id] if !SITE_PAGES.contains(id)
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) =>
        {
            Ok(HuyaLink { id: id.to_string() })
        }
        _ => Err(invalid()),
    }
}

/// Site pages with a one-segment path that are not rooms: the live list,
/// the category list and the followed-anchors page. Kept short so real room
/// aliases (such as `lpl`) are never rejected.
const SITE_PAGES: [&str; 3] = ["l", "g", "myfollow"];

/// Room number from a page loaded with the mini-program headers
/// (StreamGet: `ProfileRoom":(.*?),"sPrivateHost`). The first non-zero
/// match wins; later ones describe other rooms on the page.
pub fn parse_alias_page(html: &str, id: &str) -> Result<String, PlatformError> {
    const START: &str = "ProfileRoom\":";
    const END: &str = ",\"sPrivateHost";
    let mut rest = html;
    while let Some(start) = rest.find(START) {
        let after = &rest[start + START.len()..];
        if let Some(end) = after.find(END) {
            let value = after[..end].trim().trim_matches('"');
            if !value.is_empty()
                && value.chars().all(|c| c.is_ascii_digit())
                && !value.trim_start_matches('0').is_empty()
            {
                return Ok(value.to_string());
            }
        }
        rest = after;
    }
    if html.contains("找不到此页面") {
        return Err(PlatformError::InvalidUrl(format!(
            "虎牙没有找到主播 {id}，页面已被删除或者链接错误"
        )));
    }
    if html.trim().is_empty()
        || html.contains("验证")
        || html.to_ascii_lowercase().contains("captcha")
    {
        return Err(PlatformError::RateLimited(
            "虎牙返回了空页面或验证页面，可能触发风控".to_string(),
        ));
    }
    Err(PlatformError::Parse(format!(
        "无法从页面找到 {id} 的房间号，请改用 https://www.huya.com/房间号"
    )))
}

/// One entry of the room's bit-rate list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitRate {
    /// `sDisplayName`, e.g. `蓝光20M`, `超清`.
    pub name: String,
    /// `iBitRate` in kbps; `0` is the original stream (no `ratio`).
    pub bitrate: u64,
}

/// Parse a bit-rate list (`[{sDisplayName, iBitRate}, …]`), given as an array
/// or as a JSON string holding one (`liveData.bitRateInfo`). The result is
/// best first: the original stream, then the others by falling bit rate.
/// The original is always present, so a missing list still gives one tier.
/// H.265 entries are skipped.
pub fn parse_bit_rates(value: Option<&Value>) -> Vec<BitRate> {
    let parsed;
    let list = match value {
        Some(Value::Array(list)) => Some(list),
        Some(Value::String(text)) => {
            parsed = serde_json::from_str::<Value>(text).ok();
            parsed.as_ref().and_then(Value::as_array)
        }
        _ => None,
    };
    let mut rates: Vec<BitRate> = list
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            // Only H.264 tiers: H.265 ones (`iCodecType` 1, e.g. `2K HDR`) are a
            // separate stream that the `codec=264` parameters do not reach.
            if entry.get("iCodecType").and_then(Value::as_i64).unwrap_or(0) != 0 {
                return None;
            }
            let bitrate = entry.get("iBitRate").and_then(Value::as_u64)?;
            let name = entry
                .get("sDisplayName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if bitrate == 0 {
                        "原画".to_string()
                    } else {
                        format!("{bitrate}")
                    }
                });
            Some(BitRate { name, bitrate })
        })
        .collect();
    // Original (0) first, then highest bit rate first; drop duplicates.
    rates.sort_by_key(|rate| (rate.bitrate != 0, std::cmp::Reverse(rate.bitrate)));
    rates.dedup_by_key(|rate| rate.bitrate);
    if rates.first().is_none_or(|rate| rate.bitrate != 0) {
        rates.insert(
            0,
            BitRate {
                name: "原画".to_string(),
                bitrate: 0,
            },
        );
    }
    rates
}

/// Room state before quality selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSnapshot {
    /// Numeric room number (`profileRoom`).
    pub room_id: String,
    /// Anchor uid (`profileInfo.uid`), the same for every link form.
    pub anchor_id: Option<String>,
    pub anchor_name: String,
    pub title: Option<String>,
    pub is_live: bool,
    /// Best-first variants, labelled with the tier at each padded position.
    pub variants: Vec<StreamVariant>,
}

/// Result of the `profileRoom` API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRoom {
    pub snapshot: RoomSnapshot,
    /// StreamGet reads `lol` rooms from the web page instead of the API.
    pub needs_web_page: bool,
}

/// A string or number field as a string.
fn text_field(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Join a CDN base, stream name, suffix and query.
fn stream_url(base: &str, stream_name: &str, suffix: &str, query: &str) -> String {
    format!(
        "{}/{stream_name}.{suffix}?{query}",
        base.trim_end_matches('/')
    )
}

/// One variant per bit rate. `ratio` builds the query suffix for a bit rate.
fn variants_for(
    rates: &[BitRate],
    flv_url: &str,
    hls_url: Option<&str>,
    ratio: impl Fn(u64) -> String,
) -> Vec<StreamVariant> {
    rates
        .iter()
        .enumerate()
        .map(|(index, rate)| {
            let suffix = ratio(rate.bitrate);
            StreamVariant {
                name: rate.name.clone(),
                quality: Quality::from_index(index),
                flv_url: Some(format!("{flv_url}{suffix}")),
                hls_url: hls_url.map(|url| format!("{url}{suffix}")),
            }
        })
        .collect()
}

/// StreamGet's app-path CDN choice: the last `TX` line, else the first line.
fn choose_api_line(lines: &[Value]) -> Option<&Value> {
    lines
        .iter()
        .rev()
        .find(|line| line.get("sCdnType").and_then(Value::as_str) == Some("TX"))
        .or_else(|| lines.first())
}

/// StreamGet switches `TX` and `HW` lines to the web player's parameters.
fn web_player_query(cdn: &str, query: &str) -> String {
    if matches!(cdn, "TX" | "HW") {
        query
            .replace("&ctype=tars_mp", "&ctype=huya_webh5")
            .replace("&fs=bhct", "&fs=bgct")
    } else {
        query.to_string()
    }
}

/// Parse a `profileRoom` API response.
pub fn parse_profile_room(body: &str) -> Result<ProfileRoom, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() {
            PlatformError::RateLimited("虎牙接口返回空内容，可能触发风控".to_string())
        } else {
            PlatformError::RateLimited("虎牙接口返回的不是 JSON，可能触发风控".to_string())
        }
    })?;
    let status = json.get("status").and_then(Value::as_i64);
    let message = json
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match status {
        Some(200) => {}
        // "该主播不存在！" for unknown room numbers.
        Some(422) => {
            let message = if message.is_empty() {
                "该主播不存在"
            } else {
                message
            };
            return Err(PlatformError::InvalidUrl(format!("虎牙返回：{message}")));
        }
        other => {
            return Err(PlatformError::RateLimited(format!(
                "虎牙接口返回状态 {}：{message}",
                other.map_or_else(|| "未知".to_string(), |code| code.to_string())
            )));
        }
    }
    let data = json
        .get("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| PlatformError::Parse("虎牙接口缺少 data".to_string()))?;
    let profile = data.get("profileInfo").unwrap_or(&Value::Null);
    let live_data = data.get("liveData").unwrap_or(&Value::Null);
    let anchor_name = text_field(profile, "nick")
        .or_else(|| text_field(live_data, "nick"))
        .ok_or_else(|| PlatformError::Parse("虎牙接口缺少主播昵称".to_string()))?;
    let room_id = text_field(profile, "profileRoom")
        .or_else(|| text_field(live_data, "profileRoom"))
        .ok_or_else(|| PlatformError::Parse("虎牙接口缺少房间号".to_string()))?;
    let anchor_id = text_field(profile, "uid").or_else(|| text_field(live_data, "uid"));
    let is_live = data.get("realLiveStatus").and_then(Value::as_str) == Some("ON");

    let mut snapshot = RoomSnapshot {
        room_id,
        anchor_id,
        anchor_name,
        title: None,
        is_live,
        variants: Vec::new(),
    };
    if !is_live {
        return Ok(ProfileRoom {
            snapshot,
            needs_web_page: false,
        });
    }

    snapshot.title = text_field(live_data, "introduction");
    if live_data.get("gameHostName").and_then(Value::as_str) == Some("lol") {
        return Ok(ProfileRoom {
            snapshot,
            needs_web_page: true,
        });
    }

    let lines = data
        .pointer("/stream/baseSteamInfoList")
        .and_then(Value::as_array)
        .filter(|lines| !lines.is_empty())
        .ok_or_else(|| PlatformError::Parse("直播中但虎牙没有返回流地址".to_string()))?;
    let line = choose_api_line(lines).expect("non-empty list");
    let field = |key: &str| line.get(key).and_then(Value::as_str).unwrap_or_default();
    let (cdn, stream_name, flv_base, flv_code) = (
        field("sCdnType"),
        field("sStreamName"),
        field("sFlvUrl"),
        field("sFlvAntiCode"),
    );
    if stream_name.is_empty() || flv_base.is_empty() {
        return Err(PlatformError::Parse(
            "虎牙流地址缺少 sStreamName 或 sFlvUrl".to_string(),
        ));
    }
    let flv_suffix = Some(field("sFlvUrlSuffix"))
        .filter(|s| !s.is_empty())
        .unwrap_or("flv");
    let hls_suffix = Some(field("sHlsUrlSuffix"))
        .filter(|s| !s.is_empty())
        .unwrap_or("m3u8");
    let flv_url = stream_url(
        flv_base,
        stream_name,
        flv_suffix,
        &web_player_query(cdn, flv_code),
    );
    let hls_url = Some(field("sHlsUrl"))
        .filter(|url| !url.is_empty())
        .map(|base| {
            stream_url(
                base,
                stream_name,
                hls_suffix,
                &web_player_query(cdn, field("sHlsAntiCode")),
            )
        });
    let rates = parse_bit_rates(live_data.get("bitRateInfo"));
    snapshot.variants = variants_for(&rates, &flv_url, hls_url.as_deref(), |bitrate| {
        if bitrate == 0 {
            String::new()
        } else {
            format!("&ratio={bitrate}")
        }
    });
    Ok(ProfileRoom {
        snapshot,
        needs_web_page: false,
    })
}

/// Random inputs of `get_anti_code`, fixed in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AntiCodeSeed {
    /// `int(time.time()) * 1000`.
    pub now_ms: u64,
    /// `random.randint(1400000000000, 1400009999999)`.
    pub uid: u64,
    /// `int(1000 * random.random())`, 0–999.
    pub random_millis: u64,
}

impl AntiCodeSeed {
    pub fn now() -> AntiCodeSeed {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() * 1000)
            .unwrap_or_default();
        let random = random_u64();
        AntiCodeSeed {
            now_ms,
            uid: 1_400_000_000_000 + random % 10_000_000,
            random_millis: (random >> 32) % 1000,
        }
    }
}

/// Non-cryptographic random number from the standard library's hasher keys.
fn random_u64() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default(),
    );
    hasher.finish()
}

/// StreamGet's `get_anti_code`: rebuild the web page's `sFlvAntiCode` into
/// the parameters the mobile SDK sends.
///
/// `wsSecret = md5("{fm prefix}_{uid}_{stream}_{md5("{seqid}|{ctype}|100")}_{wsTime}")`
/// where the `fm` prefix is the part before the first `_` of the base64
/// decoded `fm`, `seqid = uid + now`, and `wsTime` is `now + 110624 ms` in
/// hex seconds.
pub fn anti_code(
    old_anti_code: &str,
    stream_name: &str,
    seed: AntiCodeSeed,
) -> Result<String, PlatformError> {
    let query = Url::parse(&format!("http://anti.code/?{old_anti_code}"))
        .map_err(|_| PlatformError::Parse("虎牙防盗链参数格式无法识别".to_string()))?;
    let param = |key: &str| {
        query
            .query_pairs()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| PlatformError::Parse(format!("虎牙防盗链参数缺少 {key}")))
    };
    let (fm, ctype, fs) = (param("fm")?, param("ctype")?, param("fs")?);
    let fm = base64_decode(&fm)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| PlatformError::Parse("虎牙防盗链参数 fm 无法解码".to_string()))?;
    let secret_prefix = fm.split('_').next().unwrap_or_default();

    let AntiCodeSeed {
        now_ms,
        uid,
        random_millis,
    } = seed;
    let init_uuid = ((now_ms % 10_000_000_000) * 1000 + random_millis) % 4_294_967_295;
    let seq_id = uid + now_ms;
    let ws_time = format!("{:x}", (now_ms + 110_624) / 1000);
    let hash = md5_hex(format!("{seq_id}|{ctype}|{ANTI_CODE_T}").as_bytes());
    let secret =
        md5_hex(format!("{secret_prefix}_{uid}_{stream_name}_{hash}_{ws_time}").as_bytes());
    Ok(format!(
        "wsSecret={secret}&wsTime={ws_time}&seqid={seq_id}&ctype={ctype}&ver=1&fs={fs}\
         &uuid={init_uuid}&u={uid}&t={ANTI_CODE_T}&sv={ANTI_CODE_SDK_VERSION}\
         &sdk_sid={now_ms}&codec=264"
    ))
}

/// Streams from the web room page (`lol` rooms), following StreamGet's
/// `fetch_stream_url`: the first CDN line, fresh anti-leech parameters, and
/// `&ratio=` (empty for the original) on both URLs. Returns no variants when
/// the page lists no streams.
pub fn parse_web_streams(
    html: &str,
    seed: AntiCodeSeed,
) -> Result<Vec<StreamVariant>, PlatformError> {
    let start = html.find(WEB_STREAM_START).ok_or_else(|| {
        if html.trim().is_empty() {
            PlatformError::RateLimited("虎牙返回空页面，可能触发风控".to_string())
        } else {
            PlatformError::Parse("虎牙直播间页面缺少 stream 数据".to_string())
        }
    })?;
    let json_start = start + "stream: ".len();
    let end = html[json_start..]
        .find(WEB_STREAM_END)
        .ok_or_else(|| PlatformError::Parse("虎牙直播间页面 stream 数据不完整".to_string()))?;
    let stream: Value = serde_json::from_str(&format!("{}}}", &html[json_start..json_start + end]))
        .map_err(|error| PlatformError::Parse(format!("虎牙直播间页面数据无法解析: {error}")))?;

    let lines = stream
        .pointer("/data/0/gameStreamInfoList")
        .and_then(Value::as_array);
    let Some(line) = lines.and_then(|lines| lines.first()) else {
        return Ok(Vec::new());
    };
    let field = |key: &str| line.get(key).and_then(Value::as_str).unwrap_or_default();
    let stream_name = field("sStreamName");
    let flv_base = field("sFlvUrl");
    if stream_name.is_empty() || flv_base.is_empty() {
        return Err(PlatformError::Parse(
            "虎牙流地址缺少 sStreamName 或 sFlvUrl".to_string(),
        ));
    }
    let query = anti_code(field("sFlvAntiCode"), stream_name, seed)?;
    let suffix = |key: &str, default: &'static str| {
        Some(field(key))
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
    };
    let flv_url = stream_url(
        flv_base,
        stream_name,
        suffix("sFlvUrlSuffix", "flv"),
        &query,
    );
    let hls_url = Some(field("sHlsUrl"))
        .filter(|url| !url.is_empty())
        .map(|base| stream_url(base, stream_name, suffix("sHlsUrlSuffix", "m3u8"), &query));
    let rates = parse_bit_rates(stream.get("vMultiStreamInfo"));
    Ok(variants_for(
        &rates,
        &flv_url,
        hls_url.as_deref(),
        |bitrate| {
            if bitrate == 0 {
                "&ratio=".to_string()
            } else {
                format!("&ratio={bitrate}")
            }
        },
    ))
}

/// Turn a snapshot into `LiveInfo`, choosing the stream for `quality` by
/// padded position (original, then falling bit rate). `primary_reachable`
/// is the result of probing the chosen FLV URL; when it failed, the
/// neighbouring tier is used instead.
pub fn build_live_info(
    snapshot: RoomSnapshot,
    quality: Quality,
    primary_reachable: bool,
) -> LiveInfo {
    let count = snapshot.variants.len();
    let index = if primary_reachable {
        select_variant_index(count, quality)
    } else {
        fallback_variant_index(count, quality)
    };
    let selected = index
        .and_then(|index| snapshot.variants.get(index))
        .filter(|_| snapshot.is_live)
        .and_then(|variant| {
            // StreamGet: `record_url = flv_url or m3u8_url`.
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
        platform: Platform::Huya,
        live_url: Some(format!("{ROOM_PAGE}{}", snapshot.room_id)),
        room_id: snapshot.room_id,
        anchor_id: snapshot.anchor_id,
        anchor_name: snapshot.anchor_name,
        title: snapshot.title,
        is_live: snapshot.is_live && selected.is_some(),
        variants: snapshot.variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Huya streams. The CDN checks only the URL's
/// parameters; the PC User-Agent and room Referer match a browser. User
/// cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), PC_USER_AGENT.to_string()),
        ("Referer".to_string(), STREAM_REFERER.to_string()),
    ])
}

/// `pc_headers`, used for the API and the web room page, with the user's
/// cookie when set (StreamGet sends it on both).
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(PC_USER_AGENT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(PC_ACCEPT_LANGUAGE),
    );
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// `mobile_headers`, used to resolve letter ids.
pub fn mobile_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(MOBILE_USER_AGENT));
    headers.insert(
        HeaderName::from_static("xweb_xhr"),
        HeaderValue::from_static("1"),
    );
    headers.insert(REFERER, HeaderValue::from_static(MOBILE_REFERER));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(MOBILE_ACCEPT_LANGUAGE),
    );
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// Huya adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct HuyaAdapter {
    clients: ClientCache,
}

impl HuyaAdapter {
    pub fn new() -> Self {
        HuyaAdapter::default()
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
        decode_body(&response.bytes().await?)
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

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let room_number = if link.is_room_number() {
            link.id.clone()
        } else {
            let html =
                Self::get_text(&client, &link.normalized_url(), mobile_headers(cookie)?).await?;
            parse_alias_page(&html, &link.id)?
        };

        let mut api = Url::parse(PROFILE_ROOM_API).expect("valid profileRoom API URL");
        api.query_pairs_mut()
            .append_pair("m", "Live")
            .append_pair("do", "profileRoom")
            .append_pair("roomid", &room_number)
            .append_pair("showSecret", "1");
        let body = Self::get_text(&client, api.as_str(), pc_headers(cookie)?).await?;
        let ProfileRoom {
            mut snapshot,
            needs_web_page,
        } = parse_profile_room(&body)?;

        if needs_web_page {
            let page = format!("{ROOM_PAGE}{}", snapshot.room_id);
            let html = Self::get_text(&client, &page, pc_headers(cookie)?).await?;
            snapshot.variants = parse_web_streams(&html, AntiCodeSeed::now())?;
            snapshot.is_live = !snapshot.variants.is_empty();
        }

        let mut reachable = true;
        if snapshot.is_live
            && let Some(flv_url) = select_variant_index(snapshot.variants.len(), options.quality)
                .and_then(|index| snapshot.variants.get(index))
                .and_then(|variant| variant.flv_url.clone())
        {
            reachable = Self::probe(&client, &flv_url).await;
        }
        Ok(build_live_info(snapshot, options.quality, reachable))
    }
}

impl PlatformAdapter for HuyaAdapter {
    fn platform(&self) -> Platform {
        Platform::Huya
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

/// Response body as text. Some Huya pages (e.g. event rooms) come gzip
/// compressed even when the request does not accept it, and the shared HTTP
/// client has no decompression, so a gzip body is inflated here.
fn decode_body(bytes: &[u8]) -> Result<String, PlatformError> {
    let inflated;
    let bytes = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut text = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(bytes), &mut text).map_err(
            |error| PlatformError::Parse(format!("虎牙返回的压缩内容无法解压: {error}")),
        )?;
        inflated = text;
        &inflated[..]
    } else {
        bytes
    };
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// Decode standard or URL-safe base64, padding optional.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.trim().bytes().take_while(|byte| *byte != b'=') {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// MD5 digest as lowercase hex (RFC 1321). Only used for the anti-leech
/// parameters, which the CDN computes the same way.
fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in message.as_chunks::<64>().0 {
        let m: Vec<u32> = chunk
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word))
            .collect();
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(k[i])
                .wrapping_add(m[g])
                .rotate_left(S[i]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }
    state
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MP_LIVE: &str = include_str!("../../tests/fixtures/huya/mp_live.json");
    const MP_OFFLINE: &str = include_str!("../../tests/fixtures/huya/mp_offline.json");
    const MP_LOL: &str = include_str!("../../tests/fixtures/huya/mp_lol.json");
    const MP_NOT_FOUND: &str = include_str!("../../tests/fixtures/huya/mp_not_found.json");
    const WEB_LOL: &str = include_str!("../../tests/fixtures/huya/web_lol.html");
    const MOBILE_ALIAS: &str = include_str!("../../tests/fixtures/huya/mobile_alias.html");
    const MOBILE_NOT_FOUND: &str = include_str!("../../tests/fixtures/huya/mobile_not_found.html");

    /// Inputs and output computed with StreamGet's `get_anti_code`.
    const SEED: AntiCodeSeed = AntiCodeSeed {
        now_ms: 1_795_476_060_000,
        uid: 1_400_005_889_389,
        random_millis: 500,
    };
    const OLD_ANTI_CODE: &str = "wsSecret=7e561fef34b42ebacf0d5fb2ab5f31a1&wsTime=6b04cb7d&fm=RFdxOEJjSjNoNkRKdDZUWV8kMF8kMV8kMl8kMw%3D%3D&ctype=huya_live&fs=bgct";
    const STREAM_NAME: &str =
        "2205342170-2205342170-9471872496639672320-4410807796-10057-A-0-1-imgplus";

    fn names(variants: &[StreamVariant]) -> Vec<&str> {
        variants
            .iter()
            .map(|variant| variant.name.as_str())
            .collect()
    }

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let cases = [
            ("https://www.huya.com/52333", "52333"),
            ("  www.huya.com/52333?from=share ", "52333"),
            ("http://huya.com/52333/", "52333"),
            ("https://m.huya.com/52333#top", "52333"),
            ("https://WWW.HUYA.COM/lpl", "lpl"),
            ("https://www.huya.com/a_b-c", "a_b-c"),
        ];
        let adapter = HuyaAdapter::new();
        for (input, id) in cases {
            let link = parse_link(input).unwrap();
            assert_eq!(link.id, id, "{input}");
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some(format!("https://www.huya.com/{id}").as_str()),
                "{input}"
            );
            assert!(adapter.matches(input), "{input}");
        }
        assert!(
            parse_link("https://www.huya.com/52333")
                .unwrap()
                .is_room_number()
        );
        assert!(
            !parse_link("https://www.huya.com/lpl")
                .unwrap()
                .is_room_number()
        );
    }

    #[test]
    fn rejects_other_sites_and_non_room_links() {
        let adapter = HuyaAdapter::new();
        for input in [
            "https://www.huya.com/",
            "https://www.huya.com/g/lol",
            "https://www.huya.com/l",
            "https://www.huya.com/g",
            "https://www.huya.com/myfollow",
            "https://v.huya.com/play/123.html",
            "https://www.huya.com/cache.php",
            "https://www.huya.com/bad%20id",
            "https://huya.com.evil.example/52333",
            "ftp://www.huya.com/52333",
            "https://live.kuaishou.com/u/52333",
            "",
        ] {
            assert_eq!(
                parse_link(input).unwrap_err().kind(),
                "invalid_url",
                "{input}"
            );
            assert!(!adapter.matches(input), "{input}");
        }
        let huya = super::super::adapter_for("https://www.huya.com/52333").unwrap();
        assert_eq!(huya.platform(), Platform::Huya);
    }

    #[test]
    fn letter_ids_resolve_from_the_mobile_page() {
        assert_eq!(parse_alias_page(MOBILE_ALIAS, "lpl").unwrap(), "660000");
        // The first non-zero room wins over other rooms on the page.
        let html =
            r#"x"lProfileRoom":0,"sPrivateHost":"" y"lProfileRoom":18524915,"sPrivateHost":"s4k""#;
        assert_eq!(parse_alias_page(html, "s4k").unwrap(), "18524915");
        assert_eq!(
            parse_alias_page(MOBILE_NOT_FOUND, "nobody")
                .unwrap_err()
                .kind(),
            "invalid_url"
        );
        assert_eq!(
            parse_alias_page("   ", "x").unwrap_err().kind(),
            "rate_limited"
        );
        assert_eq!(
            parse_alias_page("<html>欢迎</html>", "x")
                .unwrap_err()
                .kind(),
            "parse"
        );
    }

    #[test]
    fn bit_rates_put_the_original_first_then_fall_by_bit_rate() {
        let rates = parse_bit_rates(Some(&Value::String(
            r#"[{"sDisplayName":"流畅","iBitRate":500},{"sDisplayName":"蓝光20M","iBitRate":0},
                {"sDisplayName":"蓝光8M","iBitRate":8000},{"sDisplayName":"超清","iBitRate":2000},
                {"sDisplayName":"重复","iBitRate":2000},
                {"sDisplayName":"2K HDR","iBitRate":14100,"iCodecType":1}]"#
                .to_string(),
        )));
        assert_eq!(
            rates
                .iter()
                .map(|rate| (rate.name.as_str(), rate.bitrate))
                .collect::<Vec<_>>(),
            [
                ("蓝光20M", 0),
                ("蓝光8M", 8000),
                ("超清", 2000),
                ("流畅", 500)
            ]
        );
        // No list, or a list without the original: the original is added.
        assert_eq!(
            parse_bit_rates(None),
            [BitRate {
                name: "原画".into(),
                bitrate: 0
            }]
        );
        let rates = parse_bit_rates(Some(
            &serde_json::json!([{"sDisplayName":"超清","iBitRate":2000}]),
        ));
        assert_eq!(
            rates.iter().map(|r| r.bitrate).collect::<Vec<_>>(),
            [0, 2000]
        );
    }

    #[test]
    fn live_room_uses_the_tx_line_with_web_player_parameters() {
        let room = parse_profile_room(MP_LIVE).unwrap();
        assert!(!room.needs_web_page);
        let snapshot = room.snapshot;
        assert!(snapshot.is_live);
        assert_eq!(snapshot.room_id, "10001");
        assert_eq!(snapshot.anchor_id.as_deref(), Some("100000001"));
        assert_eq!(snapshot.anchor_name, "测试主播");
        assert_eq!(snapshot.title.as_deref(), Some("测试直播标题"));
        assert_eq!(
            names(&snapshot.variants),
            ["蓝光20M", "蓝光8M", "蓝光4M", "超清", "流畅"]
        );
        assert_eq!(
            snapshot
                .variants
                .iter()
                .map(|v| v.quality)
                .collect::<Vec<_>>(),
            Quality::ALL.map(Some)
        );
        let stream = "100000001-100000001-1000000000000000001-200000002-10057-A-0-1-imgplus";
        let original = &snapshot.variants[0];
        let flv = original.flv_url.as_deref().unwrap();
        assert!(
            flv.starts_with(&format!(
                "http://tx.flv.huya.com/src/{stream}.flv?wsSecret="
            )),
            "{flv}"
        );
        assert!(flv.contains("&ctype=huya_webh5&fs=bgct&t=102"), "{flv}");
        assert!(!flv.contains("ratio="), "{flv}");
        let hls = original.hls_url.as_deref().unwrap();
        assert!(
            hls.starts_with(&format!("http://tx.hls.huya.com/src/{stream}.m3u8?")),
            "{hls}"
        );
        assert!(hls.contains("&ctype=huya_webh5"), "{hls}");
        for (variant, ratio) in snapshot.variants[1..].iter().zip([8000, 4000, 2000, 500]) {
            assert!(
                variant
                    .flv_url
                    .as_deref()
                    .unwrap()
                    .ends_with(&format!("&t=102&ratio={ratio}"))
            );
            assert!(
                variant
                    .hls_url
                    .as_deref()
                    .unwrap()
                    .ends_with(&format!("&ratio={ratio}"))
            );
        }
    }

    #[test]
    fn lines_other_than_tx_keep_their_parameters_and_bhct_is_switched_on_tx() {
        let without_tx = MP_LIVE.replace("\"TX\"", "\"XX\"");
        let snapshot = parse_profile_room(&without_tx).unwrap().snapshot;
        let flv = snapshot.variants[0].flv_url.as_deref().unwrap();
        assert!(flv.starts_with("http://al.flv.huya.com/src/"), "{flv}");
        assert!(flv.contains("&ctype=tars_mp&fs=bgct"), "{flv}");

        let bhct = MP_LIVE.replace("&fs=bgct", "&fs=bhct");
        let snapshot = parse_profile_room(&bhct).unwrap().snapshot;
        assert!(
            snapshot.variants[0]
                .flv_url
                .as_deref()
                .unwrap()
                .contains("&ctype=huya_webh5&fs=bgct")
        );
    }

    #[test]
    fn quality_picks_by_padded_position_and_records_flv() {
        let snapshot = parse_profile_room(MP_LIVE).unwrap().snapshot;
        let expected = [
            (Quality::Od, "蓝光20M"),
            (Quality::Uhd, "蓝光8M"),
            (Quality::Hd, "蓝光4M"),
            (Quality::Sd, "超清"),
            (Quality::Ld, "流畅"),
        ];
        for (quality, name) in expected {
            let info = build_live_info(snapshot.clone(), quality, true);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Huya);
            assert_eq!(info.live_url.as_deref(), Some("https://www.huya.com/10001"));
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert_eq!(selected.variant.name, name, "{quality}");
            assert_eq!(
                Some(&selected.record_url),
                selected.variant.flv_url.as_ref()
            );
            // Huya is not an FLV-preferred platform, but record_url is FLV.
            assert_eq!(
                selected.source_url(false, false).0,
                super::super::SourceFormat::Flv
            );
        }
        // Unreachable: one tier down, or up from the lowest.
        let pick = |quality| {
            build_live_info(snapshot.clone(), quality, false)
                .selected
                .unwrap()
                .variant
                .name
        };
        assert_eq!(pick(Quality::Od), "蓝光8M");
        assert_eq!(pick(Quality::Ld), "超清");

        // Without a bit-rate list only the original exists.
        let mut single = snapshot.clone();
        single.variants.truncate(1);
        let selected = build_live_info(single, Quality::Ld, true).selected.unwrap();
        assert_eq!(selected.variant.name, "蓝光20M");
    }

    #[test]
    fn offline_room_has_anchor_but_no_streams() {
        let room = parse_profile_room(MP_OFFLINE).unwrap();
        assert!(!room.needs_web_page);
        let info = build_live_info(room.snapshot, Quality::Od, true);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        assert_eq!(info.anchor_name, "离线主播");
        assert_eq!(info.room_id, "10003");
        assert_eq!(info.anchor_id.as_deref(), Some("100000003"));
        assert_eq!(info.title, None);
    }

    #[test]
    fn api_errors_map_to_error_kinds() {
        let error = parse_profile_room(MP_NOT_FOUND).unwrap_err();
        assert_eq!(error.kind(), "invalid_url");
        assert!(error.message().contains("该主播不存在"), "{error}");
        let cases = [
            ("", "rate_limited"),
            ("<html>blocked</html>", "rate_limited"),
            (
                r#"{"status":500,"message":"busy","data":[]}"#,
                "rate_limited",
            ),
            (r#"{"status":200,"message":"","data":[]}"#, "parse"),
            (
                r#"{"status":200,"data":{"profileInfo":{"nick":"x"}}}"#,
                "parse",
            ),
            (
                r#"{"status":200,"data":{"profileInfo":{"nick":"x","profileRoom":1},"realLiveStatus":"ON","liveData":{}}}"#,
                "parse",
            ),
        ];
        for (body, kind) in cases {
            assert_eq!(parse_profile_room(body).unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn lol_rooms_read_streams_from_the_web_page_with_fresh_anti_codes() {
        let room = parse_profile_room(MP_LOL).unwrap();
        assert!(room.needs_web_page);
        assert!(room.snapshot.is_live);
        assert!(room.snapshot.variants.is_empty());
        assert_eq!(room.snapshot.room_id, "10002");
        assert_eq!(room.snapshot.title.as_deref(), Some("联盟测试标题"));

        let variants = parse_web_streams(WEB_LOL, SEED).unwrap();
        assert_eq!(
            names(&variants),
            ["蓝光20M", "蓝光8M", "蓝光4M", "超清", "流畅"]
        );
        let stream = "100000002-100000002-1000000000000000002-200000004-10057-A-0-1-imgplus";
        let original = variants[0].flv_url.as_deref().unwrap();
        // StreamGet uses the first line (AL) on this path.
        assert!(original.starts_with(&format!(
            "http://al.flv.huya.com/src/{stream}.flv?wsSecret="
        )));
        assert!(
            original.contains("&wsTime=6b04caca&seqid=3195481949389&ctype=huya_live&ver=1&fs=bgct")
        );
        assert!(
            original.ends_with("&sdk_sid=1795476060000&codec=264&ratio="),
            "{original}"
        );
        assert!(
            variants[0]
                .hls_url
                .as_deref()
                .unwrap()
                .starts_with("http://al.hls.huya.com/src/")
        );
        assert!(
            variants[3]
                .flv_url
                .as_deref()
                .unwrap()
                .ends_with("&codec=264&ratio=2000")
        );

        let mut snapshot = room.snapshot;
        snapshot.variants = variants;
        let info = build_live_info(snapshot, Quality::Od, true);
        assert!(info.is_live);
        assert_eq!(info.selected.unwrap().variant.name, "蓝光20M");

        // No stream lines on the page: not live.
        let empty = WEB_LOL.replacen(
            "\"gameStreamInfoList\":[",
            "\"gameStreamInfoList\":[],\"x\":[",
            1,
        );
        assert!(parse_web_streams(&empty, SEED).unwrap().is_empty());
        assert_eq!(
            parse_web_streams("", SEED).unwrap_err().kind(),
            "rate_limited"
        );
        assert_eq!(
            parse_web_streams("<html></html>", SEED).unwrap_err().kind(),
            "parse"
        );
    }

    #[test]
    fn anti_code_matches_streamget() {
        assert_eq!(
            anti_code(OLD_ANTI_CODE, STREAM_NAME, SEED).unwrap(),
            "wsSecret=aed203aec48321acae8aa35ef19f77b4&wsTime=6b04caca&seqid=3195481949389\
             &ctype=huya_live&ver=1&fs=bgct&uuid=4271666670&u=1400005889389&t=100\
             &sv=2403051612&sdk_sid=1795476060000&codec=264"
        );
        let other = AntiCodeSeed {
            now_ms: 1_700_000_000_000,
            uid: 1_400_000_000_000,
            random_millis: 0,
        };
        assert_eq!(
            anti_code(
                &format!("{OLD_ANTI_CODE}&exsphd=264_500,264_2000,"),
                "abc",
                other
            )
            .unwrap(),
            "wsSecret=42928e50a2f15394b3c591323bef319e&wsTime=6553f16e&seqid=3100000000000\
             &ctype=huya_live&ver=1&fs=bgct&uuid=0&u=1400000000000&t=100\
             &sv=2403051612&sdk_sid=1700000000000&codec=264"
        );
        assert_eq!(
            anti_code("wsSecret=1&ctype=a&fs=b", "x", SEED)
                .unwrap_err()
                .kind(),
            "parse"
        );
        assert_eq!(
            anti_code("fm=%%%&ctype=a&fs=b", "x", SEED)
                .unwrap_err()
                .kind(),
            "parse"
        );
    }

    #[test]
    fn random_seed_stays_in_streamget_ranges() {
        for _ in 0..50 {
            let seed = AntiCodeSeed::now();
            assert!((1_400_000_000_000..=1_400_009_999_999).contains(&seed.uid));
            assert!(seed.random_millis < 1000);
            assert_eq!(seed.now_ms % 1000, 0);
        }
    }

    #[test]
    fn gzip_bodies_are_inflated_and_plain_bodies_kept() {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(WEB_LOL.as_bytes()).unwrap();
        let gzipped = encoder.finish().unwrap();
        let html = decode_body(&gzipped).unwrap();
        assert_eq!(html, WEB_LOL);
        assert_eq!(parse_web_streams(&html, SEED).unwrap().len(), 5);
        assert_eq!(decode_body("虎牙".as_bytes()).unwrap(), "虎牙");
        assert_eq!(decode_body(&[0x1f, 0x8b, 0]).unwrap_err().kind(), "parse");
    }

    #[test]
    fn md5_and_base64_match_reference_values() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        assert_eq!(
            md5_hex("中文".as_bytes()),
            "a7bac2239fcdcb3a067903d8077c4a07"
        );
        assert_eq!(md5_hex(&[b'a'; 64]), "014842d480b571495a4a0363793f7367");
        assert_eq!(
            base64_decode("RFdxOEJjSjNoNkRKdDZUWV8kMF8kMV8kMl8kMw==").unwrap(),
            b"DWq8BcJ3h6DJt6TY_$0_$1_$2_$3"
        );
        assert_eq!(base64_decode("YWI").unwrap(), b"ab");
        assert!(base64_decode("a*b").is_none());
    }

    #[test]
    fn headers_carry_cookies_only_to_huya_pages() {
        let info = build_live_info(
            parse_profile_room(MP_LIVE).unwrap().snapshot,
            Quality::Od,
            true,
        );
        assert!(
            info.stream_headers
                .keys()
                .all(|key| !key.eq_ignore_ascii_case("cookie"))
        );
        assert_eq!(
            info.stream_headers.get("Referer").map(String::as_str),
            Some("https://www.huya.com/")
        );
        assert!(pc_headers(None).unwrap().get(COOKIE).is_none());
        assert_eq!(pc_headers(Some("a=1")).unwrap().get(COOKIE).unwrap(), "a=1");
        assert_eq!(mobile_headers(None).unwrap().get("xweb_xhr").unwrap(), "1");
        assert_eq!(pc_headers(Some("a=1\r\nb=2")).unwrap_err().kind(), "config");
    }
}
