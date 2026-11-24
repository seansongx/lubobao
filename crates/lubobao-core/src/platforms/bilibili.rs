//! Bilibili live adapter, following StreamCap's `BilibiliHandler` and StreamGet
//! v4.0.10 `BilibiliLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, commit
//! `e57e548`), `streamget/platforms/bilibili/live_stream.py`, MIT License,
//! Copyright (c) 2025 Hmily. The request flow, headers, default cookie and
//! the `qn` quality mapping follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * `room/v1/Room/room_init?id={id}` resolves a short or long room id into
//!   the real room id, the anchor's uid and `live_status` (1 = live; 0 = off
//!   air; 2 = 轮播, which StreamGet also treats as offline).
//! * `xlive/web-room/v1/index/getH5InfoByRoom` gives the title and the
//!   anchor name; `live_user/v1/Master/info` is the fallback for the name.
//! * When live, `room/v1/Room/playUrl?cid=…&qn=…&platform=web` returns the
//!   FLV URLs for the requested `qn`. StreamGet prefers a `d1--cn-gotcha`
//!   host, otherwise the last `durl` entry. If that API fails it falls back
//!   to `xlive/web-room/v2/index/getRoomPlayInfo`.
//!
//! Without a logged-in cookie Bilibili caps guests at 超清 (`qn=250`): the
//! API answers every higher request with the 250 stream.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{
    ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT,
};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header, user_cookie,
};

/// `BilibiliLiveStream._get_pc_headers` user agent, also used to pull streams.
pub const PC_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:109.0) Gecko/20100101 Firefox/115.0";
const ACCEPT_LANGUAGE_VALUE: &str = "zh-CN,zh;q=0.8,zh-TW;q=0.7,zh-HK;q=0.5,en-US;q=0.3,en;q=0.2";
/// StreamGet's cookie when the user has none.
const DEFAULT_COOKIE: &str = "__ac_nonce=064caded4009deafd8b89;";
const ORIGIN_VALUE: &str = "https://live.bilibili.com";
/// `_get_pc_headers` referer for API requests.
const API_REFERER: &str = "https://live.bilibili.com/26066074";
/// Referer sent when pulling streams.
const STREAM_REFERER: &str = "https://live.bilibili.com/";

const ROOM_PAGE: &str = "https://live.bilibili.com/";
const SHARE_PAGE: &str = "https://b23.tv/";
const ROOM_INIT_API: &str = "https://api.live.bilibili.com/room/v1/Room/room_init";
const H5_INFO_API: &str = "https://api.live.bilibili.com/xlive/web-room/v1/index/getH5InfoByRoom";
const MASTER_INFO_API: &str = "https://api.live.bilibili.com/live_user/v1/Master/info";
const PLAY_URL_API: &str = "https://api.live.bilibili.com/room/v1/Room/playUrl";
const ROOM_PLAY_INFO_API: &str =
    "https://api.live.bilibili.com/xlive/web-room/v2/index/getRoomPlayInfo";

/// StreamGet's `video_quality_options` for the tiers StreamCap offers
/// (`OD=10000, UHD=250, HD=150, SD=80, LD=80`). StreamGet's extra
/// `BD: 400` (蓝光) is not reachable from StreamCap.
pub fn qn_for(quality: Quality) -> u32 {
    match quality {
        Quality::Od => 10000,
        Quality::Uhd => 250,
        Quality::Hd => 150,
        Quality::Sd | Quality::Ld => 80,
    }
}

/// Best tier that requests this `qn`, used to label variants.
fn quality_for_qn(qn: u32) -> Option<Quality> {
    Quality::ALL
        .into_iter()
        .find(|quality| qn_for(*quality) == qn)
}

/// A recognized Bilibili room link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BilibiliLink {
    /// Room id in the link: the short id (`320`) or the long id (`286893`).
    pub room_id: String,
}

impl BilibiliLink {
    /// Canonical URL for the link.
    pub fn normalized_url(&self) -> String {
        format!("{ROOM_PAGE}{}", self.room_id)
    }
}

