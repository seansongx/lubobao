//! AcFun adapter, following StreamCap's `AcfunHandler` and StreamGet v4.0.10
//! `AcfunLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/acfun/live_stream.py`, MIT License, Copyright (c)
//! 2025 Hmily. The request flow, headers, visitor login and the bitrate
//! ordering follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * `live.acfun.cn/rest/pc-direct/user/userInfo?userId={uid}` gives the
//!   anchor's name; the profile carries `liveId` only while the anchor is
//!   live. Unknown users answer `result: 110`「用户不存在」.
//! * When live, a visitor login at `id.app.acfun.cn/rest/app/visitor/login`
//!   (with a random `_did` cookie) returns the `userId` and
//!   `acfun.api.visitor_st` that `api.kuaishouzt.com/rest/zt/live/web/startPlay`
//!   needs. Its `videoPlayRes` (a JSON string) lists FLV representations;
//!   StreamGet sorts them by bitrate, best first, and picks by position.
//!   A broadcast that ended between the two requests answers
//!   `result: 129004`「直播已关播」.

use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

/// `AcfunLiveStream.pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36 Edg/136.0.0.0";
/// User-Agent StreamGet sends with the visitor login.
const LOGIN_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:109.0) Gecko/20100101 Firefox/115.0";
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
/// Cookie StreamGet sends when the user has none.
const DEFAULT_COOKIE: &str = "__ac_nonce=064caded4009deafd8b89;";
const WEB_ORIGIN: &str = "https://live.acfun.cn";
const WEB_REFERER: &str = "https://live.acfun.cn/";
const ROOM_PAGE: &str = "https://live.acfun.cn/live/";
const USER_INFO_API: &str = "https://live.acfun.cn/rest/pc-direct/user/userInfo";
const VISITOR_LOGIN_API: &str = "https://id.app.acfun.cn/rest/app/visitor/login";
const START_PLAY_API: &str = "https://api.kuaishouzt.com/rest/zt/live/web/startPlay";
/// `userInfo` result for an unknown user.
const RESULT_USER_MISSING: i64 = 110;
/// `startPlay` result for a broadcast that has ended.
const RESULT_LIVE_ENDED: i64 = 129004;

/// Recognize a room link and return the anchor's user id, without any
/// network access.
///
/// StreamCap registers `live.acfun.cn/` and StreamGet takes the last path
/// segment as the user id; its README example is
/// `live.acfun.cn/live/179922`. Only `live.acfun.cn/live/{uid}` is accepted.
pub fn parse_link(input: &str) -> Result<String, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的 AcFun 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "live.acfun.cn" {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        // `/live/0179922` and `/live/179922` are the same user.
        ["live", uid] if is_user_id(uid) => Ok(uid.trim_start_matches('0').to_string()),
        _ => Err(invalid()),
    }
}

fn is_user_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 19
        && text.bytes().all(|b| b.is_ascii_digit())
        && text.trim_start_matches('0').parse::<u64>().is_ok()
}

/// Canonical room URL for a user id.
pub fn room_url(uid: &str) -> String {
    format!("{ROOM_PAGE}{uid}")
}

fn parse_json(body: &str, what: &str) -> Result<Value, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited(format!("AcFun {what}返回的不是 JSON，可能触发风控"))
        } else {
            PlatformError::Parse(format!("AcFun {what}返回的数据无法解析"))
        }
    })?;
    if json.is_object() {
        Ok(json)
    } else {
        Err(PlatformError::Parse(format!(
            "AcFun {what}返回的数据无法解析"
        )))
    }
}

fn result_code(json: &Value) -> Option<i64> {
    json.get("result").and_then(Value::as_i64)
}

fn error_message(json: &Value) -> String {
    json.get("error_msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or("未知错误")
        .to_string()
}

/// An error result from an AcFun API, sorted by what the message says.
fn api_error(what: &str, json: &Value) -> PlatformError {
    let code = result_code(json).unwrap_or_default();
    let detail = error_message(json);
    let message = format!("AcFun {what}返回错误 {code}：{detail}");
    if detail.contains("登录") {
        PlatformError::AuthRequired(message)
    } else if detail.contains("频繁") || detail.contains("风控") {
        PlatformError::RateLimited(message)
    } else {
        PlatformError::Parse(message)
    }
}

/// Anchor facts from `userInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub uid: String,
    pub name: String,
    /// Present only while the anchor is live (StreamGet: `'liveId' in profile`).
    pub live_id: Option<String>,
}

