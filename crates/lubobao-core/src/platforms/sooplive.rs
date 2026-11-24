//! SOOP (formerly AfreecaTV) adapter, following StreamCap's `SoopHandler` and
//! StreamGet v4.0.10 `SoopLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/soop/live_stream.py`, MIT License, Copyright (c) 2025
//! Hmily. The request headers, the `get_station_status.php` nickname lookup,
//! the `player_live_api.php` live check and `AID` token, the
//! `broad_stream_assign.html` CDN lookup and the master playlist sorted by
//! bandwidth follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * `st.sooplive.com/api/get_station_status.php?szBjId=…` gives the nickname;
//!   `RESULT: 0` means the broadcaster does not exist.
//! * `live.sooplive.com/afreeca/player_live_api.php` (form POST) answers with
//!   `CHANNEL.RESULT`: `1` live (with `BNO`, the broadcast number), `0` not
//!   live, `-6` login needed (adult or age-limited broadcast). A second call
//!   with `type=aid` returns the stream token.
//! * `livestream-manager.sooplive.com/broad_stream_assign.html` returns the
//!   CDN master playlist for the broadcast; `?aid=` makes it playable.
//!
//! StreamGet logs in with a username and password when the answer is not
//! `0`/`1`. LuBoBao only supports the user's cookie (`Cookie` setting), sent to
//! the player API; a login-only room without one is `auth_required`.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT};
use serde_json::Value;

use super::hls::{MasterVariant, parse_master};
use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

/// `SoopLiveStream._get_pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:122.0) Gecko/20100101 Firefox/122.0";
const ORIGIN_VALUE: &str = "https://play.sooplive.com";
const REFERER_VALUE: &str = "https://play.sooplive.com/superbsw123/277837074";
const STATION_API: &str = "https://st.sooplive.com/api/get_station_status.php";
const PLAYER_API: &str = "https://live.sooplive.com/afreeca/player_live_api.php";
const ASSIGN_API: &str = "http://livestream-manager.sooplive.com/broad_stream_assign.html";

/// First path segments of SOOP pages that are not broadcasters.
const RESERVED_PATHS: [&str; 14] = [
    "directory",
    "player",
    "vod",
    "search",
    "my",
    "total",
    "live",
    "login",
    "signup",
    "station",
    "main",
    "schedule",
    "notice",
    "event",
];

/// Broadcaster ids: letters, digits and `_`.
fn is_bj_id(text: &str) -> bool {
    (2..=20).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A recognized SOOP broadcaster link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoopLink {
    pub bj_id: String,
    /// Room password from `?pwd=`, for password-protected broadcasts.
    pub password: Option<String>,
}

impl SoopLink {
    /// Canonical URL used to detect duplicate tasks; keeps the password.
    pub fn normalized_url(&self) -> String {
        let mut url = room_url(&self.bj_id);
        if let Some(password) = &self.password {
            let mut parsed = Url::parse(&url).expect("valid room URL");
            parsed.query_pairs_mut().append_pair("pwd", password);
            url = parsed.to_string();
        }
        url
    }
}

/// Broadcaster page.
pub fn room_url(bj_id: &str) -> String {
    format!("https://play.sooplive.com/{bj_id}")
}

/// Recognize a SOOP link without any network access.
///
/// StreamCap registers `sooplive.co.kr/` and `sooplive.com/`; StreamGet reads
/// the broadcaster id from the first path segment (`play.sooplive.com/{id}` or
/// `play.sooplive.com/{id}/{broadcast number}`). `ch.sooplive.com/{id}`
/// station pages name the same broadcaster.
pub fn parse_link(input: &str) -> Result<SoopLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的 SOOP 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    let known = ["sooplive.com", "sooplive.co.kr"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")));
    if !known {
        return Err(invalid());
    }
    let bj_id = url
        .path_segments()
        .and_then(|mut segments| segments.find(|segment| !segment.is_empty()))
        .map(str::to_ascii_lowercase)
        .filter(|id| is_bj_id(id) && !RESERVED_PATHS.contains(&id.as_str()))
        .ok_or_else(|| {
            PlatformError::InvalidUrl(format!(
                "SOOP 链接里没有主播 ID，请复制形如 play.sooplive.com/主播ID 的链接: {trimmed}"
            ))
        })?;
    let password = url
        .query_pairs()
        .find(|(key, _)| key == "pwd")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    Ok(SoopLink { bj_id, password })
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn json_body(body: &str, what: &str) -> Result<Value, PlatformError> {
    serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited(format!("SOOP {what}返回的不是 JSON，可能触发了风控"))
        } else {
            PlatformError::Parse(format!("SOOP {what}返回的数据无法解析"))
        }
    })
}