/// Recognize and normalize a Bilibili live link without any network access.
///
/// StreamCap registers `https://live.bilibili.com/` and StreamGet takes the
/// last path segment as the room id. Accepted: `live.bilibili.com/{id}` and
/// the `h5/{id}` and `blanc/{id}` page forms, with a numeric id; query
/// strings and fragments are dropped.
pub fn parse_link(input: &str) -> Result<BilibiliLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的 B站直播间链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "live.bilibili.com" {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    let id = match segments.as_slice() {
        [id] | ["h5" | "blanc", id] => *id,
        _ => return Err(invalid()),
    };
    if id.is_empty() || id.len() > 16 || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    // `live.bilibili.com/0320` and `/320` are the same room.
    let id = id.trim_start_matches('0');
    if id.is_empty() {
        return Err(invalid());
    }
    Ok(BilibiliLink {
        room_id: id.to_string(),
    })
}

/// Code of a share short link `b23.tv/{code}` (an intentional extension over
/// StreamCap; the room is only known after the redirect).
pub fn parse_share_link(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = Url::parse(&with_scheme).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str()?.to_ascii_lowercase().as_str(),
            "b23.tv" | "www.b23.tv"
        )
    {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        [code] if code.chars().all(|c| c.is_ascii_alphanumeric()) => Some(code.to_string()),
        _ => None,
    }
}

/// Parse a JSON API body, classifying non-JSON answers.
fn parse_json(body: &str, api: &str) -> Result<Value, PlatformError> {
    serde_json::from_str::<Value>(body).map_err(|_| {
        let trimmed = body.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('<') {
            PlatformError::RateLimited(format!("B站 {api} 返回的不是 JSON，可能触发风控"))
        } else {
            PlatformError::Parse(format!("B站 {api} 返回内容无法解析"))
        }
    })
}

/// Map a non-zero `code` from a Bilibili API to an error kind.
fn classify_api_code(code: i64, message: &str) -> PlatformError {
    let detail = if message.is_empty() {
        format!("B站接口返回错误 {code}")
    } else {
        format!("B站接口返回错误 {code}：{message}")
    };
    match code {
        // 房间不存在 / 房间信息不存在 / 获取初始化数据失败（不存在的房间）。
        60004 | 19002003 => PlatformError::InvalidUrl(format!("B站直播间不存在（{detail}）")),
        -101 => PlatformError::AuthRequired(detail),
        -352 | -412 | -509 | -799 => PlatformError::RateLimited(detail),
        _ => PlatformError::RateLimited(detail),
    }
}

fn api_code(json: &Value) -> i64 {
    json.get("code").and_then(Value::as_i64).unwrap_or(-1)
}

fn api_message(json: &Value) -> &str {
    json.get("message")
        .or_else(|| json.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn as_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// Result of `room_init`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomInit {
    /// Real (long) room id.
    pub room_id: u64,
    /// Short id, 0 when the room has none.
    pub short_id: u64,
    pub uid: u64,
    /// `live_status == 1`; 2 (轮播) counts as offline, as in StreamGet.
    pub is_live: bool,
}

/// Parse a `room_init` response.
pub fn parse_room_init(body: &str) -> Result<RoomInit, PlatformError> {
    let json = parse_json(body, "room_init")?;
    let code = api_code(&json);
    if code != 0 {
        return Err(classify_api_code(code, api_message(&json)));
    }
    let data = json.get("data").unwrap_or(&Value::Null);
    let room_id = as_u64(data.get("room_id"))
        .filter(|id| *id > 0)
        .ok_or_else(|| PlatformError::Parse("room_init 缺少 room_id".to_string()))?;
    let uid = as_u64(data.get("uid"))
        .filter(|uid| *uid > 0)
        .ok_or_else(|| PlatformError::Parse("room_init 缺少主播 uid".to_string()))?;
    if data.get("is_locked").and_then(Value::as_bool) == Some(true) {
        return Err(PlatformError::InvalidUrl(
            "B站直播间已被封禁（锁定）".to_string(),
        ));
    }
    if data.get("encrypted").and_then(Value::as_bool) == Some(true)
        && data.get("pwd_verified").and_then(Value::as_bool) != Some(true)
    {
        return Err(PlatformError::AuthRequired(
            "B站直播间设置了密码，无法检测".to_string(),
        ));
    }
    Ok(RoomInit {
        room_id,
        short_id: as_u64(data.get("short_id")).unwrap_or(0),
        uid,
        is_live: data.get("live_status").and_then(Value::as_i64) == Some(1),
    })
}

/// Title and anchor name from `getH5InfoByRoom`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomDetails {
    pub title: Option<String>,
    pub anchor_name: Option<String>,
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Parse a `getH5InfoByRoom` response. Failures only lose the details.
pub fn parse_h5_info(body: &str) -> RoomDetails {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return RoomDetails::default();
    };
    if api_code(&json) != 0 {
        return RoomDetails::default();
    }
    RoomDetails {
        title: non_empty(json.pointer("/data/room_info/title")),
        anchor_name: non_empty(json.pointer("/data/anchor_info/base_info/uname")),
    }
}