/// Parse a `userInfo` response for the user `uid` from the link.
pub fn parse_user_info(body: &str, uid: &str) -> Result<UserInfo, PlatformError> {
    let json = parse_json(body, "用户信息接口")?;
    match result_code(&json) {
        Some(0) => {}
        Some(RESULT_USER_MISSING) => {
            return Err(PlatformError::InvalidUrl(format!(
                "AcFun 没有找到这个用户（{}），请检查链接",
                error_message(&json)
            )));
        }
        _ => return Err(api_error("用户信息接口", &json)),
    }
    let profile = json
        .get("profile")
        .filter(|profile| profile.is_object())
        .ok_or_else(|| PlatformError::Parse("AcFun 用户信息缺少 profile".to_string()))?;
    let name = profile
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("AcFun {uid}"));
    let live_id = profile.get("liveId").map(|value| match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    });
    Ok(UserInfo {
        uid: uid.to_string(),
        name,
        live_id,
    })
}

/// Visitor credentials for `startPlay`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visitor {
    pub user_id: String,
    pub visitor_st: String,
}

/// Parse the visitor login response.
pub fn parse_visitor_login(body: &str) -> Result<Visitor, PlatformError> {
    let json = parse_json(body, "游客登录接口")?;
    if result_code(&json) != Some(0) {
        return Err(api_error("游客登录接口", &json));
    }
    let user_id = match json.get("userId") {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        _ => {
            return Err(PlatformError::Parse(
                "AcFun 游客登录没有返回 userId".to_string(),
            ));
        }
    };
    let visitor_st = json
        .get("acfun.api.visitor_st")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| PlatformError::Parse("AcFun 游客登录没有返回 visitor_st".to_string()))?;
    Ok(Visitor {
        user_id,
        visitor_st: visitor_st.to_string(),
    })
}

/// `startPlay` URL with StreamGet's query parameters.
pub fn start_play_url(visitor: &Visitor, did: &str) -> String {
    let mut url = Url::parse(START_PLAY_API).expect("valid startPlay URL");
    url.query_pairs_mut()
        .append_pair("subBiz", "mainApp")
        .append_pair("kpn", "ACFUN_APP")
        .append_pair("kpf", "PC_WEB")
        .append_pair("userId", &visitor.user_id)
        .append_pair("did", did)
        .append_pair("acfun.api.visitor_st", &visitor.visitor_st);
    url.to_string()
}

/// What `startPlay` says about a live room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartPlay {
    pub title: Option<String>,
    /// FLV variants, highest bitrate first.
    pub variants: Vec<StreamVariant>,
}

/// Parse a `startPlay` response. `None` when the broadcast has ended.
pub fn parse_start_play(body: &str) -> Result<Option<StartPlay>, PlatformError> {
    let json = parse_json(body, "取流接口")?;
    match result_code(&json) {
        Some(1) => {}
        Some(RESULT_LIVE_ENDED) => return Ok(None),
        _ => return Err(api_error("取流接口", &json)),
    }
    let data = json
        .get("data")
        .ok_or_else(|| PlatformError::Parse("AcFun 取流接口缺少 data".to_string()))?;
    let title = data
        .get("caption")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    let play_res: Value = data
        .get("videoPlayRes")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str(text).ok())
        .ok_or_else(|| PlatformError::Parse("AcFun 取流接口缺少 videoPlayRes".to_string()))?;
    let representations = play_res
        .get("liveAdaptiveManifest")
        .and_then(|manifests| manifests.get(0))
        .and_then(|manifest| manifest.get("adaptationSet"))
        .and_then(|set| set.get("representation"))
        .and_then(Value::as_array)
        .ok_or_else(|| PlatformError::Parse("AcFun 取流接口没有画质列表".to_string()))?;
    let mut streams: Vec<(f64, String, String)> = representations
        .iter()
        .filter_map(|representation| {
            let url = representation
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())?;
            let bitrate = representation
                .get("bitrate")
                .and_then(Value::as_f64)
                .unwrap_or_default();
            let name = representation
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{bitrate} kbps"));
            Some((bitrate, name, url.to_string()))
        })
        .collect();
    if streams.is_empty() {
        return Err(PlatformError::Parse(
            "AcFun 取流接口没有返回流地址".to_string(),
        ));
    }
    // StreamGet: `sorted(..., key=itemgetter('bitrate'), reverse=True)`,
    // stable for equal bitrates.
    streams.sort_by(|a, b| b.0.total_cmp(&a.0));
    let variants = streams
        .into_iter()
        .enumerate()
        .map(|(index, (_, name, url))| StreamVariant {
            name,
            quality: Quality::from_index(index),
            flv_url: Some(url),
            hls_url: None,
        })
        .collect();
    Ok(Some(StartPlay { title, variants }))
}

