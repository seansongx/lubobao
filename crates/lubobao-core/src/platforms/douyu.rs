//! Douyu adapter, following StreamCap's `DouyuHandler` and StreamGet v4.0.10
//! `DouyuLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/douyu/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The request flow, headers, the `getEncryption` white-key
//! signature and the quality-to-rate mapping follow that file; see
//! `THIRD_PARTY_NOTICES.md`. The signature is plain MD5, so no JavaScript
//! runtime is needed.
//!
//! Flow:
//!
//! * The room id comes from the link (`douyu.com/{rid}` or `?rid=`); a named
//!   room (`douyu.com/lpl`) is looked up on `m.douyu.com/{name}`.
//! * `www.douyu.com/betard/{rid}` gives the anchor, title and live state
//!   (`show_status == 1`). Rooms that do not exist get an HTML notice page.
//! * When live, `wgapi/livenc/liveweb/websec/getEncryption` returns the white
//!   key, which signs a `getH5PlayV1` request for the chosen `rate`. Its
//!   answer holds one FLV URL (`rtmp_url/rtmp_live`) and the room's
//!   `multirates`.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header, user_cookie,
};

/// `DouyuLiveStream.USER_AGENT`, also used to pull streams.
pub const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// `DouyuLiveStream.DEFAULT_DID`.
const DEFAULT_DID: &str = "10000000000000000000000000001501";
const WEB_ORIGIN: &str = "https://www.douyu.com";
const WEB_REFERER: &str = "https://www.douyu.com/";
const ROOM_PAGE: &str = "https://www.douyu.com/";
const MOBILE_PAGE: &str = "https://m.douyu.com/";
const BETARD_API: &str = "https://www.douyu.com/betard/";
const ENCRYPTION_API: &str = "https://www.douyu.com/wgapi/livenc/liveweb/websec/getEncryption";
const PLAY_API: &str = "https://playweb.douyucdn.cn/lapi/live/getH5PlayV1/";
/// `ver` sent with `getH5PlayV1`.
const PLAYER_VERSION: &str = "219032101";

/// StreamGet's `video_quality_options`: the `rate` asked from `getH5PlayV1`.
/// Douyu numbers its tiers 0 原画, 4 蓝光, 3 超清, 2 高清, 1 流畅; a rate the
/// room does not offer is answered with the nearest one it has.
pub fn rate_for(quality: Quality) -> u32 {
    match quality {
        Quality::Od => 0,
        Quality::Uhd => 3,
        Quality::Hd => 2,
        Quality::Sd | Quality::Ld => 1,
    }
}

/// A recognized Douyu room link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DouyuLink {
    /// Numeric room id, from the path or a `rid` query parameter.
    Room(String),
    /// Named room (`www.douyu.com/lpl`), resolved through `m.douyu.com`.
    Named(String),
}

impl DouyuLink {
    /// Canonical URL for the link.
    pub fn normalized_url(&self) -> String {
        match self {
            DouyuLink::Room(rid) => format!("{ROOM_PAGE}{rid}"),
            DouyuLink::Named(name) => format!("{ROOM_PAGE}{name}"),
        }
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Recognize and normalize a Douyu link without any network access.
///
/// StreamCap registers `https://.*\.douyu\.com/`; StreamGet takes the room id
/// from `douyu.com/(\d+)`, then from `rid=(\d+)`, and otherwise looks the
/// first path segment up on `m.douyu.com`. Only the web and mobile hosts are
/// accepted, and a topic page (`/topic/…`) must carry `rid`.
pub fn parse_link(input: &str) -> Result<DouyuLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的斗鱼直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if !matches!(host.as_str(), "www.douyu.com" | "douyu.com" | "m.douyu.com") {
        return Err(invalid());
    }
    let first = url
        .path_segments()
        .and_then(|mut segments| segments.find(|segment| !segment.is_empty()))
        .unwrap_or_default();
    if is_digits(first) {
        return Ok(DouyuLink::Room(first.to_string()));
    }
    if let Some((_, rid)) = url.query_pairs().find(|(key, _)| key == "rid")
        && is_digits(&rid)
    {
        return Ok(DouyuLink::Room(rid.into_owned()));
    }
    if first.eq_ignore_ascii_case("topic") {
        return Err(PlatformError::InvalidUrl(format!(
            "斗鱼专题页链接需要带 rid 参数（房间号）: {trimmed}"
        )));
    }
    if !first.is_empty()
        && first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Ok(DouyuLink::Named(first.to_string()));
    }
    Err(invalid())
}