/// Parse a `Master/info` response into the anchor name.
pub fn parse_master_info(body: &str) -> Result<String, PlatformError> {
    let json = parse_json(body, "Master/info")?;
    let code = api_code(&json);
    if code != 0 {
        return Err(classify_api_code(code, api_message(&json)));
    }
    non_empty(json.pointer("/data/info/uname"))
        .ok_or_else(|| PlatformError::Parse("B站主播信息缺少昵称".to_string()))
}

/// One stream answer from `playUrl` or `getRoomPlayInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayStream {
    /// `qn` the API actually served (guests get at most 250).
    pub current_qn: u32,
    /// `qn` values the room offers with their descriptions, best first.
    pub qualities: Vec<(u32, String)>,
    pub url: String,
}

fn qualities_from(list: Option<&Value>, qn_key: &str) -> Vec<(u32, String)> {
    let mut qualities: Vec<(u32, String)> = list
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| {
                    let qn = u32::try_from(as_u64(entry.get(qn_key))?).ok()?;
                    let desc = entry
                        .get("desc")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some((qn, desc))
                })
                .collect()
        })
        .unwrap_or_default();
    qualities.sort_by_key(|(qn, _)| std::cmp::Reverse(*qn));
    qualities.dedup_by_key(|(qn, _)| *qn);
    qualities
}

/// Parse a `playUrl` response. `Ok(None)` means the API refused (non-zero
/// code or no `durl`); StreamGet then tries `getRoomPlayInfo`.
pub fn parse_play_url(body: &str) -> Result<Option<PlayStream>, PlatformError> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Ok(None);
    };
    if api_code(&json) != 0 {
        return Ok(None);
    }
    let data = json.get("data").unwrap_or(&Value::Null);
    let urls: Vec<&str> = data
        .get("durl")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| entry.get("url").and_then(Value::as_str))
                .filter(|url| !url.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // StreamGet: the first `d1--cn-gotcha` URL, else the last one.
    let Some(url) = urls
        .iter()
        .find(|url| url.contains("d1--cn-gotcha"))
        .or_else(|| urls.last())
    else {
        return Ok(None);
    };
    let current_qn = as_u64(data.get("current_qn"))
        .and_then(|qn| u32::try_from(qn).ok())
        .unwrap_or(0);
    Ok(Some(PlayStream {
        current_qn,
        qualities: qualities_from(data.get("quality_description"), "qn"),
        url: url.to_string(),
    }))
}

/// Parse a `getRoomPlayInfo` response. `Ok(None)` means the room is offline.
///
/// StreamGet takes `stream[0].format[0].codec`, sorts it by `current_qn` and
/// indexes it by quality tier, which mixes H.264 and HEVC. This picks the
/// H.264 (`avc`) FLV stream instead, then any FLV, then an HLS `ts` stream.
pub fn parse_room_play_info(body: &str) -> Result<Option<PlayStream>, PlatformError> {
    let json = parse_json(body, "getRoomPlayInfo")?;
    let code = api_code(&json);
    if code != 0 {
        return Err(classify_api_code(code, api_message(&json)));
    }
    let data = json.get("data").unwrap_or(&Value::Null);
    if data.get("live_status").and_then(Value::as_i64) != Some(1) {
        return Ok(None);
    }
    let playurl = data
        .pointer("/playurl_info/playurl")
        .ok_or_else(|| PlatformError::Parse("直播中但没有返回播放地址".to_string()))?;
    let streams = playurl
        .get("stream")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let codecs = |protocol: &str, format: &str| -> Vec<&Value> {
        streams
            .iter()
            .filter(|stream| stream.get("protocol_name").and_then(Value::as_str) == Some(protocol))
            .flat_map(|stream| {
                stream
                    .get("format")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
            })
            .filter(|entry| entry.get("format_name").and_then(Value::as_str) == Some(format))
            .flat_map(|entry| {
                entry
                    .get("codec")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
            })
            .collect()
    };
    let is_avc = |codec: &&Value| codec.get("codec_name").and_then(Value::as_str) == Some("avc");
    let flv = codecs("http_stream", "flv");
    let ts = codecs("http_hls", "ts");
    let codec = flv
        .iter()
        .find(|codec| is_avc(codec))
        .or_else(|| flv.first())
        .or_else(|| ts.iter().find(|codec| is_avc(codec)))
        .or_else(|| ts.first())
        .ok_or_else(|| PlatformError::Parse("直播中但没有找到可用的流".to_string()))?;
    let base_url = codec
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let url_info = codec.pointer("/url_info/0").unwrap_or(&Value::Null);
    let host = url_info
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let extra = url_info
        .get("extra")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if base_url.is_empty() || host.is_empty() {
        return Err(PlatformError::Parse(
            "播放地址缺少 host 或 base_url".to_string(),
        ));
    }
    let current_qn = as_u64(codec.get("current_qn"))
        .and_then(|qn| u32::try_from(qn).ok())
        .unwrap_or(0);
    // Room's own qualities: `accept_qn`, named through `g_qn_desc`.
    let names = qualities_from(playurl.get("g_qn_desc"), "qn");
    let mut qualities: Vec<(u32, String)> = codec
        .get("accept_qn")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|qn| u32::try_from(qn.as_u64()?).ok())
                .map(|qn| {
                    let desc = names
                        .iter()
                        .find(|(known, _)| *known == qn)
                        .map(|(_, desc)| desc.clone())
                        .unwrap_or_default();
                    (qn, desc)
                })
                .collect()
        })
        .unwrap_or_default();
    qualities.sort_by_key(|(qn, _)| std::cmp::Reverse(*qn));
    qualities.dedup_by_key(|(qn, _)| *qn);
    Ok(Some(PlayStream {
        current_qn,
        qualities,
        url: format!("{host}{base_url}{extra}"),
    }))
}

