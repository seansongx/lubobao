//! NetEase CC adapter, following StreamCap's `NeteaseHandler` (platform key
//! `netease`) and StreamGet v4.0.10 `NeteaseLiveStream`.
//!
//! StreamGet (https://github.com/ihmily/streamget, v4.0.10,
//! `streamget/platforms/netease/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily) reads the room page's `__NEXT_DATA__`
//! (`roomInfoInitData.live`). Since then `cc.163.com/{CC号}/` answers with a
//! 301 to `ds.163.com/glive/?ccid=…` (网易大神, sign-in required), so that page
//! no longer exists. This adapter keeps StreamGet's link form, headers, result
//! fields and position-based quality choice, but reads CC's own JSON APIs:
//!
//! * `vapi.cc.163.com/video_play_url/{ccid}` decides the live state: 200 with
//!   `videourl` (FLV) when live, 410 `no live` / `no tag` when not, 400 for
//!   ids CC rejects. It also lists the room's bit rates (`vbr_list`,
//!   `tcvbr_list`, `vbrname_mapping`); `?vbr={kbps}` returns that tier.
//! * `cc.163.com/live/channel/?channelids={subId}` gives the anchor name, the
//!   title and the original HLS stream (`sharefile`, StreamGet's `m3u8_url`).
//! * When the room is not live, the user page `cc.163.com/user/{ccid}/` (still
//!   a Next.js page) gives the anchor name and tells unknown ids apart
//!   (`personalInfo.code` 404).

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header,
    select_variant_index, user_cookie,
};

/// `BaseLiveStream._get_pc_headers` user agent, also used to pull streams.
pub const PC_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36 Edg/121.0.0.0";
const PC_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.8,zh-TW;q=0.7,zh-HK;q=0.5,en-US;q=0.3,en;q=0.2";
/// Referer sent when pulling streams.
const STREAM_REFERER: &str = "https://cc.163.com/";

const ROOM_PAGE: &str = "https://cc.163.com/";
const PLAY_URL_API: &str = "https://vapi.cc.163.com/video_play_url/";
const CHANNEL_API: &str = "https://cc.163.com/live/channel/";
const USER_PAGE: &str = "https://cc.163.com/user/";

/// Largest CC id the play-URL API accepts (its schema maximum).
const MAX_CCID: u64 = 2_147_483_648;

const NEXT_DATA_START: &str = "<script id=\"__NEXT_DATA__\"";

/// A recognized CC room link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeteaseLink {
    /// The anchor's CC number (`ccid`), which is also the room address.
    pub ccid: String,
}

impl NeteaseLink {
    /// Canonical URL for the link (StreamGet adds the trailing slash).
    pub fn normalized_url(&self) -> String {
        format!("{ROOM_PAGE}{}/", self.ccid)
    }
}

/// Recognize and normalize a CC link without any network access.
///
/// StreamCap registers `cc\.163\.com/`. Accepted forms:
/// `cc.163.com/{ccid}` (README form), the mobile share link
/// `h5.cc.163.com/cc/{ccid}`, and `ds.163.com/glive/?ccid={ccid}`, where the
/// browser now lands after opening a room link. Query strings are dropped.
pub fn parse_link(input: &str) -> Result<NeteaseLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的网易CC直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    let candidate = match (host.as_str(), segments.as_slice()) {
        ("cc.163.com" | "www.cc.163.com", [id]) => Some(id.to_string()),
        ("h5.cc.163.com", ["cc", id]) => Some(id.to_string()),
        ("ds.163.com", ["glive"]) => url
            .query_pairs()
            .find(|(key, _)| key == "ccid")
            .map(|(_, value)| value.into_owned()),
        _ => None,
    };
    let ccid = candidate.ok_or_else(invalid)?;
    if ccid.is_empty() || ccid.len() > 10 || !ccid.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    match ccid.parse::<u64>() {
        Ok(value) if (1..=MAX_CCID).contains(&value) => Ok(NeteaseLink {
            ccid: value.to_string(),
        }),
        _ => Err(PlatformError::InvalidUrl(format!(
            "网易CC号 {ccid} 超出范围，请检查链接"
        ))),
    }
}

