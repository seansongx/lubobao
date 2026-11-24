//! Twitch adapter, following StreamCap's `TwitchHandler` and StreamGet
//! v4.0.10 `TwitchLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/twitch/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The GQL request headers and web client id, the
//! `streamPlaybackAccessToken` query, the `usher.ttvnw.net` playlist request
//! and its parameters, and picking a variant from the master playlist sorted
//! by bandwidth follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * One GQL request to `gql.twitch.tv/gql` asks for the channel (`user`:
//!   display name, current `stream`, title) and the playback access token.
//!   StreamGet sends two requests and reads the channel through the
//!   persisted `ComscoreStreamingQuery` hash; a plain query does not break
//!   when Twitch retires that hash. `user: null` means the channel does not
//!   exist; `stream: null` means offline.
//! * `usher.ttvnw.net/api/channel/hls/{login}.m3u8?sig=…&token=…` returns the
//!   master playlist. Each variant is a media playlist that FFmpeg records.
//!   A 404 here means the stream ended between the two requests.
//!
//! Twitch is not reachable from mainland China; the proxy in `FetchOptions`
//! is used for both requests.

use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::header::{
    ACCEPT_LANGUAGE, CONTENT_TYPE, COOKIE, HeaderMap, HeaderName, HeaderValue, REFERER, USER_AGENT,
};
use serde_json::{Value, json};

use super::hls::{MasterVariant, parse_master};
use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

/// `TwitchLiveStream._get_pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:109.0) Gecko/20100101 Firefox/115.0";
/// Twitch's public web client id, as StreamGet sends it.
const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_API: &str = "https://gql.twitch.tv/gql";
const USHER_API: &str = "https://usher.ttvnw.net/api/channel/hls";
const REFERER_VALUE: &str = "https://www.twitch.tv/";

/// Channel info and the playback token in one query.
const CHANNEL_QUERY: &str = "query LubobaoChannel($login: String!) { user(login: $login) { id login displayName stream { id type } broadcastSettings { title } } streamPlaybackAccessToken(channelName: $login, params: {platform: \"web\", playerBackend: \"mediaplayer\", playerType: \"site\"}) { value signature authorization { isForbidden forbiddenReasonCode } } }";

/// First path segments of twitch.tv pages that are not channels.
const RESERVED_PATHS: [&str; 30] = [
    "directory",
    "videos",
    "settings",
    "subscriptions",
    "inventory",
    "wallet",
    "drops",
    "search",
    "downloads",
    "jobs",
    "turbo",
    "friends",
    "messages",
    "payments",
    "prime",
    "store",
    "signup",
    "login",
    "logout",
    "p",
    "popout",
    "embed",
    "clip",
    "following",
    "u",
    "team",
    "broadcast",
    "dashboard",
    "event",
    "collections",
];

/// Channel logins: letters, digits and `_`, at most 25 characters.
fn is_login(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 25
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A recognized Twitch channel; the login is lowercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwitchLink {
    pub login: String,
}

impl TwitchLink {
    /// Canonical URL used to detect duplicate tasks.
    pub fn normalized_url(&self) -> String {
        channel_url(&self.login)
    }
}

/// Channel page.
pub fn channel_url(login: &str) -> String {
    format!("https://www.twitch.tv/{login}")
}

/// Recognize a Twitch link without any network access.
///
/// StreamCap registers `https://.*\.twitch\.tv/` and StreamGet takes the last
/// path segment as the channel. The first segment is used instead, so links
/// to a channel's sub pages (`/{login}/videos`) still name the channel, and
/// the embedded player link `player.twitch.tv/?channel={login}` is accepted.
pub fn parse_link(input: &str) -> Result<TwitchLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的 Twitch 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "twitch.tv" && !host.ends_with(".twitch.tv") {
        return Err(invalid());
    }
    let login = if host == "player.twitch.tv" {
        url.query_pairs()
            .find(|(key, _)| key == "channel")
            .map(|(_, value)| value.trim().to_string())
    } else {
        url.path_segments()
            .and_then(|mut segments| segments.find(|segment| !segment.is_empty()))
            .map(str::to_string)
    };
    let login = login
        .map(|login| login.to_ascii_lowercase())
        .filter(|login| is_login(login) && !RESERVED_PATHS.contains(&login.as_str()));
    match login {
        Some(login) => Ok(TwitchLink { login }),
        None => Err(PlatformError::InvalidUrl(format!(
            "Twitch 链接里没有频道名，请复制形如 twitch.tv/频道名 的链接: {trimmed}"
        ))),
    }
}