/// `get_station_status.php`: the broadcaster's nickname and station number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Station {
    pub nickname: String,
    pub station_no: Option<String>,
}

pub fn parse_station(body: &str, bj_id: &str) -> Result<Station, PlatformError> {
    let json = json_body(body, "主播接口")?;
    let data = json.get("DATA").filter(|data| data.is_object());
    let (Some(data), Some(1)) = (data, json.get("RESULT").and_then(Value::as_i64)) else {
        return Err(PlatformError::InvalidUrl(format!(
            "SOOP 没有找到主播「{bj_id}」，请检查链接"
        )));
    };
    Ok(Station {
        nickname: text(data.get("user_nick")).unwrap_or_else(|| bj_id.to_string()),
        station_no: text(data.get("station_no")),
    })
}

/// `player_live_api.php` without `type=aid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    pub is_live: bool,
    /// Broadcast number (`BNO`), set while live.
    pub broad_no: Option<String>,
    pub title: Option<String>,
}

pub fn parse_player(body: &str) -> Result<Player, PlatformError> {
    let json = json_body(body, "播放接口")?;
    let channel = json
        .get("CHANNEL")
        .filter(|channel| channel.is_object())
        .ok_or_else(|| PlatformError::Parse("SOOP 播放接口缺少 CHANNEL".to_string()))?;
    let result = channel.get("RESULT").and_then(Value::as_i64);
    let protected = text(channel.get("BPWD")).as_deref() == Some("Y");
    match result {
        Some(1) => {
            let broad_no = text(channel.get("BNO"));
            Ok(Player {
                is_live: broad_no.is_some(),
                broad_no,
                title: text(channel.get("TITLE")),
            })
        }
        Some(0) => Ok(Player {
            is_live: false,
            broad_no: None,
            title: None,
        }),
        Some(-6) => Err(PlatformError::AuthRequired(
            "SOOP 这个直播需要登录（成人或年龄限制），请在设置的平台账号中填写 Cookie".to_string(),
        )),
        Some(code) if protected => Err(PlatformError::AuthRequired(format!(
            "SOOP 这个直播间设了密码（返回码 {code}），请在链接后加上 ?pwd=密码"
        ))),
        other => Err(PlatformError::Parse(format!(
            "SOOP 播放接口返回了未知结果 {}",
            other.map_or_else(|| "（空）".to_string(), |code| code.to_string())
        ))),
    }
}

/// The `AID` stream token.
pub fn parse_aid(body: &str) -> Result<String, PlatformError> {
    let json = json_body(body, "播放接口")?;
    text(json.pointer("/CHANNEL/AID"))
        .ok_or_else(|| PlatformError::Parse("SOOP 没有返回播放凭证 AID".to_string()))
}

/// The CDN master playlist URL from `broad_stream_assign.html`.
pub fn parse_assign(body: &str) -> Result<String, PlatformError> {
    let json = json_body(body, "CDN 分配接口")?;
    text(json.get("view_url"))
        .ok_or_else(|| PlatformError::Parse("SOOP 没有返回播放地址 view_url".to_string()))
}

/// Master playlist variants, best first. Audio-only entries are dropped.
pub fn parse_variants(body: &str, base: &Url) -> Result<Vec<MasterVariant>, PlatformError> {
    let variants: Vec<MasterVariant> = parse_master(body, base)
        .into_iter()
        .filter(|variant| !variant.audio_only)
        .collect();
    if variants.is_empty() {
        return Err(PlatformError::Parse(
            "SOOP 播放列表里没有视频流".to_string(),
        ));
    }
    Ok(variants)
}