/// One tier of the room, from the play-URL API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    /// Display name, e.g. `原画`, `超清` (`vbrname_mapping`).
    pub name: String,
    /// Bit rate in kbps, the value `?vbr=` takes.
    pub vbr: u64,
}

/// Result of the play-URL API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayUrl {
    Live(LivePlayUrl),
    /// `no live` (anchor exists, not live) or `no tag` (never streamed or
    /// unknown id; the user page tells them apart).
    Offline,
}

/// A live room as the play-URL API describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivePlayUrl {
    /// Channel id (`subId`) for the channel API.
    pub channel_id: Option<String>,
    /// Anchor uid (`anchor_uid`).
    pub anchor_uid: Option<String>,
    /// Tiers, highest bit rate first. Always at least one.
    pub tiers: Vec<Tier>,
    /// Bit rate of the returned stream (`vbr_sel`).
    pub selected_vbr: Option<u64>,
    /// FLV stream for the selected tier (`videourl`).
    pub flv_url: String,
}

/// A string or number field as a string.
fn text_field(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Parse a play-URL API response (`status` is the HTTP status).
pub fn parse_play_url(status: u16, body: &str) -> Result<PlayUrl, PlatformError> {
    let json: Value = match serde_json::from_str(body) {
        Ok(json) => json,
        Err(_) if status == 404 || status == 400 => {
            return Err(PlatformError::InvalidUrl(
                "网易CC不认这个CC号，请检查链接".to_string(),
            ));
        }
        Err(_) if (200..300).contains(&status) => {
            return Err(PlatformError::RateLimited(if body.trim().is_empty() {
                "网易CC接口返回空内容，可能触发风控".to_string()
            } else {
                "网易CC接口返回的不是 JSON，可能触发风控".to_string()
            }));
        }
        Err(_) => return Err(classify_http_status(status, "网易CC取流接口")),
    };
    let code = json.get("code").and_then(Value::as_str).unwrap_or_default();
    let data = json.get("data").and_then(Value::as_str).unwrap_or_default();
    match code {
        "Gone" if matches!(data, "no live" | "no tag") => return Ok(PlayUrl::Offline),
        "BAD_REQUEST" => {
            return Err(PlatformError::InvalidUrl(
                "网易CC不认这个CC号，请检查链接".to_string(),
            ));
        }
        "" => {}
        other => {
            let message = json
                .get("msg")
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
                .unwrap_or(data);
            return Err(if (200..300).contains(&status) || status >= 500 {
                PlatformError::RateLimited(format!("网易CC接口返回 {other}：{message}"))
            } else {
                classify_http_status(status, &format!("网易CC接口返回 {other}：{message}"))
            });
        }
    }
    if !(200..300).contains(&status) {
        return Err(classify_http_status(status, "网易CC取流接口"));
    }
    let flv_url = json
        .get("videourl")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| PlatformError::Parse("网易CC取流接口没有返回 videourl".to_string()))?
        .to_string();
    let selected_vbr = json.get("vbr_sel").and_then(Value::as_u64);

    // Names by bit rate: `tcvbr_list` maps tier key → kbps and
    // `vbrname_mapping` tier key → display name.
    let mut names: BTreeMap<u64, String> = BTreeMap::new();
    if let Some(list) = json.get("tcvbr_list").and_then(Value::as_object) {
        for (key, vbr) in list {
            if let Some(vbr) = vbr.as_u64() {
                let name = json
                    .pointer(&format!("/vbrname_mapping/{key}"))
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| key.clone());
                names.insert(vbr, name);
            }
        }
    }
    let mut rates: Vec<u64> = json
        .get("vbr_list")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    if rates.is_empty() {
        rates = names.keys().copied().collect();
    }
    if rates.is_empty()
        && let Some(vbr) = selected_vbr
    {
        rates.push(vbr);
    }
    rates.sort_unstable_by(|a, b| b.cmp(a));
    rates.dedup();
    let mut tiers: Vec<Tier> = rates
        .into_iter()
        .map(|vbr| Tier {
            name: names.get(&vbr).cloned().unwrap_or_else(|| format!("{vbr}")),
            vbr,
        })
        .collect();
    if tiers.is_empty() {
        tiers.push(Tier {
            name: "原画".to_string(),
            vbr: 0,
        });
    }
    Ok(PlayUrl::Live(LivePlayUrl {
        channel_id: text_field(&json, "subId"),
        anchor_uid: text_field(&json, "anchor_uid"),
        tiers,
        selected_vbr,
        flv_url,
    }))
}