/// Room state before the stream is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSnapshot {
    pub init: RoomInit,
    /// Room id from the link, used for `live_url` as StreamGet does.
    pub link_room_id: String,
    pub anchor_name: String,
    pub title: Option<String>,
}

fn is_hls(url: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .is_some_and(|path| path.ends_with(".m3u8"))
}

/// Variants for a stream answer: every `qn` the room offers, best first.
/// Only the served `qn` carries a URL; the API returns one stream per request.
pub fn build_variants(stream: &PlayStream) -> (Vec<StreamVariant>, usize) {
    let mut qualities = stream.qualities.clone();
    if !qualities.iter().any(|(qn, _)| *qn == stream.current_qn) {
        qualities.push((stream.current_qn, String::new()));
        qualities.sort_by_key(|(qn, _)| std::cmp::Reverse(*qn));
    }
    let mut selected = 0;
    let variants = qualities
        .iter()
        .enumerate()
        .map(|(index, (qn, desc))| {
            let served = *qn == stream.current_qn;
            if served {
                selected = index;
            }
            let url = served.then(|| stream.url.clone());
            let hls = is_hls(&stream.url);
            StreamVariant {
                name: if desc.is_empty() {
                    qn.to_string()
                } else {
                    desc.clone()
                },
                quality: quality_for_qn(*qn),
                flv_url: url.clone().filter(|_| !hls),
                hls_url: url.filter(|_| hls),
            }
        })
        .collect();
    (variants, selected)
}

/// Turn a snapshot and the stream answer into `LiveInfo`. `stream` is `None`
/// when the room is offline.
pub fn build_live_info(
    snapshot: RoomSnapshot,
    stream: Option<PlayStream>,
    quality: Quality,
) -> LiveInfo {
    let (variants, selected) = match stream.filter(|_| snapshot.init.is_live) {
        Some(stream) => {
            let (variants, index) = build_variants(&stream);
            let selected = variants.get(index).map(|variant| SelectedStream {
                requested: quality,
                variant: variant.clone(),
                // StreamGet: `record_url = play_url`.
                record_url: stream.url.clone(),
            });
            (variants, selected)
        }
        None => (Vec::new(), None),
    };
    LiveInfo {
        platform: Platform::Bilibili,
        room_id: snapshot.init.room_id.to_string(),
        anchor_id: Some(snapshot.init.uid.to_string()),
        live_url: Some(format!("{ROOM_PAGE}{}", snapshot.link_room_id)),
        anchor_name: snapshot.anchor_name,
        title: snapshot.title,
        is_live: selected.is_some(),
        variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Bilibili streams: the PC User-Agent and the live
/// Referer. User cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), PC_USER_AGENT.to_string()),
        ("Referer".to_string(), STREAM_REFERER.to_string()),
    ])
}