/// What GQL says about the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub login: String,
    pub user_id: Option<String>,
    pub display_name: Option<String>,
    pub title: Option<String>,
    pub is_live: bool,
    /// Playback token `(value, signature)` when Twitch allows playback.
    pub token: Option<(String, String)>,
    /// `forbiddenReasonCode` when Twitch refuses playback.
    pub forbidden: Option<String>,
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Parse the GQL answer.
pub fn parse_gql(body: &str, login: &str) -> Result<Channel, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited("Twitch 接口返回的不是 JSON，可能触发了风控".to_string())
        } else {
            PlatformError::Parse("Twitch 接口返回的数据无法解析".to_string())
        }
    })?;
    let data = json.get("data").filter(|data| data.is_object());
    let Some(data) = data else {
        let message = json
            .get("errors")
            .and_then(|errors| errors.get(0))
            .and_then(|error| error.get("message"))
            .or_else(|| json.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Err(PlatformError::RateLimited(format!(
            "Twitch 接口返回错误: {message}"
        )));
    };
    let Some(user) = data.get("user").filter(|user| user.is_object()) else {
        return Err(PlatformError::InvalidUrl(format!(
            "Twitch 没有频道「{login}」，请检查链接"
        )));
    };
    let token = data
        .get("streamPlaybackAccessToken")
        .filter(|token| token.is_object());
    let forbidden = token
        .and_then(|token| token.get("authorization"))
        .filter(|auth| auth.get("isForbidden").and_then(Value::as_bool) == Some(true))
        .map(|auth| text(auth.get("forbiddenReasonCode")).unwrap_or_else(|| "UNKNOWN".into()));
    let token =
        token.and_then(|token| Some((text(token.get("value"))?, text(token.get("signature"))?)));
    Ok(Channel {
        login: text(user.get("login"))
            .map(|login| login.to_ascii_lowercase())
            .unwrap_or_else(|| login.to_string()),
        user_id: text(user.get("id")),
        display_name: text(user.get("displayName")),
        title: user
            .get("broadcastSettings")
            .and_then(|settings| text(settings.get("title"))),
        is_live: user.get("stream").is_some_and(Value::is_object),
        token,
        forbidden,
    })
}

/// What usher answered for the master playlist.
#[derive(Debug, Clone, PartialEq)]
pub enum Usher {
    Variants(Vec<MasterVariant>),
    /// 404: the stream ended after the GQL request.
    Offline,
}

/// Parse usher's answer. Errors come as `[{"error": …, "error_code": …}]`.
pub fn parse_usher(status: u16, body: &str, base: &Url) -> Result<Usher, PlatformError> {
    if status == 404 {
        return Ok(Usher::Offline);
    }
    if !(200..300).contains(&status) {
        let error: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let first = error.get(0).unwrap_or(&error);
        let message = text(first.get("error")).unwrap_or_default();
        let code = text(first.get("error_code")).unwrap_or_default();
        if status == 403 && code.contains("geoblock") {
            return Err(PlatformError::Config(format!(
                "Twitch 不允许代理节点所在的地区观看这个频道（{message}），请换其他地区的节点"
            )));
        }
        if status == 403 && !code.is_empty() {
            return Err(PlatformError::AuthRequired(format!(
                "Twitch 拒绝播放这个频道: {message}（{code}）"
            )));
        }
        return Err(classify_http_status(
            status,
            &format!("Twitch 播放列表 {message}"),
        ));
    }
    let variants: Vec<MasterVariant> = parse_master(body, base)
        .into_iter()
        .filter(|variant| !variant.audio_only)
        .collect();
    if variants.is_empty() {
        return Err(PlatformError::Parse(
            "Twitch 播放列表里没有视频流".to_string(),
        ));
    }
    Ok(Usher::Variants(variants))
}