/// Channel details from the channel API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub anchor_name: Option<String>,
    pub title: Option<String>,
    /// Original HLS stream (`sharefile`), StreamGet's `m3u8_url`.
    pub hls_url: Option<String>,
}

/// Parse a channel API response. `None` when the channel is not listed.
pub fn parse_channel(body: &str) -> Result<Option<Channel>, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        PlatformError::RateLimited("网易CC频道接口返回的不是 JSON，可能触发风控".to_string())
    })?;
    let Some(room) = json
        .pointer("/data/0")
        .filter(|room| room.get("nickname").is_some() || room.get("title").is_some())
    else {
        return Ok(None);
    };
    Ok(Some(Channel {
        anchor_name: text_field(room, "nickname"),
        title: text_field(room, "title"),
        hls_url: text_field(room, "sharefile"),
    }))
}

/// Anchor from the user page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub anchor_name: String,
    pub uid: Option<String>,
}

/// Read `personalInfo.uinfo` from the user page's `__NEXT_DATA__`.
pub fn parse_user_page(html: &str, ccid: &str) -> Result<UserInfo, PlatformError> {
    let json = next_data(html).ok_or_else(|| {
        if html.trim().is_empty() {
            PlatformError::RateLimited("网易CC返回了空页面，可能触发风控".to_string())
        } else {
            PlatformError::Parse("网易CC用户页面缺少 __NEXT_DATA__".to_string())
        }
    })?;
    let info = json
        .pointer("/props/pageProps/personalInfo")
        .ok_or_else(|| PlatformError::Parse("网易CC用户页面缺少 personalInfo".to_string()))?;
    let not_found =
        || PlatformError::InvalidUrl(format!("网易CC没有找到 CC号 {ccid} 的用户，请检查链接"));
    if info.get("code").and_then(Value::as_i64) == Some(404) {
        return Err(not_found());
    }
    let uinfo = info.get("uinfo").unwrap_or(&Value::Null);
    let uid = text_field(uinfo, "uid").filter(|uid| uid != "0");
    let anchor_name = text_field(uinfo, "nickname");
    match (anchor_name, &uid) {
        (Some(anchor_name), _) => Ok(UserInfo { anchor_name, uid }),
        (None, None) => Err(not_found()),
        (None, Some(_)) => Err(PlatformError::Parse(
            "网易CC用户页面缺少主播昵称".to_string(),
        )),
    }
}

/// The JSON inside `<script id="__NEXT_DATA__" …>…</script>`.
fn next_data(html: &str) -> Option<Value> {
    let start = html.find(NEXT_DATA_START)?;
    let rest = &html[start..];
    let open = rest.find('>')? + 1;
    let end = rest[open..].find("</script>")?;
    serde_json::from_str(&rest[open..open + end]).ok()
}

/// Room state before the stream for a tier is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSnapshot {
    pub ccid: String,
    pub anchor_id: Option<String>,
    pub anchor_name: String,
    pub title: Option<String>,
    pub is_live: bool,
    /// Best-first variants; only the tier whose URL was fetched has one.
    pub variants: Vec<StreamVariant>,
}

/// Tier index for `quality`, padding the list like StreamGet does.
pub fn tier_index(tiers: &[Tier], quality: Quality) -> Option<usize> {
    select_variant_index(tiers.len(), quality)
}

/// Variants for a live room. `flv_url` belongs to the tier with bit rate
/// `flv_vbr` (the one the API returned); the original HLS stream goes on the
/// first tier.
pub fn build_variants(
    tiers: &[Tier],
    flv_vbr: Option<u64>,
    flv_url: &str,
    hls_url: Option<&str>,
) -> Vec<StreamVariant> {
    let flv_index = flv_vbr
        .and_then(|vbr| tiers.iter().position(|tier| tier.vbr == vbr))
        .unwrap_or(0);
    tiers
        .iter()
        .enumerate()
        .map(|(index, tier)| StreamVariant {
            name: tier.name.clone(),
            quality: Quality::from_index(index),
            flv_url: (index == flv_index).then(|| flv_url.to_string()),
            hls_url: hls_url.filter(|_| index == 0).map(str::to_string),
        })
        .collect()
}