/// `BilibiliLiveStream.pc_headers` for API requests: the user's cookie, or
/// StreamGet's default cookie when none is set.
pub fn api_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(PC_USER_AGENT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
    );
    headers.insert(ORIGIN, HeaderValue::from_static(ORIGIN_VALUE));
    headers.insert(REFERER, HeaderValue::from_static(API_REFERER));
    headers.insert(
        COOKIE,
        match cookie {
            Some(cookie) => cookie_header(cookie)?,
            None => HeaderValue::from_static(DEFAULT_COOKIE),
        },
    );
    Ok(headers)
}

/// Bilibili adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct BilibiliAdapter {
    clients: ClientCache,
}

impl BilibiliAdapter {
    pub fn new() -> Self {
        BilibiliAdapter::default()
    }

    async fn get_text(
        client: &reqwest::Client,
        url: &Url,
        headers: HeaderMap,
    ) -> Result<String, PlatformError> {
        let response = client.get(url.as_str()).headers(headers).send().await?;
        let status = response.status();
        if !status.is_success() {
            // Bilibili answers risk control with HTTP 412.
            if status.as_u16() == 412 {
                return Err(PlatformError::RateLimited(format!(
                    "HTTP 412: B站触发风控 {}",
                    url.path()
                )));
            }
            return Err(classify_http_status(status.as_u16(), url.path()));
        }
        Ok(response.text().await?)
    }

    fn api(base: &str, query: &[(&str, &str)]) -> Url {
        let mut url = Url::parse(base).expect("valid Bilibili API URL");
        url.query_pairs_mut().extend_pairs(query);
        url
    }