/// Turn the user info and `startPlay` answer into `LiveInfo`.
pub fn build_live_info(user: UserInfo, play: Option<StartPlay>, quality: Quality) -> LiveInfo {
    let (title, variants) = match play {
        Some(play) => (play.title, play.variants),
        None => (None, Vec::new()),
    };
    let selected = select_variant_index(variants.len(), quality)
        .and_then(|index| selected_stream(quality, &variants[index]));
    LiveInfo {
        platform: Platform::Acfun,
        live_url: Some(room_url(&user.uid)),
        anchor_id: Some(user.uid.clone()),
        room_id: user.uid,
        anchor_name: user.name,
        title,
        is_live: selected.is_some(),
        variants,
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling AcFun streams: User-Agent and the web Referer. User
/// cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Referer".to_string(), WEB_REFERER.to_string()),
    ])
}

/// `AcfunLiveStream.pc_headers`: Origin, Referer, User-Agent and the user
/// cookie, or StreamGet's default `__ac_nonce` cookie.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(ORIGIN, HeaderValue::from_static(WEB_ORIGIN));
    headers.insert(REFERER, HeaderValue::from_static(WEB_REFERER));
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(COOKIE, cookie_header(cookie.unwrap_or(DEFAULT_COOKIE))?);
    Ok(headers)
}

/// Random device id like StreamGet's `web_` + 16 upper-case letters or digits.
pub fn random_did() -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut did = String::from("web_");
    let mut random = random_u64();
    for index in 0..16 {
        if index == 8 {
            random = random_u64();
        }
        did.push(CHARS[(random % CHARS.len() as u64) as usize] as char);
        random /= CHARS.len() as u64;
    }
    did
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

/// AcFun adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct AcfunAdapter {
    clients: ClientCache,
}

impl AcfunAdapter {
    pub fn new() -> Self {
        AcfunAdapter::default()
    }

    async fn read(response: reqwest::Response, url: &str) -> Result<String, PlatformError> {
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), url));
        }
        Ok(response.text().await?)
    }

    async fn visitor_login(client: &reqwest::Client, did: &str) -> Result<Visitor, PlatformError> {
        let mut headers = HeaderMap::new();
        headers.insert(REFERER, HeaderValue::from_static(WEB_REFERER));
        headers.insert(USER_AGENT, HeaderValue::from_static(LOGIN_USER_AGENT));
        headers.insert(COOKIE, cookie_header(&format!("_did={did}"))?);
        let response = client
            .post(VISITOR_LOGIN_API)
            .headers(headers)
            .header(CONTENT_TYPE, FORM_CONTENT_TYPE)
            .body("sid=acfun.api.visitor")
            .send()
            .await?;
        parse_visitor_login(&Self::read(response, VISITOR_LOGIN_API).await?)
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let uid = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let mut info_url = Url::parse(USER_INFO_API).expect("valid userInfo URL");
        info_url.query_pairs_mut().append_pair("userId", &uid);
        let response = client
            .get(info_url.as_str())
            .headers(pc_headers(cookie)?)
            .send()
            .await?;
        let user = parse_user_info(&Self::read(response, info_url.as_str()).await?, &uid)?;
        if user.live_id.is_none() {
            return Ok(build_live_info(user, None, options.quality));
        }

        let did = random_did();
        let visitor = Self::visitor_login(&client, &did).await?;
        let api = start_play_url(&visitor, &did);
        let response = client
            .post(&api)
            .headers(pc_headers(cookie)?)
            .header(CONTENT_TYPE, FORM_CONTENT_TYPE)
            // The user id is all digits, so it needs no escaping.
            .body(format!("authorId={uid}&pullStreamType=FLV"))
            .send()
            .await?;
        let play = parse_start_play(&Self::read(response, START_PLAY_API).await?)?;
        Ok(build_live_info(user, play, options.quality))
    }
}