/// Turn the broadcaster and its variants into `LiveInfo`.
pub fn build_live_info(
    link: &SoopLink,
    station: &Station,
    player: &Player,
    variants: &[MasterVariant],
    quality: Quality,
) -> LiveInfo {
    let variants: Vec<StreamVariant> = variants
        .iter()
        .enumerate()
        .map(|(index, variant)| StreamVariant {
            name: variant.label(),
            quality: Quality::from_index(index),
            flv_url: None,
            hls_url: Some(variant.url.clone()),
        })
        .collect();
    let selected = select_variant_index(variants.len(), quality)
        .and_then(|index| variants.get(index))
        .filter(|_| player.is_live)
        .and_then(|variant| selected_stream(quality, variant));
    LiveInfo {
        platform: Platform::Sooplive,
        room_id: link.bj_id.clone(),
        anchor_id: station.station_no.clone(),
        live_url: Some(room_url(&link.bj_id)),
        anchor_name: station.nickname.clone(),
        title: player.title.clone(),
        is_live: selected.is_some(),
        variants: if selected.is_some() {
            variants
        } else {
            Vec::new()
        },
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling SOOP streams: the User-Agent only.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("User-Agent".to_string(), USER_AGENT_VALUE.to_string())])
}

/// `_get_pc_headers`: form POST headers with the user cookie.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded; charset=UTF-8"),
    );
    headers.insert(ORIGIN, HeaderValue::from_static(ORIGIN_VALUE));
    headers.insert(REFERER, HeaderValue::from_static(REFERER_VALUE));
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// Form body of the player API; `rtype` is `""` or `"aid"`.
pub fn player_body(bj_id: &str, password: Option<&str>, rtype: &str) -> String {
    let mut url = Url::parse("http://form.invalid/").expect("valid URL");
    url.query_pairs_mut()
        .append_pair("bid", bj_id)
        .append_pair("bno", "")
        .append_pair("type", rtype)
        .append_pair("pwd", password.unwrap_or_default())
        .append_pair("player_type", "html5")
        .append_pair("stream_type", "common")
        .append_pair("quality", "master")
        .append_pair("mode", "landing")
        .append_pair("from_api", "0")
        .append_pair("is_revive", "false");
    url.query().unwrap_or_default().to_string()
}

/// URL of `broad_stream_assign.html` for a broadcast.
pub fn assign_url(broad_no: &str) -> String {
    let mut url = Url::parse(ASSIGN_API).expect("valid assign URL");
    url.query_pairs_mut()
        .append_pair("return_type", "gcp_cdn")
        .append_pair("use_cors", "false")
        .append_pair("cors_origin_url", "play.sooplive.com")
        .append_pair("broad_key", &format!("{broad_no}-common-master-hls"))
        .append_pair("time", "3061.2892404235236");
    url.to_string()
}

/// SOOP adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct SoopAdapter {
    clients: ClientCache,
}

impl SoopAdapter {
    pub fn new() -> Self {
        SoopAdapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let station_url = format!("{STATION_API}?szBjId={}", link.bj_id);
        let response = client
            .get(&station_url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), &station_url));
        }
        let station = parse_station(&response.text().await?, &link.bj_id)?;

        let player_url = format!("{PLAYER_API}?bjid={}", link.bj_id);
        let post = |rtype: &'static str| {
            let body = player_body(&link.bj_id, link.password.as_deref(), rtype);
            let client = client.clone();
            let player_url = player_url.clone();
            async move {
                let response = client
                    .post(&player_url)
                    .headers(pc_headers(cookie)?)
                    .body(body)
                    .send()
                    .await?;
                let status = response.status();
                if !status.is_success() {
                    return Err(classify_http_status(status.as_u16(), PLAYER_API));
                }
                Ok::<String, PlatformError>(response.text().await?)
            }
        };
        let player = parse_player(&post("").await?)?;
        let Some(broad_no) = player.broad_no.clone().filter(|_| player.is_live) else {
            return Ok(build_live_info(
                &link,
                &station,
                &player,
                &[],
                options.quality,
            ));
        };

        let aid = parse_aid(&post("aid").await?)?;
        let assign = assign_url(&broad_no);
        let response = client
            .get(&assign)
            .headers(pc_headers(cookie)?)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), ASSIGN_API));
        }
        let view_url = parse_assign(&response.text().await?)?;
        let master = Url::parse(&format!("{view_url}?aid={aid}"))
            .map_err(|_| PlatformError::Parse("SOOP 播放地址无法解析".to_string()))?;
        let response = client
            .get(master.clone())
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            // The broadcast ended between the live check and the playlist.
            if status.as_u16() == 404 {
                let ended = Player {
                    is_live: false,
                    ..player
                };
                return Ok(build_live_info(
                    &link,
                    &station,
                    &ended,
                    &[],
                    options.quality,
                ));
            }
            return Err(classify_http_status(status.as_u16(), view_url.as_str()));
        }
        let base = response.url().clone();
        let variants = parse_variants(&response.text().await?, &base)?;
        Ok(build_live_info(
            &link,
            &station,
            &player,
            &variants,
            options.quality,
        ))
    }
}