    /// Room behind a `b23.tv` share link.
    async fn resolve_share_link(
        code: &str,
        options: &FetchOptions,
    ) -> Result<BilibiliLink, PlatformError> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(PC_USER_AGENT));
        let target = super::follow_short_link(
            options.proxy.as_deref(),
            &format!("{SHARE_PAGE}{code}"),
            headers,
            |url| parse_link(url.as_str()).is_ok(),
        )
        .await?;
        parse_link(&target).map_err(|_| {
            PlatformError::InvalidUrl(format!(
                "B站分享链接没有指向直播间，请检查链接是否失效: {target}"
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
        let headers = api_headers(user_cookie(options))?;

        let body = Self::get_text(
            &client,
            &Self::api(ROOM_INIT_API, &[("id", &link.room_id)]),
            headers.clone(),
        )
        .await?;
        let init = parse_room_init(&body)?;
        let room_id = init.room_id.to_string();

        let details = match Self::get_text(
            &client,
            &Self::api(H5_INFO_API, &[("room_id", &room_id)]),
            headers.clone(),
        )
        .await
        {
            Ok(body) => parse_h5_info(&body),
            Err(_) => RoomDetails::default(),
        };
        let anchor_name = match details.anchor_name {
            Some(name) => name,
            None => {
                let body = Self::get_text(
                    &client,
                    &Self::api(MASTER_INFO_API, &[("uid", &init.uid.to_string())]),
                    headers.clone(),
                )
                .await?;
                parse_master_info(&body)?
            }
        };

        let stream = if init.is_live {
            Self::fetch_stream(&client, &room_id, options.quality, headers).await?
        } else {
            None
        };
        let snapshot = RoomSnapshot {
            init,
            link_room_id: link.room_id,
            anchor_name,
            title: details.title,
        };
        Ok(build_live_info(snapshot, stream, options.quality))
    }

    async fn fetch_stream(
        client: &reqwest::Client,
        room_id: &str,
        quality: Quality,
        headers: HeaderMap,
    ) -> Result<Option<PlayStream>, PlatformError> {
        let qn = qn_for(quality).to_string();
        let play_url = Self::api(
            PLAY_URL_API,
            &[("cid", room_id), ("qn", &qn), ("platform", "web")],
        );
        if let Ok(body) = Self::get_text(client, &play_url, headers.clone()).await
            && let Some(stream) = parse_play_url(&body)?
        {
            return Ok(Some(stream));
        }
        let play_info = Self::api(
            ROOM_PLAY_INFO_API,
            &[
                ("room_id", room_id),
                ("protocol", "0,1"),
                ("format", "0,1,2"),
                ("codec", "0,1,2"),
                ("qn", &qn),
                ("platform", "web"),
                ("ptype", "8"),
                ("dolby", "5"),
                ("panorama", "1"),
                ("hdr_type", "0,1"),
            ],
        );
        let body = Self::get_text(client, &play_info, headers).await?;
        parse_room_play_info(&body)
    }
}

impl PlatformAdapter for BilibiliAdapter {
    fn platform(&self) -> Platform {
        Platform::Bilibili
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
    use crate::platforms::SourceFormat;

    const ROOM_INIT_LIVE: &str = include_str!("../../tests/fixtures/bilibili/room_init_live.json");
    const ROOM_INIT_OFFLINE: &str =
        include_str!("../../tests/fixtures/bilibili/room_init_offline.json");
    const ROOM_INIT_RERUN: &str =
        include_str!("../../tests/fixtures/bilibili/room_init_rerun.json");
    const ROOM_INIT_MISSING: &str =
        include_str!("../../tests/fixtures/bilibili/room_init_missing.json");
    const H5_INFO: &str = include_str!("../../tests/fixtures/bilibili/h5_info.json");
    const MASTER_INFO: &str = include_str!("../../tests/fixtures/bilibili/master_info.json");
    const PLAY_URL_GUEST: &str = include_str!("../../tests/fixtures/bilibili/play_url_guest.json");
    const PLAY_URL_CN_GOTCHA: &str =
        include_str!("../../tests/fixtures/bilibili/play_url_cn_gotcha.json");
    const PLAY_URL_MISSING: &str =
        include_str!("../../tests/fixtures/bilibili/play_url_missing.json");
    const ROOM_PLAY_INFO_LIVE: &str =
        include_str!("../../tests/fixtures/bilibili/room_play_info_live.json");
    const ROOM_PLAY_INFO_OFFLINE: &str =
        include_str!("../../tests/fixtures/bilibili/room_play_info_offline.json");

    fn snapshot(init: &str) -> RoomSnapshot {
        RoomSnapshot {
            init: parse_room_init(init).unwrap(),
            link_room_id: "321".to_string(),
            anchor_name: "测试主播".to_string(),
            title: Some("测试直播标题".to_string()),
        }
    }

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let cases = [
            ("https://live.bilibili.com/320", "320"),
            ("  live.bilibili.com/320?visit_id=abc&spm_id_from=x ", "320"),
            ("http://LIVE.bilibili.com/286893/", "286893"),
            ("https://live.bilibili.com/h5/320", "320"),
            ("https://live.bilibili.com/blanc/320#top", "320"),
            ("https://live.bilibili.com/0320", "320"),
        ];
        let adapter = BilibiliAdapter::new();
        for (input, id) in cases {
            let link = parse_link(input).unwrap();
            assert_eq!(link.room_id, id, "{input}");
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some(format!("https://live.bilibili.com/{id}").as_str()),
                "{input}"
            );
            assert!(adapter.matches(input), "{input}");
        }
    }

    #[test]
    fn share_links_are_recognized_offline() {
        let adapter = BilibiliAdapter::new();
        for input in [
            "https://b23.tv/oolIxoB",
            "b23.tv/oolIxoB/",
            "http://B23.tv/oolIxoB?x=1",
        ] {
            assert_eq!(
                parse_share_link(input).as_deref(),
                Some("oolIxoB"),
                "{input}"
            );
            assert!(adapter.matches(input), "{input}");
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some("https://b23.tv/oolIxoB"),
                "{input}"
            );
        }
        // Where the share link above redirects; only room links are taken.
        let target = "https://live.bilibili.com/21452505?plat_id=124&share_from=h5&share_source=COPY&unique_k=oolIxoB";
        assert_eq!(parse_link(target).unwrap().room_id, "21452505");
        assert!(parse_link("https://www.bilibili.com/video/BV1xx411c7mD").is_err());
    }

    #[test]
    fn rejects_other_sites_and_non_room_links() {
        let adapter = BilibiliAdapter::new();
        for input in [
            "https://live.bilibili.com/",
            "https://live.bilibili.com/p/eden/area-tags",
            "https://live.bilibili.com/abc",
            "https://live.bilibili.com/0",
            "https://live.bilibili.com/320/extra",
            "https://www.bilibili.com/320",
            "https://space.bilibili.com/13763303",
            "https://b23.tv/",
            "https://b23.tv/a/b",
            "ftp://live.bilibili.com/320",
            "https://live.kuaishou.com/u/320",
            "",
        ] {
            assert_eq!(
                parse_link(input).unwrap_err().kind(),
                "invalid_url",
                "{input}"
            );
            assert!(!adapter.matches(input), "{input}");
        }
        let bilibili = super::super::adapter_for("https://live.bilibili.com/320").unwrap();
        assert_eq!(bilibili.platform(), Platform::Bilibili);
    }

    #[test]
    fn room_init_reads_ids_and_live_status() {
        let live = parse_room_init(ROOM_INIT_LIVE).unwrap();
        assert_eq!(
            live,
            RoomInit {
                room_id: 100001,
                short_id: 321,
                uid: 200001,
                is_live: true,
            }
        );
        assert!(!parse_room_init(ROOM_INIT_OFFLINE).unwrap().is_live);
        // 轮播 (live_status 2) is offline, as in StreamGet.
        assert!(!parse_room_init(ROOM_INIT_RERUN).unwrap().is_live);
    }

    #[test]
    fn missing_room_and_api_errors_map_to_error_kinds() {
        let missing = parse_room_init(ROOM_INIT_MISSING).unwrap_err();
        assert_eq!(missing.kind(), "invalid_url");
        assert!(missing.message().contains("不存在"), "{missing}");
        let cases = [
            (r#"{"code":-352,"message":"-352","ttl":1}"#, "rate_limited"),
            (r#"{"code":-412,"message":"请求被拦截"}"#, "rate_limited"),
            (r#"{"code":-101,"message":"账号未登录"}"#, "auth_required"),
            (r#"{"code":12345,"message":"未知"}"#, "rate_limited"),
            ("<html>412</html>", "rate_limited"),
            ("", "rate_limited"),
            (r#"{"code":0,"data":{"uid":1}}"#, "parse"),
            ("{bad", "parse"),
            (
                r#"{"code":0,"data":{"room_id":1,"uid":2,"is_locked":true,"live_status":0}}"#,
                "invalid_url",
            ),
            (
                r#"{"code":0,"data":{"room_id":1,"uid":2,"encrypted":true,"pwd_verified":false}}"#,
                "auth_required",
            ),
        ];
        for (body, kind) in cases {
            assert_eq!(parse_room_init(body).unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn anchor_name_and_title_come_from_h5_info_or_master_info() {
        assert_eq!(
            parse_h5_info(H5_INFO),
            RoomDetails {
                title: Some("测试直播标题".to_string()),
                anchor_name: Some("测试主播".to_string()),
            }
        );
        assert_eq!(
            parse_h5_info(r#"{"code":19002000,"message":"获取初始化数据失败","data":null}"#),
            RoomDetails::default()
        );
        assert_eq!(parse_h5_info("<html>"), RoomDetails::default());
        assert_eq!(parse_master_info(MASTER_INFO).unwrap(), "测试主播");
        assert_eq!(
            parse_master_info(r#"{"code":0,"data":{"info":{}}}"#)
                .unwrap_err()
                .kind(),
            "parse"
        );
    }

    #[test]
    fn play_url_prefers_cn_gotcha_then_the_last_url() {
        let guest = parse_play_url(PLAY_URL_GUEST).unwrap().unwrap();
        assert_eq!(guest.current_qn, 250);
        assert!(
            guest.url.starts_with("https://d1--ov-gotcha05b."),
            "{}",
            guest.url
        );
        assert_eq!(
            guest.qualities,
            [
                (10000, "原画".to_string()),
                (400, "蓝光".to_string()),
                (250, "超清".to_string())
            ]
        );
        let cn = parse_play_url(PLAY_URL_CN_GOTCHA).unwrap().unwrap();
        assert!(cn.url.starts_with("https://d1--cn-gotcha03."), "{}", cn.url);
        assert_eq!(parse_play_url(PLAY_URL_MISSING).unwrap(), None);
        assert_eq!(parse_play_url("<html>").unwrap(), None);
    }

    #[test]
    fn guest_stream_is_capped_at_the_served_qn() {
        let stream = parse_play_url(PLAY_URL_GUEST).unwrap().unwrap();
        let info = build_live_info(snapshot(ROOM_INIT_LIVE), Some(stream), Quality::Od);
        assert!(info.is_live);
        assert_eq!(info.platform, Platform::Bilibili);
        assert_eq!(info.room_id, "100001");
        assert_eq!(info.anchor_id.as_deref(), Some("200001"));
        assert_eq!(
            info.live_url.as_deref(),
            Some("https://live.bilibili.com/321")
        );
        assert_eq!(info.title.as_deref(), Some("测试直播标题"));
        let names: Vec<&str> = info.variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["原画", "蓝光", "超清"]);
        assert_eq!(
            info.variants.iter().map(|v| v.quality).collect::<Vec<_>>(),
            [Some(Quality::Od), None, Some(Quality::Uhd)]
        );
        let selected = info.selected.unwrap();
        assert_eq!(selected.requested, Quality::Od);
        assert_eq!(selected.variant.name, "超清");
        assert_eq!(
            Some(&selected.record_url),
            selected.variant.flv_url.as_ref()
        );
        // Not an FLV-preferred platform: records `record_url`, which is FLV.
        assert_eq!(selected.source_url(false, true).0, SourceFormat::Flv);
        assert!(
            info.variants
                .iter()
                .filter(|v| v.name != "超清")
                .all(|v| v.flv_url.is_none() && v.hls_url.is_none())
        );
    }

    #[test]
    fn quality_tiers_request_streamget_qn_values() {
        assert_eq!(Quality::ALL.map(qn_for), [10000, 250, 150, 80, 80]);
        assert_eq!(quality_for_qn(80), Some(Quality::Sd));
        assert_eq!(quality_for_qn(400), None);
    }

    #[test]
    fn offline_room_has_no_streams() {
        let stream = parse_play_url(PLAY_URL_GUEST).unwrap();
        for init in [ROOM_INIT_OFFLINE, ROOM_INIT_RERUN] {
            let info = build_live_info(snapshot(init), stream.clone(), Quality::Od);
            assert!(!info.is_live);
            assert!(info.selected.is_none());
            assert!(info.variants.is_empty());
            assert_eq!(info.anchor_name, "测试主播");
        }
    }

    #[test]
    fn room_play_info_fallback_picks_the_avc_flv_stream() {
        let stream = parse_room_play_info(ROOM_PLAY_INFO_LIVE).unwrap().unwrap();
        assert_eq!(stream.current_qn, 250);
        assert!(stream.url.starts_with(
            "https://d1--ov-gotcha05.bilivideo.com/live-bvc/969454/live_test_2500.flv?expires="
        ));
        assert_eq!(
            stream.qualities,
            [
                (10000, "原画".to_string()),
                (400, "蓝光".to_string()),
                (250, "超清".to_string())
            ]
        );
        assert_eq!(parse_room_play_info(ROOM_PLAY_INFO_OFFLINE).unwrap(), None);

        // Only HLS: the variant carries an HLS URL and records as HLS.
        let hls_only = ROOM_PLAY_INFO_LIVE.replace("\"http_stream\"", "\"http_other\"");
        let stream = parse_room_play_info(&hls_only).unwrap().unwrap();
        assert!(
            stream.url.contains("live_test_2500.m3u8?expires="),
            "{}",
            stream.url
        );
        let info = build_live_info(snapshot(ROOM_INIT_LIVE), Some(stream), Quality::Hd);
        let selected = info.selected.unwrap();
        assert_eq!(
            selected.variant.hls_url.as_ref(),
            Some(&selected.record_url)
        );
        assert_eq!(selected.source_url(false, false).0, SourceFormat::Hls);

        let empty = ROOM_PLAY_INFO_LIVE.replace("\"stream\":", "\"stream_x\":");
        assert_eq!(parse_room_play_info(&empty).unwrap_err().kind(), "parse");
    }

    #[test]
    fn served_qn_missing_from_the_list_is_still_selected() {
        let stream = PlayStream {
            current_qn: 150,
            qualities: vec![(10000, "原画".to_string()), (80, "流畅".to_string())],
            url: "https://cdn.example/live.flv?qn=150".to_string(),
        };
        let (variants, index) = build_variants(&stream);
        let names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["原画", "150", "流畅"]);
        assert_eq!(index, 1);
        assert_eq!(variants[1].quality, Some(Quality::Hd));
    }

    #[test]
    fn user_cookie_goes_to_the_api_but_never_to_the_stream() {
        let headers = api_headers(None).unwrap();
        assert_eq!(headers.get(COOKIE).unwrap(), DEFAULT_COOKIE);
        let headers = api_headers(Some("SESSDATA=abc")).unwrap();
        assert_eq!(headers.get(COOKIE).unwrap(), "SESSDATA=abc");
        assert_eq!(
            api_headers(Some("a=1\r\nb=2")).unwrap_err().kind(),
            "config"
        );
        let stream = stream_headers();
        assert!(stream.keys().all(|key| !key.eq_ignore_ascii_case("cookie")));
        assert_eq!(
            stream.get("Referer").map(String::as_str),
            Some("https://live.bilibili.com/")
        );
    }
}