/// Turn the channel and its variants into `LiveInfo`.
pub fn build_live_info(
    channel: &Channel,
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
        .filter(|_| channel.is_live)
        .and_then(|variant| selected_stream(quality, variant));
    LiveInfo {
        platform: Platform::Twitch,
        room_id: channel.login.clone(),
        anchor_id: channel.user_id.clone(),
        live_url: Some(channel_url(&channel.login)),
        anchor_name: channel
            .display_name
            .clone()
            .unwrap_or_else(|| channel.login.clone()),
        title: channel.title.clone(),
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

/// Headers for pulling Twitch streams: the User-Agent only. User cookies are
/// never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("User-Agent".to_string(), USER_AGENT_VALUE.to_string())])
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

/// `generate_random_string(n).lower()`: lowercase letters and digits.
fn random_id(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut id = String::with_capacity(len);
    while id.len() < len {
        let mut random = random_u64();
        for _ in 0..8 {
            if id.len() == len {
                break;
            }
            id.push(CHARS[(random % CHARS.len() as u64) as usize] as char);
            random /= CHARS.len() as u64;
        }
    }
    id
}

/// `_get_pc_headers`: web client id, a random device id, the user cookie.
pub fn gql_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US"));
    headers.insert(REFERER, HeaderValue::from_static(REFERER_VALUE));
    headers.insert(
        HeaderName::from_static("client-id"),
        HeaderValue::from_static(CLIENT_ID),
    );
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain;charset=UTF-8"),
    );
    headers.insert(
        HeaderName::from_static("device-id"),
        HeaderValue::from_str(&random_id(16)).expect("ascii device id"),
    );
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// Body of the GQL request.
pub fn gql_body(login: &str) -> String {
    json!({
        "operationName": "LubobaoChannel",
        "query": CHANNEL_QUERY,
        "variables": { "login": login },
    })
    .to_string()
}

/// Usher master playlist URL with StreamGet's parameters.
pub fn usher_url(login: &str, token: &str, signature: &str, session: &str, p: u64) -> String {
    let mut url = Url::parse(&format!("{USHER_API}/{login}.m3u8")).expect("valid usher URL");
    url.query_pairs_mut()
        .append_pair("acmb", "e30=")
        .append_pair("allow_audio_only", "true")
        .append_pair("allow_source", "true")
        .append_pair("browser_family", "firefox")
        .append_pair("browser_version", "124.0")
        .append_pair("cdm", "wv")
        .append_pair("fast_bread", "true")
        .append_pair("os_name", "Windows")
        .append_pair("os_version", "NT 10.0")
        .append_pair("p", &p.to_string())
        .append_pair("platform", "web")
        .append_pair("play_session_id", session)
        .append_pair("player_backend", "mediaplayer")
        .append_pair("player_version", "1.28.0-rc.1")
        .append_pair("playlist_include_framerate", "true")
        .append_pair("reassignments_supported", "true")
        .append_pair("sig", signature)
        .append_pair("token", token)
        .append_pair("transcode_mode", "cbr_v1");
    url.to_string()
}

/// Twitch adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct TwitchAdapter {
    clients: ClientCache,
}

impl TwitchAdapter {
    pub fn new() -> Self {
        TwitchAdapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let response = client
            .post(GQL_API)
            .headers(gql_headers(cookie)?)
            .body(gql_body(&link.login))
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() && !body.trim_start().starts_with('{') {
            return Err(classify_http_status(status.as_u16(), GQL_API));
        }
        let channel = parse_gql(&body, &link.login)?;
        if !channel.is_live {
            return Ok(build_live_info(&channel, &[], options.quality));
        }
        if let Some(reason) = &channel.forbidden {
            return Err(PlatformError::AuthRequired(format!(
                "Twitch 不允许播放这个频道（{reason}），可能需要订阅或登录"
            )));
        }
        let Some((token, signature)) = &channel.token else {
            return Err(PlatformError::Parse("Twitch 没有返回播放凭证".to_string()));
        };
        let playlist = usher_url(
            &channel.login,
            token,
            signature,
            &format!("{:016x}{:016x}", random_u64(), random_u64()),
            random_u64() % 9_000_000 + 1_000_000,
        );
        let response = client
            .get(&playlist)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        let status = response.status().as_u16();
        let base = response.url().clone();
        let body = response.text().await?;
        match parse_usher(status, &body, &base)? {
            Usher::Variants(variants) => Ok(build_live_info(&channel, &variants, options.quality)),
            Usher::Offline => {
                let offline = Channel {
                    is_live: false,
                    ..channel
                };
                Ok(build_live_info(&offline, &[], options.quality))
            }
        }
    }
}