impl PlatformAdapter for SoopAdapter {
    fn platform(&self) -> Platform {
        Platform::Sooplive
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

    const STATION_LIVE: &str = include_str!("../../tests/fixtures/sooplive/station_live.json");
    const STATION_MISSING: &str =
        include_str!("../../tests/fixtures/sooplive/station_missing.json");
    const PLAYER_LIVE: &str = include_str!("../../tests/fixtures/sooplive/player_live.json");
    const PLAYER_OFFLINE: &str = include_str!("../../tests/fixtures/sooplive/player_offline.json");
    const PLAYER_LOGIN: &str = include_str!("../../tests/fixtures/sooplive/player_login.json");
    const AID: &str = include_str!("../../tests/fixtures/sooplive/aid.json");
    const ASSIGN: &str = include_str!("../../tests/fixtures/sooplive/assign.json");
    const MASTER: &str = include_str!("../../tests/fixtures/sooplive/master.m3u8");

    fn base() -> Url {
        Url::parse(
            "http://live-global-cdn-v02.sooplive.com/live-stm-07/auth_master_playlist.m3u8?aid=x",
        )
        .unwrap()
    }

    #[test]
    fn recognizes_and_normalizes_links() {
        let adapter = SoopAdapter::new();
        for (input, id, normalized) in [
            (
                "https://play.sooplive.com/superbsw123/277837074",
                "superbsw123",
                "https://play.sooplive.com/superbsw123",
            ),
            (
                "https://play.sooplive.co.kr/PHONICS1",
                "phonics1",
                "https://play.sooplive.com/phonics1",
            ),
            (
                "ch.sooplive.com/phonics1",
                "phonics1",
                "https://play.sooplive.com/phonics1",
            ),
            (
                "https://play.sooplive.com/abc123?pwd=1234",
                "abc123",
                "https://play.sooplive.com/abc123?pwd=1234",
            ),
        ] {
            assert_eq!(parse_link(input).unwrap().bj_id, id, "{input}");
            assert!(adapter.matches(input), "{input}");
            assert_eq!(
                adapter.normalize_url(input).as_deref(),
                Some(normalized),
                "{input}"
            );
        }
        assert_eq!(
            parse_link("https://play.sooplive.com/abc123?pwd=1234")
                .unwrap()
                .password
                .as_deref(),
            Some("1234")
        );
        let soop = super::super::adapter_for("https://play.sooplive.com/phonics1").unwrap();
        assert_eq!(soop.platform(), Platform::Sooplive);
        assert_eq!(Platform::Sooplive.key(), "sooplive");
        assert_eq!(Platform::Sooplive.display_name(), "SOOP");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        for input in [
            "https://www.sooplive.com/",
            "https://www.sooplive.com/directory/game",
            "https://vod.sooplive.com/player/123456",
            "https://play.sooplive.com.evil.example/abc123",
            "https://notsooplive.com/abc123",
            "ftp://play.sooplive.com/abc123",
            "https://www.twitch.tv/nix",
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
    fn live_broadcast_maps_variants_by_bandwidth() {
        let link = parse_link("https://play.sooplive.com/phonics1").unwrap();
        let station = parse_station(STATION_LIVE, "phonics1").unwrap();
        assert_eq!(station.nickname, "김민교.");
        assert_eq!(station.station_no.as_deref(), Some("18003616"));
        let player = parse_player(PLAYER_LIVE).unwrap();
        assert!(player.is_live);
        assert_eq!(player.broad_no.as_deref(), Some("298828609"));
        let variants = parse_variants(MASTER, &base()).unwrap();
        assert_eq!(parse_aid(AID).unwrap(), ".A32.testaid");
        assert_eq!(
            parse_assign(ASSIGN).unwrap(),
            "http://live-global-cdn-v02.sooplive.com/live-stm-07/auth_master_playlist.m3u8"
        );
        for (quality, name, token) in [
            (Quality::Od, "1080p", "TOKEN1"),
            (Quality::Uhd, "720p", "TOKEN2"),
            (Quality::Hd, "540p", "TOKEN3"),
            (Quality::Sd, "360p", "TOKEN4"),
            (Quality::Ld, "360p", "TOKEN4"),
        ] {
            let info = build_live_info(&link, &station, &player, &variants, quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Sooplive);
            assert_eq!(info.room_id, "phonics1");
            assert_eq!(info.anchor_id.as_deref(), Some("18003616"));
            assert_eq!(info.anchor_name, "김민교.");
            assert!(info.title.as_deref().unwrap().starts_with("김민교 REAL"));
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://play.sooplive.com/phonics1")
            );
            let selected = info.selected.unwrap();
            assert_eq!(selected.variant.name, name);
            assert_eq!(selected.requested, quality);
            assert_eq!(
                selected.record_url,
                format!(
                    "http://live-global-cdn-v02.sooplive.com/live-stm-07/auth_playlist.m3u8?aid={token}"
                )
            );
            assert_eq!(
                selected.source_url(false, false).0,
                super::super::SourceFormat::Hls
            );
        }
    }

    #[test]
    fn offline_missing_and_login_only_broadcasts() {
        let link = parse_link("https://play.sooplive.com/phonics1").unwrap();
        let station = parse_station(STATION_LIVE, "phonics1").unwrap();
        let player = parse_player(PLAYER_OFFLINE).unwrap();
        assert!(!player.is_live);
        let info = build_live_info(&link, &station, &player, &[], Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        assert_eq!(
            parse_station(STATION_MISSING, "zzz").unwrap_err().kind(),
            "invalid_url"
        );
        assert_eq!(
            parse_player(PLAYER_LOGIN).unwrap_err().kind(),
            "auth_required"
        );
        // A password-protected room that rejects the password.
        let protected = r#"{"CHANNEL":{"RESULT":-3,"BPWD":"Y"}}"#;
        assert_eq!(parse_player(protected).unwrap_err().kind(), "auth_required");
        for (body, kind) in [
            ("<html>", "rate_limited"),
            ("{", "parse"),
            (r#"{"CHANNEL":{"RESULT":-99}}"#, "parse"),
            (r#"{}"#, "parse"),
        ] {
            assert_eq!(parse_player(body).unwrap_err().kind(), kind, "{body}");
        }
        assert_eq!(
            parse_aid(r#"{"CHANNEL":{"RESULT":1}}"#).unwrap_err().kind(),
            "parse"
        );
        assert_eq!(
            parse_variants("#EXTM3U\n", &base()).unwrap_err().kind(),
            "parse"
        );
        // A live broadcast without variants is not reported as live.
        let live = parse_player(PLAYER_LIVE).unwrap();
        assert!(!build_live_info(&link, &station, &live, &[], Quality::Od).is_live);
    }

    #[test]
    fn requests_carry_streamget_headers_and_parameters() {
        let headers = pc_headers(None).unwrap();
        assert_eq!(headers.get(ORIGIN).unwrap(), ORIGIN_VALUE);
        assert!(headers.get(COOKIE).is_none());
        assert_eq!(
            pc_headers(Some("PdboxTicket=1"))
                .unwrap()
                .get(COOKIE)
                .unwrap(),
            "PdboxTicket=1"
        );
        assert_eq!(pc_headers(Some("a\nb")).unwrap_err().kind(), "config");
        let body = player_body("phonics1", Some("a b"), "aid");
        let pairs: BTreeMap<String, String> = Url::parse(&format!("http://form.invalid/?{body}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(pairs["bid"], "phonics1");
        assert_eq!(pairs["type"], "aid");
        assert_eq!(pairs["pwd"], "a b");
        assert_eq!(pairs["quality"], "master");
        let assign = Url::parse(&assign_url("298828609")).unwrap();
        let query: BTreeMap<String, String> = assign.query_pairs().into_owned().collect();
        assert_eq!(query["broad_key"], "298828609-common-master-hls");
        assert_eq!(query["return_type"], "gcp_cdn");
    }
}