/// Turn a snapshot into `LiveInfo`. The selected stream is the variant that
/// carries the FLV URL (StreamGet: `record_url = flv_url or m3u8_url`).
pub fn build_live_info(snapshot: RoomSnapshot, quality: Quality) -> LiveInfo {
    let selected = snapshot
        .variants
        .iter()
        .find(|variant| variant.flv_url.is_some())
        .or_else(|| snapshot.variants.first())
        .filter(|_| snapshot.is_live)
        .and_then(|variant| {
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
        platform: Platform::Netease,
        live_url: Some(format!("{ROOM_PAGE}{}/", snapshot.ccid)),
        room_id: snapshot.ccid,
        anchor_id: snapshot.anchor_id,
        anchor_name: snapshot.anchor_name,
        title: snapshot.title,
        is_live: snapshot.is_live && selected.is_some(),
        variants: snapshot.variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling CC streams: the PC User-Agent and the site Referer.
/// User cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), PC_USER_AGENT.to_string()),
        ("Referer".to_string(), STREAM_REFERER.to_string()),
    ])
}

/// `pc_headers`, with the user's cookie when set (StreamGet sends it on the
/// room page request).
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

/// NetEase CC adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct NeteaseAdapter {
    clients: ClientCache,
}

impl NeteaseAdapter {
    pub fn new() -> Self {
        NeteaseAdapter::default()
    }