/// Room id in an `m.douyu.com/{name}` page (StreamGet: `"rid":(\d+)`).
pub fn parse_mobile_page(html: &str) -> Result<String, PlatformError> {
    const KEY: &str = "\"rid\":";
    if let Some(start) = html.find(KEY) {
        let rest = &html[start + KEY.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            return Ok(digits);
        }
    }
    if html.trim().is_empty() {
        return Err(PlatformError::RateLimited(
            "斗鱼返回空页面，可能触发风控".to_string(),
        ));
    }
    Err(PlatformError::InvalidUrl(
        "斗鱼没有找到这个直播间，请检查链接".to_string(),
    ))
}

/// Room state from `betard`, before the stream is requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSnapshot {
    /// Numeric room id as Douyu reports it.
    pub room_id: String,
    /// Anchor's user id (`owner_uid`).
    pub anchor_id: Option<String>,
    pub anchor_name: String,
    /// Title, prefixed with `【轮播】` for replay loops as StreamGet does.
    pub title: Option<String>,
    /// `show_status == 1`. StreamGet also records replay loops.
    pub is_live: bool,
}

/// A JSON number or numeric string as text.
fn id_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

fn int_value(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Message of Douyu's「提示信息」page (`<div class="error">…<p>message</p>`),
/// shown for rooms that do not exist (「该房间目前没有开放」) or were closed
/// (「您观看的房间已被关闭…」).
fn notice_text(html: &str) -> Option<String> {
    if !html.contains("<title>提示信息") {
        return None;
    }
    let message = html
        .find("class=\"error\"")
        .map(|start| &html[start..])
        .and_then(|rest| {
            let start = rest.find("<p>")? + 3;
            let end = rest[start..].find("</p>")?;
            Some(rest[start..start + end].trim().to_string())
        })
        .filter(|message| !message.is_empty() && !message.contains('<'));
    Some(message.unwrap_or_else(|| "该房间目前无法访问（房间不存在或已关闭）".to_string()))
}

/// Parse a `betard/{rid}` response.
pub fn parse_betard(body: &str) -> Result<RoomSnapshot, PlatformError> {
    let json = match serde_json::from_str::<Value>(body) {
        Ok(json) => json,
        Err(error) => {
            // Rooms that do not exist or were closed get Douyu's「提示信息」page.
            if let Some(notice) = notice_text(body) {
                return Err(PlatformError::InvalidUrl(format!("斗鱼提示：{notice}")));
            }
            if body.trim().is_empty() {
                return Err(PlatformError::RateLimited(
                    "斗鱼返回空内容，可能触发风控".to_string(),
                ));
            }
            if body.trim_start().starts_with('<') {
                return Err(PlatformError::RateLimited(
                    "斗鱼返回了非直播间页面，可能触发风控".to_string(),
                ));
            }
            return Err(PlatformError::Parse(format!(
                "斗鱼直播间数据无法解析: {error}"
            )));
        }
    };
    let room = json
        .get("room")
        .filter(|room| room.is_object())
        .ok_or_else(|| PlatformError::Parse("斗鱼直播间数据缺少 room".to_string()))?;
    let room_id = id_text(room.get("room_id"))
        .ok_or_else(|| PlatformError::Parse("斗鱼直播间数据缺少房间号".to_string()))?;
    let anchor_name = room
        .get("nickname")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| PlatformError::Parse("斗鱼直播间数据缺少主播昵称".to_string()))?
        .to_string();
    let raw_title = room
        .get("room_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace("&nbsp;", " ");
    let raw_title = raw_title.trim();
    let is_loop = int_value(room.get("videoLoop")) == Some(1);
    let title = match (raw_title.is_empty(), is_loop) {
        (true, true) => Some("【轮播】".to_string()),
        (true, false) => None,
        (false, true) => Some(format!("【轮播】{raw_title}")),
        (false, false) => Some(raw_title.to_string()),
    };
    Ok(RoomSnapshot {
        room_id,
        anchor_id: id_text(room.get("owner_uid")),
        anchor_name,
        title,
        is_live: int_value(room.get("show_status")) == Some(1),
    })
}

/// White key from `getEncryption`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhiteKey {
    pub key: String,
    pub rand_str: String,
    pub enc_time: u32,
    pub is_special: bool,
    pub enc_data: String,
}