impl PlatformAdapter for AcfunAdapter {
    fn platform(&self) -> Platform {
        Platform::Acfun
    }

    fn matches(&self, url: &str) -> bool {
        parse_link(url).is_ok()
    }

    fn normalize_url(&self, url: &str) -> Option<String> {
        parse_link(url).ok().map(|uid| room_url(&uid))
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

    const USER_LIVE: &str = include_str!("../../tests/fixtures/acfun/user_info_live.json");
    const USER_OFFLINE: &str = include_str!("../../tests/fixtures/acfun/user_info_offline.json");
    const USER_MISSING: &str = include_str!("../../tests/fixtures/acfun/user_info_missing.json");
    const VISITOR_LOGIN: &str = include_str!("../../tests/fixtures/acfun/visitor_login.json");
    const PLAY_LIVE: &str = include_str!("../../tests/fixtures/acfun/start_play_live.json");
    const PLAY_OFFLINE: &str = include_str!("../../tests/fixtures/acfun/start_play_offline.json");

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let adapter = AcfunAdapter::new();
        for (input, uid) in [
            ("https://live.acfun.cn/live/179922", "179922"),
            ("  live.acfun.cn/live/179922/ ", "179922"),
            ("http://LIVE.ACFUN.CN/live/2889712?from=share#x", "2889712"),
            ("https://live.acfun.cn/live/0179922", "179922"),
        ] {
            assert_eq!(parse_link(input).unwrap(), uid, "{input}");
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            adapter
                .normalize_url("http://live.acfun.cn/live/179922?x=1")
                .as_deref(),
            Some("https://live.acfun.cn/live/179922")
        );
        let acfun = super::super::adapter_for("https://live.acfun.cn/live/179922").unwrap();
        assert_eq!(acfun.platform(), Platform::Acfun);
        assert_eq!(Platform::Acfun.key(), "acfun");
        assert_eq!(Platform::Acfun.display_name(), "Acfun");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        let adapter = AcfunAdapter::new();
        for input in [
            "https://live.acfun.cn/",
            "https://live.acfun.cn/live/",
            "https://live.acfun.cn/live/abc",
            "https://live.acfun.cn/live/1/2",
            "https://live.acfun.cn/game/179922",
            "https://live.acfun.cn/live/99999999999999999999",
            "https://live.acfun.cn/live/000",
            "https://www.acfun.cn/u/179922",
            "https://live.acfun.cn.evil.example/live/179922",
            "ftp://live.acfun.cn/live/179922",
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
    fn user_info_gives_name_and_live_state() {
        assert_eq!(
            parse_user_info(USER_LIVE, "100001").unwrap(),
            UserInfo {
                uid: "100001".to_string(),
                name: "测试主播🎤".to_string(),
                live_id: Some("liveAbC123x".to_string()),
            }
        );
        let offline = parse_user_info(USER_OFFLINE, "100002").unwrap();
        assert_eq!(offline.name, "下播主播");
        assert_eq!(offline.live_id, None);
        let nameless = USER_OFFLINE.replace("下播主播", " ");
        assert_eq!(
            parse_user_info(&nameless, "100002").unwrap().name,
            "AcFun 100002"
        );
    }

    #[test]
    fn user_info_errors_map_to_error_kinds() {
        let missing = parse_user_info(USER_MISSING, "1").unwrap_err();
        assert_eq!(missing.kind(), "invalid_url", "{missing}");
        assert!(missing.message().contains("用户不存在"), "{missing}");
        for (body, kind) in [
            ("", "rate_limited"),
            ("<html>", "rate_limited"),
            ("{\"result\":", "parse"),
            ("[]", "parse"),
            ("{\"result\":0}", "parse"),
            (
                "{\"result\":-401,\"error_msg\":\"请先登录\"}",
                "auth_required",
            ),
            ("{\"result\":500,\"error_msg\":\"服务异常\"}", "parse"),
        ] {
            assert_eq!(
                parse_user_info(body, "1").unwrap_err().kind(),
                kind,
                "{body}"
            );
        }
    }

    #[test]
    fn visitor_login_and_start_play_url() {
        let visitor = parse_visitor_login(VISITOR_LOGIN).unwrap();
        assert_eq!(visitor.user_id, "1000000285666797");
        assert!(
            visitor
                .visitor_st
                .starts_with("ChRhY2Z1bi5hcGkudmlzaXRvci5zdBJw")
        );
        let url = start_play_url(&visitor, "web_ABCDEFGH12345678");
        assert!(url.starts_with(
            "https://api.kuaishouzt.com/rest/zt/live/web/startPlay?subBiz=mainApp&kpn=ACFUN_APP&kpf=PC_WEB&userId=1000000285666797&did=web_ABCDEFGH12345678&acfun.api.visitor_st=ChRh"
        ));
        assert_eq!(
            parse_visitor_login("{\"result\":-1,\"error_msg\":\"x\"}")
                .unwrap_err()
                .kind(),
            "parse"
        );
        assert_eq!(
            parse_visitor_login("{\"result\":0,\"userId\":1}")
                .unwrap_err()
                .kind(),
            "parse"
        );
    }

    #[test]
    fn random_did_looks_like_streamget() {
        let did = random_did();
        assert_eq!(did.len(), 20);
        assert!(did.starts_with("web_"));
        assert!(
            did[4..]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        );
    }

    #[test]
    fn live_room_sorts_variants_by_bitrate_and_pads_to_five() {
        let user = parse_user_info(USER_LIVE, "100001").unwrap();
        let play = parse_start_play(PLAY_LIVE).unwrap().unwrap();
        assert_eq!(play.title.as_deref(), Some("测试🎮标题"));
        let names: Vec<&str> = play.variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["蓝光 8M", "蓝光 4M", "超清", "高清"]);
        let expected = [
            (Quality::Od, "GameAvcFhdL3"),
            (Quality::Uhd, "hd4000"),
            (Quality::Hd, "hd2000"),
            (Quality::Sd, "sd1000"),
            (Quality::Ld, "sd1000"),
        ];
        for (quality, stream) in expected {
            let info = build_live_info(user.clone(), Some(play.clone()), quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Acfun);
            assert_eq!(info.room_id, "100001");
            assert_eq!(info.anchor_id.as_deref(), Some("100001"));
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://live.acfun.cn/live/100001")
            );
            assert_eq!(info.anchor_name, "测试主播🎤");
            assert_eq!(info.variants.len(), 4);
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert!(
                selected
                    .record_url
                    .contains(&format!("kszt_testStream_{stream}.flv?auth_key=")),
                "{quality}: {}",
                selected.record_url
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
        }
    }

    #[test]
    fn ended_broadcasts_and_offline_rooms_are_not_live() {
        assert_eq!(parse_start_play(PLAY_OFFLINE).unwrap(), None);
        let user = parse_user_info(USER_LIVE, "100001").unwrap();
        let info = build_live_info(user, None, Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        assert_eq!(info.title, None);

        for (body, kind) in [
            ("", "rate_limited"),
            ("{\"result\":1}", "parse"),
            ("{\"result\":1,\"data\":{\"videoPlayRes\":\"{}\"}}", "parse"),
            (
                "{\"result\":1,\"data\":{\"videoPlayRes\":\"{\\\"liveAdaptiveManifest\\\":[{\\\"adaptationSet\\\":{\\\"representation\\\":[]}}]}\"}}",
                "parse",
            ),
            (
                "{\"result\":380023,\"error_msg\":\"请求过于频繁\"}",
                "rate_limited",
            ),
        ] {
            assert_eq!(parse_start_play(body).unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn page_headers_use_the_user_cookie_or_streamget_default() {
        assert_eq!(
            pc_headers(None).unwrap().get(COOKIE).unwrap(),
            DEFAULT_COOKIE
        );
        assert_eq!(
            pc_headers(Some("acPasstoken=1"))
                .unwrap()
                .get(COOKIE)
                .unwrap(),
            "acPasstoken=1"
        );
        assert_eq!(pc_headers(Some("a=1\nb=2")).unwrap_err().kind(), "config");
        assert_eq!(
            pc_headers(None).unwrap().get(ORIGIN).unwrap(),
            "https://live.acfun.cn"
        );
    }
}