    /// GET returning the status and body, without failing on error statuses
    /// (the play-URL API answers 410 for rooms that are not live).
    async fn get(
        client: &reqwest::Client,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(u16, String), PlatformError> {
        let response = client.get(url).headers(headers).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        Ok((status, body))
    }

    async fn get_ok(
        client: &reqwest::Client,
        url: &str,
        headers: HeaderMap,
    ) -> Result<String, PlatformError> {
        let (status, body) = Self::get(client, url, headers).await?;
        if !(200..300).contains(&status) {
            return Err(classify_http_status(status, url));
        }
        Ok(body)
    }

    async fn play_url(
        client: &reqwest::Client,
        ccid: &str,
        vbr: Option<u64>,
        cookie: Option<&str>,
    ) -> Result<PlayUrl, PlatformError> {
        let mut url = format!("{PLAY_URL_API}{ccid}");
        if let Some(vbr) = vbr {
            url.push_str(&format!("?vbr={vbr}"));
        }
        let (status, body) = Self::get(client, &url, pc_headers(cookie)?).await?;
        parse_play_url(status, &body)
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let PlayUrl::Live(mut play) = Self::play_url(&client, &link.ccid, None, cookie).await?
        else {
            let page = format!("{USER_PAGE}{}/", link.ccid);
            let html = Self::get_ok(&client, &page, pc_headers(cookie)?).await?;
            let user = parse_user_page(&html, &link.ccid)?;
            return Ok(build_live_info(
                RoomSnapshot {
                    ccid: link.ccid,
                    anchor_id: user.uid,
                    anchor_name: user.anchor_name,
                    title: None,
                    is_live: false,
                    variants: Vec::new(),
                },
                options.quality,
            ));
        };

        // The first answer is the default tier (the original); ask again for
        // any other tier.
        let wanted = tier_index(&play.tiers, options.quality)
            .and_then(|index| play.tiers.get(index))
            .map(|tier| tier.vbr)
            .filter(|vbr| *vbr != 0 && Some(*vbr) != play.selected_vbr);
        if let Some(vbr) = wanted
            && let PlayUrl::Live(tier) =
                Self::play_url(&client, &link.ccid, Some(vbr), cookie).await?
        {
            play.flv_url = tier.flv_url;
            play.selected_vbr = tier.selected_vbr.or(Some(vbr));
        }

        let channel = match &play.channel_id {
            Some(channel_id) => {
                let mut api = Url::parse(CHANNEL_API).expect("valid channel API URL");
                api.query_pairs_mut().append_pair("channelids", channel_id);
                let body = Self::get_ok(&client, api.as_str(), pc_headers(cookie)?).await?;
                parse_channel(&body)?
            }
            None => None,
        };
        let channel = channel.unwrap_or(Channel {
            anchor_name: None,
            title: None,
            hls_url: None,
        });
        let anchor_name = match channel.anchor_name {
            Some(name) => name,
            None => {
                let page = format!("{USER_PAGE}{}/", link.ccid);
                let html = Self::get_ok(&client, &page, pc_headers(cookie)?).await?;
                parse_user_page(&html, &link.ccid)?.anchor_name
            }
        };
        let variants = build_variants(
            &play.tiers,
            play.selected_vbr,
            &play.flv_url,
            channel.hls_url.as_deref(),
        );
        Ok(build_live_info(
            RoomSnapshot {
                ccid: link.ccid,
                anchor_id: play.anchor_uid,
                anchor_name,
                title: channel.title,
                is_live: true,
                variants,
            },
            options.quality,
        ))
    }
}

impl PlatformAdapter for NeteaseAdapter {
    fn platform(&self) -> Platform {
        Platform::Netease
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

    const PLAY_LIVE: &str = include_str!("../../tests/fixtures/netease/video_play_url_live.json");
    const PLAY_LOW: &str = include_str!("../../tests/fixtures/netease/video_play_url_low.json");
    const PLAY_OFFLINE: &str =
        include_str!("../../tests/fixtures/netease/video_play_url_offline.json");
    const PLAY_NO_TAG: &str =
        include_str!("../../tests/fixtures/netease/video_play_url_no_tag.json");
    const CHANNEL_LIVE: &str = include_str!("../../tests/fixtures/netease/channel_live.json");
    const USER_OFFLINE: &str = include_str!("../../tests/fixtures/netease/user_offline.html");
    const USER_NOT_FOUND: &str = include_str!("../../tests/fixtures/netease/user_not_found.html");

    fn live(body: &str) -> LivePlayUrl {
        match parse_play_url(200, body).unwrap() {
            PlayUrl::Live(play) => play,
            PlayUrl::Offline => panic!("expected live"),
        }
    }

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let cases = [
            "https://cc.163.com/583946984",
            "https://cc.163.com/583946984/",
            "  cc.163.com/583946984?from=share ",
            "http://CC.163.COM/583946984#top",
            "https://h5.cc.163.com/cc/583946984",
            "https://ds.163.com/glive/?ccid=583946984",
            "https://ds.163.com/glive?appKey=x&ccid=583946984",
        ];
        let adapter = NeteaseAdapter::new();
        for input in cases {
            assert_eq!(parse_link(input).unwrap().ccid, "583946984", "{input}");
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some("https://cc.163.com/583946984/"),
                "{input}"
            );
            assert!(adapter.matches(input), "{input}");
        }
        let cc = super::super::adapter_for("https://cc.163.com/583946984").unwrap();
        assert_eq!(cc.platform(), Platform::Netease);
    }

    #[test]
    fn rejects_other_pages_and_bad_ids() {
        let adapter = NeteaseAdapter::new();
        for input in [
            "https://cc.163.com/",
            "https://cc.163.com/user/583946984/",
            "https://cc.163.com/category/game",
            "https://cc.163.com/abc",
            "https://cc.163.com/0",
            "https://cc.163.com/99999999999",
            "https://cc.163.com/2147483649",
            "https://ds.163.com/glive/",
            "https://look.163.com/live?id=65108820",
            "https://cc.163.com.evil.example/583946984",
            "ftp://cc.163.com/583946984",
            "",
        ] {
            assert_eq!(
                parse_link(input).unwrap_err().kind(),
                "invalid_url",
                "{input}"
            );
            assert!(!adapter.matches(input), "{input}");
        }
        assert_eq!(
            parse_link("cc.163.com/2147483648").unwrap().ccid,
            "2147483648"
        );
    }