/// Parse a `getEncryption` response.
pub fn parse_encryption(body: &str) -> Result<WhiteKey, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        PlatformError::RateLimited("斗鱼签名接口返回的不是 JSON，可能触发风控".to_string())
    })?;
    let error = int_value(json.get("error"));
    if error != Some(0) {
        let message = json.get("msg").and_then(Value::as_str).unwrap_or_default();
        return Err(PlatformError::RateLimited(format!(
            "获取斗鱼签名密钥失败 {}：{message}",
            error.map_or_else(|| "未知".to_string(), |code| code.to_string())
        )));
    }
    let data = json.get("data").unwrap_or(&Value::Null);
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| PlatformError::Parse(format!("斗鱼签名密钥缺少 {key}")))
    };
    let enc_time = int_value(data.get("enc_time"))
        .and_then(|count| u32::try_from(count).ok())
        .filter(|count| *count <= 64)
        .ok_or_else(|| PlatformError::Parse("斗鱼签名密钥的 enc_time 无效".to_string()))?;
    Ok(WhiteKey {
        key: text("key")?,
        rand_str: text("rand_str")?,
        enc_time,
        is_special: int_value(data.get("is_special")).unwrap_or(0) != 0,
        enc_data: text("enc_data")?,
    })
}

/// `auth` for `getH5PlayV1`: MD5 `rand_str + key` `enc_time` times, then
/// MD5 of that, the key and (unless `is_special`) `rid + ts`.
pub fn sign_auth(white: &WhiteKey, rid: &str, timestamp: u64) -> String {
    let mut secret = white.rand_str.clone();
    for _ in 0..white.enc_time {
        secret = md5_hex(format!("{secret}{}", white.key).as_bytes());
    }
    let salt = if white.is_special {
        String::new()
    } else {
        format!("{rid}{timestamp}")
    };
    md5_hex(format!("{secret}{}{salt}", white.key).as_bytes())
}

/// Form fields for `getH5PlayV1`, in StreamGet's order.
pub fn play_form(
    white: &WhiteKey,
    rid: &str,
    rate: u32,
    timestamp: u64,
) -> Vec<(&'static str, String)> {
    vec![
        ("rate", rate.to_string()),
        ("ver", PLAYER_VERSION.to_string()),
        ("iar", "0".to_string()),
        ("ive", "0".to_string()),
        ("rid", rid.to_string()),
        ("hevc", "0".to_string()),
        ("fa", "0".to_string()),
        ("sov", "0".to_string()),
        ("enc_data", white.enc_data.clone()),
        ("tt", timestamp.to_string()),
        ("did", DEFAULT_DID.to_string()),
        ("auth", sign_auth(white, rid, timestamp)),
    ]
}

/// Stream answer from `getH5PlayV1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayResult {
    /// Variants best first and the index of the one the URL belongs to.
    Live {
        variants: Vec<StreamVariant>,
        selected: usize,
    },
    /// Error -5「房间未开播」.
    Offline,
}

/// Classify a non-zero `getH5PlayV1` error.
fn classify_play_error(code: i64, message: &str) -> PlatformError {
    let detail = format!("斗鱼返回错误 {code}：{message}");
    if code == -3 || message.contains("不存在") || message.contains("关闭") {
        return PlatformError::InvalidUrl(detail);
    }
    if message.contains("登录") {
        return PlatformError::AuthRequired(detail);
    }
    PlatformError::RateLimited(detail)
}