impl PlatformAdapter for TwitchAdapter {
    fn platform(&self) -> Platform {
        Platform::Twitch
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

    const GQL_LIVE: &str = include_str!("../../tests/fixtures/twitch/gql_live.json");
    const GQL_OFFLINE: &str = include_str!("../../tests/fixtures/twitch/gql_offline.json");
    const GQL_MISSING: &str = include_str!("../../tests/fixtures/twitch/gql_missing.json");
    const USHER_LIVE: &str = include_str!("../../tests/fixtures/twitch/usher_live.m3u8");
    const USHER_OFFLINE: &str = include_str!("../../tests/fixtures/twitch/usher_offline.json");

    fn base() -> Url {
        Url::parse("https://usher.ttvnw.net/api/channel/hls/nix.m3u8?sig=x").unwrap()
    }

    fn live_variants() -> Vec<MasterVariant> {
        match parse_usher(200, USHER_LIVE, &base()).unwrap() {
            Usher::Variants(variants) => variants,
            Usher::Offline => panic!("offline"),
        }
    }

    #[test]
    fn recognizes_and_normalizes_channel_links() {
        let adapter = TwitchAdapter::new();
        for (input, login) in [
            ("https://www.twitch.tv/nix", "nix"),
            (
                "https://www.twitch.tv/MontanaBlack88?sr=a",
                "montanablack88",
            ),
            ("twitch.tv/some_user/videos", "some_user"),
            ("https://m.twitch.tv/nix/", "nix"),
            (
                "https://player.twitch.tv/?channel=Nix&parent=example.com",
                "nix",
            ),
        ] {
            assert_eq!(parse_link(input).unwrap().login, login, "{input}");
            assert!(adapter.matches(input), "{input}");
            assert_eq!(
                adapter.normalize_url(input),
                Some(format!("https://www.twitch.tv/{login}")),
                "{input}"
            );
        }
        let twitch = super::super::adapter_for("https://www.twitch.tv/nix").unwrap();
        assert_eq!(twitch.platform(), Platform::Twitch);
        assert_eq!(Platform::Twitch.key(), "twitch");
        assert_eq!(Platform::Twitch.display_name(), "Twitch");
    }

    #[test]
    fn rejects_pages_that_are_not_channels_and_other_sites() {
        for input in [
            "https://www.twitch.tv/",
            "https://www.twitch.tv/directory/category/just-chatting",
            "https://www.twitch.tv/videos/123",
            "https://www.twitch.tv/bad-name",
            "https://player.twitch.tv/?video=123",
            "https://www.twitch.tv.evil.example/nix",
            "https://nottwitch.tv/nix",
            "ftp://www.twitch.tv/nix",
            "https://www.huya.com/lpl",
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
    fn live_channel_maps_variants_to_qualities_by_bandwidth() {
        let channel = parse_gql(GQL_LIVE, "nix").unwrap();
        assert!(channel.is_live);
        assert_eq!(channel.display_name.as_deref(), Some("Nix"));
        assert_eq!(channel.user_id.as_deref(), Some("67708794"));
        assert!(channel.forbidden.is_none());
        assert!(channel.token.is_some());
        let variants = live_variants();
        // Audio only is left out.
        let names: Vec<String> = variants.iter().map(MasterVariant::label).collect();
        assert_eq!(names, ["1080p60", "720p60", "480p30", "360p30", "160p30"]);
        for (quality, name, token) in [
            (Quality::Od, "1080p60", "TOKEN1"),
            (Quality::Uhd, "720p60", "TOKEN2"),
            (Quality::Hd, "480p30", "TOKEN3"),
            (Quality::Sd, "360p30", "TOKEN4"),
            (Quality::Ld, "160p30", "TOKEN5"),
        ] {
            let info = build_live_info(&channel, &variants, quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Twitch);
            assert_eq!(info.room_id, "nix");
            assert_eq!(info.anchor_name, "Nix");
            assert_eq!(info.live_url.as_deref(), Some("https://www.twitch.tv/nix"));
            assert!(info.title.as_deref().unwrap().starts_with("Yandex vs 1w"));
            let selected = info.selected.unwrap();
            assert_eq!(selected.variant.name, name);
            assert_eq!(selected.requested, quality);
            assert_eq!(
                selected.record_url,
                format!("https://usw21.playlist.ttvnw.net/v1/playlist/{token}.m3u8")
            );
            assert_eq!(
                selected.source_url(false, false).0,
                super::super::SourceFormat::Hls
            );
        }
        // A short list repeats its last variant for the lower tiers.
        let info = build_live_info(&channel, &variants[..2], Quality::Ld);
        assert_eq!(info.selected.unwrap().variant.name, "720p60");
    }

    #[test]
    fn offline_and_missing_channels() {
        let channel = parse_gql(GQL_OFFLINE, "shroud").unwrap();
        assert!(!channel.is_live);
        let info = build_live_info(&channel, &[], Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert_eq!(info.anchor_name, "shroud");
        assert_eq!(info.anchor_id.as_deref(), Some("37402112"));
        let error = parse_gql(GQL_MISSING, "nonexistentchannelzz9q").unwrap_err();
        assert_eq!(error.kind(), "invalid_url", "{error}");
        // The stream ended between the GQL request and the playlist request.
        assert_eq!(
            parse_usher(404, USHER_OFFLINE, &base()).unwrap(),
            Usher::Offline
        );
        // A live channel without playable variants is not reported as live.
        let live = parse_gql(GQL_LIVE, "nix").unwrap();
        assert!(!build_live_info(&live, &[], Quality::Od).is_live);
    }

    #[test]
    fn refusals_and_broken_answers_map_to_error_kinds() {
        let forbidden = GQL_LIVE.replace("\"isForbidden\":false", "\"isForbidden\":true");
        let forbidden = forbidden.replace(
            "\"forbiddenReasonCode\":\"NONE\"",
            "\"forbiddenReasonCode\":\"GEOBLOCK\"",
        );
        assert_eq!(
            parse_gql(&forbidden, "nix").unwrap().forbidden.as_deref(),
            Some("GEOBLOCK")
        );
        for (body, kind) in [
            ("<html>", "rate_limited"),
            ("{", "parse"),
            (
                r#"{"errors":[{"message":"service timeout"}]}"#,
                "rate_limited",
            ),
        ] {
            assert_eq!(parse_gql(body, "nix").unwrap_err().kind(), kind, "{body}");
        }
        for (status, body, kind) in [
            (
                403,
                r#"[{"error":"Content is geoblocked","error_code":"content_geoblocked"}]"#,
                "config",
            ),
            (
                403,
                r#"[{"error":"Bad auth token","error_code":"nauth_token_invalid"}]"#,
                "auth_required",
            ),
            (500, "", "network"),
            (200, "#EXTM3U\n", "parse"),
        ] {
            assert_eq!(
                parse_usher(status, body, &base()).unwrap_err().kind(),
                kind,
                "{status} {body}"
            );
        }
    }

    #[test]
    fn requests_carry_streamget_headers_and_parameters() {
        let headers = gql_headers(None).unwrap();
        assert_eq!(headers.get("client-id").unwrap(), CLIENT_ID);
        let device = headers.get("device-id").unwrap().to_str().unwrap();
        assert_eq!(device.len(), 16);
        assert!(
            device
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        );
        assert!(headers.get(COOKIE).is_none());
        assert_eq!(
            gql_headers(Some("auth-token=1"))
                .unwrap()
                .get(COOKIE)
                .unwrap(),
            "auth-token=1"
        );
        assert_eq!(gql_headers(Some("a\nb")).unwrap_err().kind(), "config");
        let body: Value = serde_json::from_str(&gql_body("nix")).unwrap();
        assert_eq!(body["variables"]["login"], "nix");
        let url = Url::parse(&usher_url("nix", "{\"a\":1}", "abc", "s", 1234567)).unwrap();
        assert_eq!(url.path(), "/api/channel/hls/nix.m3u8");
        let query: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(query["sig"], "abc");
        assert_eq!(query["token"], "{\"a\":1}");
        assert_eq!(query["allow_source"], "true");
        assert_eq!(query["p"], "1234567");
    }
}