    #[test]
    fn live_play_url_lists_tiers_by_bit_rate() {
        let play = live(PLAY_LIVE);
        assert_eq!(play.channel_id.as_deref(), Some("6519303"));
        assert_eq!(play.anchor_uid.as_deref(), Some("289164163"));
        assert_eq!(play.selected_vbr, Some(7999));
        assert_eq!(
            play.tiers
                .iter()
                .map(|tier| (tier.name.as_str(), tier.vbr))
                .collect::<Vec<_>>(),
            [
                ("原画", 7999),
                ("超清", 2000),
                ("高清", 1000),
                ("标清", 600)
            ]
        );
        assert!(
            play.flv_url
                .starts_with("http://hsrelayhdl.cc.netease.com/pushstation/"),
            "{}",
            play.flv_url
        );
        assert!(play.flv_url.contains(".flv?"));

        let low = live(PLAY_LOW);
        assert_eq!(low.selected_vbr, Some(600));
        assert!(low.flv_url.contains("tc4.flv?"), "{}", low.flv_url);
    }

    #[test]
    fn offline_and_unknown_ids_are_not_live_and_errors_map_to_kinds() {
        assert_eq!(parse_play_url(410, PLAY_OFFLINE).unwrap(), PlayUrl::Offline);
        assert_eq!(parse_play_url(410, PLAY_NO_TAG).unwrap(), PlayUrl::Offline);
        let cases = [
            (
                400,
                r#"{"msg":"bad","code":"BAD_REQUEST","why":"BadRequest"}"#,
                "invalid_url",
            ),
            (404, "<title>404 Not Found</title>", "invalid_url"),
            (200, "", "rate_limited"),
            (200, "<html>blocked</html>", "rate_limited"),
            (503, "<html>busy</html>", "network"),
            (403, "<html>denied</html>", "rate_limited"),
            (410, r#"{"code":"Gone","data":"other"}"#, "invalid_url"),
            (200, r#"{"code":"LIMIT","msg":"slow down"}"#, "rate_limited"),
            (200, r#"{"cdn_list":[]}"#, "parse"),
        ];
        for (status, body, kind) in cases {
            assert_eq!(
                parse_play_url(status, body).unwrap_err().kind(),
                kind,
                "{status} {body}"
            );
        }
    }

    #[test]
    fn play_url_without_lists_still_has_one_tier() {
        let play = live(r#"{"videourl":"http://x/a.flv","vbr_sel":3000}"#);
        assert_eq!(
            play.tiers,
            [Tier {
                name: "3000".into(),
                vbr: 3000
            }]
        );
        let play = live(r#"{"videourl":"http://x/a.flv"}"#);
        assert_eq!(play.tiers[0].name, "原画");
        assert_eq!(play.tiers[0].vbr, 0);
    }

    #[test]
    fn channel_gives_name_title_and_original_hls() {
        let channel = parse_channel(CHANNEL_LIVE).unwrap().unwrap();
        assert_eq!(channel.anchor_name.as_deref(), Some("实况足球官方"));
        assert_eq!(
            channel.title.as_deref(),
            Some("实况足球超级联赛26-27夏季赛")
        );
        assert!(
            channel
                .hls_url
                .as_deref()
                .unwrap()
                .starts_with("https://alipullhls.cc.netease.com/pushstation/")
        );
        assert_eq!(parse_channel(r#"{"data":[{"nolive":1}]}"#).unwrap(), None);
        assert_eq!(parse_channel(r#"{"data":[]}"#).unwrap(), None);
        assert_eq!(parse_channel("oops").unwrap_err().kind(), "rate_limited");
    }

    #[test]
    fn user_page_gives_the_anchor_or_says_the_id_is_unknown() {
        let user = parse_user_page(USER_OFFLINE, "583946984").unwrap();
        assert_eq!(user.anchor_name, "路人6984");
        assert_eq!(user.uid.as_deref(), Some("440395813"));
        let error = parse_user_page(USER_NOT_FOUND, "1").unwrap_err();
        assert_eq!(error.kind(), "invalid_url");
        assert!(error.message().contains("CC号 1"), "{error}");
        assert_eq!(
            parse_user_page("  ", "1").unwrap_err().kind(),
            "rate_limited"
        );
        assert_eq!(
            parse_user_page("<html></html>", "1").unwrap_err().kind(),
            "parse"
        );
    }

    #[test]
    fn quality_picks_by_padded_position() {
        let tiers = live(PLAY_LIVE).tiers;
        let expected = [
            (Quality::Od, 7999),
            (Quality::Uhd, 2000),
            (Quality::Hd, 1000),
            (Quality::Sd, 600),
            (Quality::Ld, 600),
        ];
        for (quality, vbr) in expected {
            let index = tier_index(&tiers, quality).unwrap();
            assert_eq!(tiers[index].vbr, vbr, "{quality}");
        }
    }

    #[test]
    fn live_info_records_the_fetched_flv_tier() {
        let play = live(PLAY_LIVE);
        let channel = parse_channel(CHANNEL_LIVE).unwrap().unwrap();
        let low = live(PLAY_LOW);
        let variants = build_variants(
            &play.tiers,
            low.selected_vbr,
            &low.flv_url,
            channel.hls_url.as_deref(),
        );
        assert_eq!(
            variants.iter().map(|v| v.quality).collect::<Vec<_>>(),
            [
                Some(Quality::Od),
                Some(Quality::Uhd),
                Some(Quality::Hd),
                Some(Quality::Sd)
            ]
        );
        assert!(variants[0].hls_url.is_some() && variants[0].flv_url.is_none());
        assert!(variants[3].flv_url.is_some());
        let info = build_live_info(
            RoomSnapshot {
                ccid: "367592480".into(),
                anchor_id: play.anchor_uid.clone(),
                anchor_name: "实况足球官方".into(),
                title: channel.title.clone(),
                is_live: true,
                variants,
            },
            Quality::Sd,
        );
        assert!(info.is_live);
        assert_eq!(info.platform, Platform::Netease);
        assert_eq!(info.room_id, "367592480");
        assert_eq!(
            info.live_url.as_deref(),
            Some("https://cc.163.com/367592480/")
        );
        let selected = info.selected.unwrap();
        assert_eq!(selected.variant.name, "标清");
        assert_eq!(selected.requested, Quality::Sd);
        assert_eq!(selected.record_url, low.flv_url);
        assert_eq!(
            selected.source_url(false, false),
            (super::super::SourceFormat::Flv, low.flv_url.as_str())
        );

        // Original: FLV and HLS on the same tier, FLV is recorded.
        let variants = build_variants(
            &play.tiers,
            play.selected_vbr,
            &play.flv_url,
            channel.hls_url.as_deref(),
        );
        let info = build_live_info(
            RoomSnapshot {
                ccid: "367592480".into(),
                anchor_id: None,
                anchor_name: "x".into(),
                title: None,
                is_live: true,
                variants,
            },
            Quality::Od,
        );
        let selected = info.selected.unwrap();
        assert_eq!(selected.variant.name, "原画");
        assert_eq!(selected.record_url, play.flv_url);
        assert_eq!(
            selected.source_url(false, false).0,
            super::super::SourceFormat::Flv
        );
    }

    #[test]
    fn offline_room_has_anchor_but_no_streams() {
        let user = parse_user_page(USER_OFFLINE, "583946984").unwrap();
        let info = build_live_info(
            RoomSnapshot {
                ccid: "583946984".into(),
                anchor_id: user.uid,
                anchor_name: user.anchor_name,
                title: None,
                is_live: false,
                variants: Vec::new(),
            },
            Quality::Od,
        );
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert_eq!(info.anchor_name, "路人6984");
        assert_eq!(info.anchor_id.as_deref(), Some("440395813"));
    }

    #[test]
    fn headers_carry_cookies_only_to_cc_pages() {
        let headers = stream_headers();
        assert!(
            headers
                .keys()
                .all(|key| !key.eq_ignore_ascii_case("cookie"))
        );
        assert_eq!(
            headers.get("Referer").map(String::as_str),
            Some("https://cc.163.com/")
        );
        assert!(pc_headers(None).unwrap().get(COOKIE).is_none());
        assert_eq!(pc_headers(Some("a=1")).unwrap().get(COOKIE).unwrap(), "a=1");
        assert_eq!(pc_headers(Some("a=1\r\nb=2")).unwrap_err().kind(), "config");
    }
}