/// Tier label for a Douyu `rate`: the best StreamCap tier that asks for it.
fn quality_for_rate(rate: i64) -> Option<Quality> {
    Quality::ALL
        .into_iter()
        .find(|quality| i64::from(rate_for(*quality)) == rate)
}

/// Fallback name for a rate the room does not list in `multirates`.
fn rate_name(rate: i64) -> String {
    match rate {
        0 => "原画".to_string(),
        4 => "蓝光".to_string(),
        3 => "超清".to_string(),
        2 => "高清".to_string(),
        1 => "流畅".to_string(),
        other => format!("rate {other}"),
    }
}

/// Parse a `getH5PlayV1` response.
pub fn parse_play(body: &str) -> Result<PlayResult, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        PlatformError::RateLimited("斗鱼取流接口返回的不是 JSON，可能触发风控".to_string())
    })?;
    let code = int_value(json.get("error"))
        .ok_or_else(|| PlatformError::Parse("斗鱼取流接口缺少 error".to_string()))?;
    let message = json.get("msg").and_then(Value::as_str).unwrap_or_default();
    match code {
        0 => {}
        -5 => return Ok(PlayResult::Offline),
        _ => return Err(classify_play_error(code, message)),
    }
    let data = json
        .get("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| PlatformError::Parse("斗鱼取流接口缺少 data".to_string()))?;
    let part = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let (Some(base), Some(live)) = (part("rtmp_url"), part("rtmp_live")) else {
        return Err(PlatformError::Parse(
            "斗鱼取流接口没有返回流地址".to_string(),
        ));
    };
    let flv_url = format!("{base}/{live}");
    let rate = int_value(data.get("rate")).unwrap_or(0);

    let mut variants: Vec<(i64, StreamVariant)> = data
        .get("multirates")
        .and_then(Value::as_array)
        .map(|rates| {
            rates
                .iter()
                .filter_map(|entry| {
                    let rate = int_value(entry.get("rate"))?;
                    let name = entry
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| rate_name(rate));
                    Some((
                        rate,
                        StreamVariant {
                            name,
                            quality: quality_for_rate(rate),
                            flv_url: None,
                            hls_url: None,
                        },
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let selected = match variants.iter().position(|(entry, _)| *entry == rate) {
        Some(index) => index,
        None => {
            variants.push((
                rate,
                StreamVariant {
                    name: rate_name(rate),
                    quality: quality_for_rate(rate),
                    flv_url: None,
                    hls_url: None,
                },
            ));
            variants.len() - 1
        }
    };
    variants[selected].1.flv_url = Some(flv_url);
    Ok(PlayResult::Live {
        variants: variants.into_iter().map(|(_, variant)| variant).collect(),
        selected,
    })
}

/// Turn the room snapshot and the stream answer into `LiveInfo`. `play` is
/// `None` when the room is offline and no stream was requested.
pub fn build_live_info(
    snapshot: RoomSnapshot,
    play: Option<PlayResult>,
    quality: Quality,
) -> LiveInfo {
    let (variants, selected) = match play {
        Some(PlayResult::Live { variants, selected }) if snapshot.is_live => {
            let chosen = variants.get(selected).and_then(|variant| {
                // StreamGet: `record_url = flv_url`.
                let record_url = variant.flv_url.clone()?;
                Some(SelectedStream {
                    requested: quality,
                    variant: variant.clone(),
                    record_url,
                })
            });
            (variants, chosen)
        }
        _ => (Vec::new(), None),
    };
    LiveInfo {
        platform: Platform::Douyu,
        live_url: Some(format!("{ROOM_PAGE}{}", snapshot.room_id)),
        room_id: snapshot.room_id,
        anchor_id: snapshot.anchor_id,
        anchor_name: snapshot.anchor_name,
        title: snapshot.title,
        is_live: selected.is_some(),
        variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Douyu streams: User-Agent and the web Referer. User
/// cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Referer".to_string(), WEB_REFERER.to_string()),
    ])
}

/// `DouyuLiveStream.base_headers`: User-Agent, Referer and the user cookie.
pub fn base_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(REFERER, HeaderValue::from_static(WEB_REFERER));
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// `application/x-www-form-urlencoded` body. reqwest's `form` needs a
/// feature the workspace does not enable; the URL query serializer encodes
/// the same way.
fn form_body(fields: &[(&str, String)]) -> String {
    let mut url = Url::parse("http://form.invalid/").expect("valid URL");
    url.query_pairs_mut()
        .extend_pairs(fields.iter().map(|(key, value)| (*key, value.as_str())));
    url.query().unwrap_or_default().to_string()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Douyu adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct DouyuAdapter {
    clients: ClientCache,
}

impl DouyuAdapter {
    pub fn new() -> Self {
        DouyuAdapter::default()
    }

    async fn read(response: reqwest::Response, url: &str) -> Result<String, PlatformError> {
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), url));
        }
        Ok(response.text().await?)
    }

    async fn get_text(
        client: &reqwest::Client,
        url: &str,
        headers: HeaderMap,
    ) -> Result<String, PlatformError> {
        let response = client.get(url).headers(headers).send().await?;
        Self::read(response, url).await
    }

    async fn resolve_room_id(
        client: &reqwest::Client,
        link: &DouyuLink,
        cookie: Option<&str>,
    ) -> Result<String, PlatformError> {
        match link {
            DouyuLink::Room(rid) => Ok(rid.clone()),
            DouyuLink::Named(name) => {
                let html = Self::get_text(
                    client,
                    &format!("{MOBILE_PAGE}{name}"),
                    base_headers(cookie)?,
                )
                .await?;
                parse_mobile_page(&html)
            }
        }
    }

    async fn fetch_play(
        client: &reqwest::Client,
        rid: &str,
        quality: Quality,
        cookie: Option<&str>,
    ) -> Result<PlayResult, PlatformError> {
        // StreamGet sends only the User-Agent to `getEncryption`.
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        let mut api = Url::parse(ENCRYPTION_API).expect("valid encryption API URL");
        api.query_pairs_mut().append_pair("did", DEFAULT_DID);
        let white = parse_encryption(&Self::get_text(client, api.as_str(), headers).await?)?;

        let form = play_form(&white, rid, rate_for(quality), now_secs());
        let mut headers = base_headers(cookie)?;
        headers.insert(ORIGIN, HeaderValue::from_static(WEB_ORIGIN));
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let url = format!("{PLAY_API}{rid}");
        let body = form_body(&form);
        let response = client.post(&url).headers(headers).body(body).send().await?;
        parse_play(&Self::read(response, &url).await?)
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let rid = Self::resolve_room_id(&client, &link, cookie).await?;
        let body = Self::get_text(
            &client,
            &format!("{BETARD_API}{rid}"),
            base_headers(cookie)?,
        )
        .await?;
        let snapshot = parse_betard(&body)?;
        if !snapshot.is_live {
            return Ok(build_live_info(snapshot, None, options.quality));
        }
        let play = Self::fetch_play(&client, &snapshot.room_id, options.quality, cookie).await?;
        Ok(build_live_info(snapshot, Some(play), options.quality))
    }
}

impl PlatformAdapter for DouyuAdapter {
    fn platform(&self) -> Platform {
        Platform::Douyu
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

/// MD5 (RFC 1321) as lowercase hex. Douyu's signature is the only user, so a
/// small local implementation avoids another dependency.
fn md5_hex(input: &[u8]) -> String {
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut message = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let (blocks, _) = message.as_chunks::<64>();
    for block in blocks {
        let (words, _) = block.as_chunks::<4>();
        let words: Vec<u32> = words.iter().map(|word| u32::from_le_bytes(*word)).collect();
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
                .wrapping_add(K[i])
                .wrapping_add(words[g])
                .rotate_left(SHIFTS[i]);
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

    const BETARD_LIVE: &str = include_str!("../../tests/fixtures/douyu/betard_live.json");
    const BETARD_OFFLINE: &str = include_str!("../../tests/fixtures/douyu/betard_offline.json");
    const BETARD_MISSING: &str = include_str!("../../tests/fixtures/douyu/betard_missing.html");
    const BETARD_CLOSED: &str = include_str!("../../tests/fixtures/douyu/betard_closed.html");
    const ENCRYPTION: &str = include_str!("../../tests/fixtures/douyu/encryption.json");
    const PLAY_LIVE: &str = include_str!("../../tests/fixtures/douyu/play_live.json");
    const PLAY_OFFLINE: &str = include_str!("../../tests/fixtures/douyu/play_offline.json");
    const PLAY_MISSING: &str = include_str!("../../tests/fixtures/douyu/play_missing.json");

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let cases = [
            ("https://www.douyu.com/3637778?dyshid=", "3637778"),
            (
                "https://www.douyu.com/topic/wzDBLS6?rid=4921614&dyshid=",
                "4921614",
            ),
            ("  www.douyu.com/9999 ", "9999"),
            ("http://douyu.com/288016/", "288016"),
            ("https://m.douyu.com/3637778?from=share", "3637778"),
            ("https://WWW.DOUYU.COM/topic/abc?a=1&rid=12#x", "12"),
        ];
        let adapter = DouyuAdapter::new();
        for (input, rid) in cases {
            assert_eq!(
                parse_link(input).unwrap(),
                DouyuLink::Room(rid.to_string()),
                "{input}"
            );
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some(format!("https://www.douyu.com/{rid}").as_str()),
                "{input}"
            );
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            parse_link("https://www.douyu.com/lpl?dyshid=1").unwrap(),
            DouyuLink::Named("lpl".to_string())
        );
        assert_eq!(
            adapter.normalize_url("douyu.com/lpl").as_deref(),
            Some("https://www.douyu.com/lpl")
        );
    }

    #[test]
    fn rejects_other_sites_and_topic_pages_without_rid() {
        let adapter = DouyuAdapter::new();
        for input in [
            "https://www.douyu.com/",
            "https://www.douyu.com/topic/wzDBLS6",
            "https://www.douyu.com/topic/wzDBLS6?rid=abc",
            "https://v.douyu.com/show/abc",
            "https://www.douyu.com.evil.example/3637778",
            "https://www.douyu.com/bad%20name",
            "ftp://www.douyu.com/3637778",
            "https://live.kuaishou.com/u/yall1102",
            "",
        ] {
            assert_eq!(
                parse_link(input).unwrap_err().kind(),
                "invalid_url",
                "{input}"
            );
            assert!(!adapter.matches(input), "{input}");
        }
        assert!(
            parse_link("https://www.douyu.com/topic/x")
                .unwrap_err()
                .message()
                .contains("rid")
        );
        let douyu = super::super::adapter_for("https://www.douyu.com/3637778").unwrap();
        assert_eq!(douyu.platform(), Platform::Douyu);
        assert_eq!(Platform::Douyu.key(), "douyu");
        assert_eq!(Platform::Douyu.display_name(), "斗鱼直播");
    }

    #[test]
    fn mobile_page_gives_the_room_id_of_a_named_room() {
        let html = r#"<script>window.__INITIAL_STATE__={"room":{"roomInfo":{"roomInfo":{"rid":288016,"nickname":"x"}}}}</script>"#;
        assert_eq!(parse_mobile_page(html).unwrap(), "288016");
        assert_eq!(
            parse_mobile_page("<html>没有找到</html>")
                .unwrap_err()
                .kind(),
            "invalid_url"
        );
        assert_eq!(parse_mobile_page("  ").unwrap_err().kind(), "rate_limited");
    }

    #[test]
    fn betard_reports_anchor_title_and_live_state() {
        let live = parse_betard(BETARD_LIVE).unwrap();
        assert_eq!(
            live,
            RoomSnapshot {
                room_id: "100001".to_string(),
                anchor_id: Some("200001".to_string()),
                anchor_name: "测试主播".to_string(),
                title: Some("测试 直播间".to_string()),
                is_live: true,
            }
        );
        let offline = parse_betard(BETARD_OFFLINE).unwrap();
        assert!(!offline.is_live);
        assert_eq!(offline.room_id, "100002");

        // Replay loops count as live, with StreamGet's title prefix.
        let looped =
            parse_betard(&BETARD_LIVE.replace("\"videoLoop\": 0", "\"videoLoop\": 1")).unwrap();
        assert!(looped.is_live);
        assert_eq!(looped.title.as_deref(), Some("【轮播】测试 直播间"));
    }

    #[test]
    fn betard_errors_map_to_error_kinds() {
        let missing = parse_betard(BETARD_MISSING).unwrap_err();
        assert_eq!(missing.kind(), "invalid_url", "{missing}");
        assert_eq!(missing.message(), "斗鱼提示：该房间目前没有开放");
        let closed = parse_betard(BETARD_CLOSED).unwrap_err();
        assert_eq!(closed.kind(), "invalid_url", "{closed}");
        assert!(closed.message().contains("房间已被关闭"), "{closed}");
        let bare = parse_betard("<html><title>提示信息 -斗鱼</title></html>").unwrap_err();
        assert_eq!(bare.kind(), "invalid_url");
        for (body, kind) in [
            ("", "rate_limited"),
            ("<html><body>请完成验证</body></html>", "rate_limited"),
            ("{\"room\":", "parse"),
            ("{\"error\":1}", "parse"),
            ("{\"room\":{\"room_id\":1}}", "parse"),
        ] {
            assert_eq!(parse_betard(body).unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn md5_matches_reference_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex(&[b'a'; 1000]), "cabe45dcc9ae5b66ba86600cca6b8ba8");
        assert_eq!(
            md5_hex("斗鱼".as_bytes()),
            "710eab25b5a7013dac491a06a6224a98"
        );
    }

    #[test]
    fn auth_follows_streamget_white_key_signature() {
        let mut white = parse_encryption(ENCRYPTION).unwrap();
        assert_eq!(white.enc_time, 1);
        assert!(!white.is_special);
        // Values computed with StreamGet's Python code.
        assert_eq!(
            sign_auth(&white, "6979222", 1795476570),
            "949464debb4812d15be76c035c8ba6cc"
        );
        white.enc_time = 3;
        white.is_special = true;
        assert_eq!(
            sign_auth(&white, "6979222", 1795476570),
            "34982cb6471acae801a504763f3906f3"
        );

        let form = play_form(&white, "6979222", 3, 1795476570);
        let get = |key: &str| {
            form.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("rate"), Some("3"));
        assert_eq!(get("rid"), Some("6979222"));
        assert_eq!(get("tt"), Some("1795476570"));
        assert_eq!(get("did"), Some(DEFAULT_DID));
        assert_eq!(get("hevc"), Some("0"));
        assert_eq!(get("enc_data"), Some(white.enc_data.as_str()));
        assert_eq!(get("auth"), Some("34982cb6471acae801a504763f3906f3"));

        for body in [
            r#"{"error":1,"msg":"失败"}"#,
            "<html>",
            r#"{"error":0,"data":{"key":"k"}}"#,
        ] {
            assert!(parse_encryption(body).is_err(), "{body}");
        }
    }

    #[test]
    fn quality_maps_to_streamget_rates() {
        assert_eq!(rate_for(Quality::Od), 0);
        assert_eq!(rate_for(Quality::Uhd), 3);
        assert_eq!(rate_for(Quality::Hd), 2);
        assert_eq!(rate_for(Quality::Sd), 1);
        assert_eq!(rate_for(Quality::Ld), 1);
    }

    #[test]
    fn live_room_records_the_flv_of_the_returned_rate() {
        let snapshot = parse_betard(BETARD_LIVE).unwrap();
        let play = parse_play(PLAY_LIVE).unwrap();
        let info = build_live_info(snapshot, Some(play), Quality::Od);
        assert!(info.is_live);
        assert_eq!(info.platform, Platform::Douyu);
        assert_eq!(info.room_id, "100001");
        assert_eq!(info.anchor_id.as_deref(), Some("200001"));
        assert_eq!(
            info.live_url.as_deref(),
            Some("https://www.douyu.com/100001")
        );
        assert_eq!(info.title.as_deref(), Some("测试 直播间"));
        let names: Vec<_> = info.variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["原画1080P60", "超清", "高清"]);
        let tiers: Vec<_> = info.variants.iter().map(|v| v.quality).collect();
        assert_eq!(
            tiers,
            [Some(Quality::Od), Some(Quality::Uhd), Some(Quality::Hd)]
        );
        let selected = info.selected.unwrap();
        assert_eq!(selected.requested, Quality::Od);
        assert_eq!(selected.variant.name, "原画1080P60");
        assert!(
            selected
                .record_url
                .starts_with("https://ws3a.douyucdn.cn/live/100001rTestStream.flv?wsAuth=")
        );
        assert_eq!(
            selected.source_url(false, true),
            (
                super::super::SourceFormat::Flv,
                selected.record_url.as_str()
            )
        );
        assert!(
            info.stream_headers
                .keys()
                .all(|key| !key.eq_ignore_ascii_case("cookie"))
        );
        assert_eq!(
            info.stream_headers.get("Referer").map(String::as_str),
            Some("https://www.douyu.com/")
        );
    }

    #[test]
    fn a_rate_missing_from_multirates_still_gets_a_variant() {
        let body = PLAY_LIVE.replace(
            "\"rate\": 0,\n    \"cdnsWithName\"",
            "\"rate\": 1,\n    \"cdnsWithName\"",
        );
        let PlayResult::Live { variants, selected } = parse_play(&body).unwrap() else {
            panic!("expected live");
        };
        assert_eq!(variants.len(), 4);
        assert_eq!(selected, 3);
        assert_eq!(variants[3].name, "流畅");
        assert_eq!(variants[3].quality, Some(Quality::Sd));
        assert!(variants[3].flv_url.is_some());
        assert!(
            variants[..3]
                .iter()
                .all(|variant| variant.flv_url.is_none())
        );
    }

    #[test]
    fn offline_and_missing_rooms_from_the_play_api() {
        assert_eq!(parse_play(PLAY_OFFLINE).unwrap(), PlayResult::Offline);
        let snapshot = parse_betard(BETARD_LIVE).unwrap();
        let info = build_live_info(snapshot, Some(PlayResult::Offline), Quality::Hd);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());

        let offline = build_live_info(parse_betard(BETARD_OFFLINE).unwrap(), None, Quality::Od);
        assert!(!offline.is_live);
        assert_eq!(offline.anchor_name, "测试主播");

        assert_eq!(parse_play(PLAY_MISSING).unwrap_err().kind(), "invalid_url");
        for (body, kind) in [
            (r#"{"error":126,"msg":"版权原因不能播放"}"#, "rate_limited"),
            (r#"{"error":-9,"msg":"请先登录"}"#, "auth_required"),
            ("<html>", "rate_limited"),
            (r#"{"error":0,"data":{"rtmp_url":""}}"#, "parse"),
            (r#"{"error":0,"data":""}"#, "parse"),
        ] {
            assert_eq!(parse_play(body).unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn form_body_is_url_encoded() {
        let fields = [("enc_data", "a+b/c=".to_string()), ("rid", "1".to_string())];
        assert_eq!(form_body(&fields), "enc_data=a%2Bb%2Fc%3D&rid=1");
    }

    #[test]
    fn page_headers_carry_the_user_cookie_only_when_set() {
        assert!(base_headers(None).unwrap().get(COOKIE).is_none());
        assert_eq!(
            base_headers(Some("dy_did=1")).unwrap().get(COOKIE).unwrap(),
            "dy_did=1"
        );
        assert_eq!(
            base_headers(Some("a=1\r\nb=2")).unwrap_err().kind(),
            "config"
        );
    }
}
