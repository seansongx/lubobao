//! Inke (映客直播) adapter, following StreamCap's `InkeHandler` and
//! StreamGet v4.0.10 `InkeLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/inke/live_stream.py`, MIT License, Copyright (c) 2025
//! Hmily. The link form (`uid` and `id` query parameters), the request
//! headers and the `live_share_pc` anchor lookup follow that file; see
//! `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * `service.inke.cn/api/live/now_publish?id={uid}` returns the anchor's
//!   current broadcast (`live`) with its live id, title and a signed FLV URL
//!   (`stream_addr`), or `live: null` when the anchor is not live.
//!   StreamGet reads the stream from `live_share_pc`'s `live_addr`, which the
//!   PC API no longer returns, and only ever looks at the broadcast whose id
//!   is in the link, so a new broadcast (new live id) would never be seen.
//! * `webapi.busi.inke.cn/web/live_share_pc?uid={uid}&id={live id}` gives the
//!   anchor's nick (`media_info.nick`). An unknown anchor or live id answers
//!   with a placeholder user (`inke_id` 0, nick `inke0`).
//!
//! Inke offers one stream whatever the requested quality, so every tier
//! records the same FLV, as StreamGet does with its single stream.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header, user_cookie,
};

/// `BaseLiveStream._get_pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36 Edg/121.0.0.0";
const ACCEPT_LANGUAGE_VALUE: &str = "zh-CN,zh;q=0.8,zh-TW;q=0.7,zh-HK;q=0.5,en-US;q=0.3,en;q=0.2";
const ROOM_PAGE: &str = "https://www.inke.cn/liveroom/index.html";
const NOW_PUBLISH_API: &str = "https://service.inke.cn/api/live/now_publish";
const SHARE_API: &str = "https://webapi.busi.inke.cn/web/live_share_pc";
/// `live_share_pc` error code for "当前用户无直播" (asked without a live id).
const SHARE_NO_LIVE: i64 = 1_099_999_920;
/// Name of the only variant.
const VARIANT_NAME: &str = "原画";

/// A recognized Inke link: the anchor (`uid`) and, when given, the broadcast
/// (`id`, or `liveid` in app share links).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InkeLink {
    pub uid: String,
    pub live_id: Option<String>,
}

impl InkeLink {
    /// Canonical URL used to detect duplicate tasks: the anchor only, since
    /// every broadcast has its own live id.
    pub fn normalized_url(&self) -> String {
        format!("{ROOM_PAGE}?uid={}", self.uid)
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Recognize an Inke link without any network access.
///
/// StreamCap registers `inke.cn/`; StreamGet reads the `uid` and `id` query
/// parameters, as in its example
/// `www.inke.cn/liveroom/index.html?uid=22954469&id=1720860391070904`.
/// Any `inke.cn` page with a numeric `uid` is accepted, which also covers app
/// share links (`…/app/hot/live?uid=…&liveid=…`).
pub fn parse_link(input: &str) -> Result<InkeLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的映客直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "inke.cn" && !host.ends_with(".inke.cn") {
        return Err(invalid());
    }
    let query = |key: &str| {
        url.query_pairs()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.trim().to_string())
    };
    let uid = query("uid").filter(|uid| is_digits(uid) && uid != "0");
    let Some(uid) = uid else {
        return Err(PlatformError::InvalidUrl(format!(
            "映客链接里没有主播 uid，请复制直播间页面的完整链接: {trimmed}"
        )));
    };
    let live_id = query("id")
        .or_else(|| query("liveid"))
        .filter(|id| is_digits(id) && id != "0");
    Ok(InkeLink { uid, live_id })
}

/// The anchor's current broadcast from `now_publish`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentLive {
    pub live_id: String,
    pub title: Option<String>,
    pub flv_url: String,
}

fn parse_json(body: &str, api: &str) -> Result<Value, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited(format!("映客{api}返回的不是 JSON，可能触发风控"))
        } else {
            PlatformError::Parse(format!("映客{api}返回的数据无法解析"))
        }
    })?;
    if json.is_object() {
        Ok(json)
    } else {
        Err(PlatformError::Parse(format!("映客{api}返回的数据无法解析")))
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Parse a `now_publish` answer: the current broadcast, or `None` when the
/// anchor is not live.
pub fn parse_now_publish(body: &str) -> Result<Option<CurrentLive>, PlatformError> {
    let json = parse_json(body, "开播接口")?;
    match json.get("dm_error").and_then(Value::as_i64) {
        Some(0) => {}
        Some(499) => {
            return Err(PlatformError::InvalidUrl(
                "映客不认识链接里的主播 uid，请检查链接".to_string(),
            ));
        }
        _ => {
            let message = text(json.get("error_msg")).unwrap_or_default();
            return Err(PlatformError::Parse(format!(
                "映客开播接口返回错误 {}: {message}",
                json.get("dm_error").cloned().unwrap_or(Value::Null)
            )));
        }
    }
    let Some(live) = json.get("live").filter(|live| live.is_object()) else {
        return Ok(None);
    };
    if live.get("status").and_then(Value::as_i64) != Some(1) {
        return Ok(None);
    }
    let live_id = text(live.get("id"))
        .filter(|id| is_digits(id))
        .ok_or_else(|| PlatformError::Parse("映客开播接口缺少直播 ID".to_string()))?;
    let flv_url = text(live.get("stream_addr"))
        .ok_or_else(|| PlatformError::Parse("映客开播接口没有返回流地址".to_string()))?;
    Ok(Some(CurrentLive {
        live_id,
        title: text(live.get("name")),
        flv_url,
    }))
}

/// Parse a `live_share_pc` answer: the anchor's nick. `None` when Inke has
/// nothing to say about the anchor (asked without a live id while offline).
/// A placeholder or different user means the link is wrong.
pub fn parse_share(body: &str, uid: &str) -> Result<Option<String>, PlatformError> {
    let json = parse_json(body, "直播间接口")?;
    match json.get("error_code").and_then(Value::as_i64) {
        Some(0) => {}
        Some(SHARE_NO_LIVE) => return Ok(None),
        _ => {
            let message = text(json.get("message")).unwrap_or_default();
            return Err(PlatformError::Parse(format!(
                "映客直播间接口返回错误 {}: {message}",
                json.get("error_code").cloned().unwrap_or(Value::Null)
            )));
        }
    }
    let media = json
        .get("data")
        .and_then(|data| data.get("media_info"))
        .ok_or_else(|| PlatformError::Parse("映客直播间接口缺少主播信息".to_string()))?;
    if text(media.get("inke_id")).as_deref() != Some(uid) {
        return Err(PlatformError::InvalidUrl(
            "映客没有找到这个直播间，请检查链接里的 uid 和 id".to_string(),
        ));
    }
    Ok(text(media.get("nick")))
}

/// Turn the anchor, the current broadcast and the nick into `LiveInfo`.
pub fn build_live_info(
    link: &InkeLink,
    live: Option<CurrentLive>,
    nick: Option<String>,
    quality: Quality,
) -> LiveInfo {
    let variant = live.as_ref().map(|live| StreamVariant {
        name: VARIANT_NAME.to_string(),
        quality: Some(Quality::Od),
        flv_url: Some(live.flv_url.clone()),
        hls_url: None,
    });
    let selected = variant.as_ref().map(|variant| SelectedStream {
        requested: quality,
        variant: variant.clone(),
        record_url: live
            .as_ref()
            .map(|live| live.flv_url.clone())
            .unwrap_or_default(),
    });
    let live_id = live
        .as_ref()
        .map(|live| live.live_id.as_str())
        .or(link.live_id.as_deref());
    let live_url = match live_id {
        Some(id) => format!("{ROOM_PAGE}?uid={}&id={id}", link.uid),
        None => link.normalized_url(),
    };
    LiveInfo {
        platform: Platform::Inke,
        room_id: link.uid.clone(),
        anchor_id: Some(link.uid.clone()),
        live_url: Some(live_url),
        anchor_name: nick.unwrap_or_else(|| format!("映客 {}", link.uid)),
        title: live.and_then(|live| live.title),
        is_live: selected.is_some(),
        variants: variant.into_iter().collect(),
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling Inke streams: only the User-Agent (the signed URL
/// needs no Referer). User cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("User-Agent".to_string(), USER_AGENT_VALUE.to_string())])
}

/// `BaseLiveStream._get_pc_headers`: User-Agent, Accept-Language and the
/// user cookie.
pub fn pc_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
    );
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, cookie_header(cookie)?);
    }
    Ok(headers)
}

/// URL of the `now_publish` request.
pub fn now_publish_url(uid: &str) -> String {
    let mut url = Url::parse(NOW_PUBLISH_API).expect("valid now_publish URL");
    url.query_pairs_mut().append_pair("id", uid);
    url.to_string()
}

/// URL of the `live_share_pc` request, with StreamGet's `_t` timestamp.
pub fn share_url(uid: &str, live_id: Option<&str>, timestamp: u64) -> String {
    let mut url = Url::parse(SHARE_API).expect("valid live_share_pc URL");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("uid", uid);
        if let Some(live_id) = live_id {
            query.append_pair("id", live_id);
        }
        query.append_pair("_t", &timestamp.to_string());
    }
    url.to_string()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Inke adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct InkeAdapter {
    clients: ClientCache,
}

impl InkeAdapter {
    pub fn new() -> Self {
        InkeAdapter::default()
    }

    async fn get(
        client: &reqwest::Client,
        url: &str,
        cookie: Option<&str>,
    ) -> Result<String, PlatformError> {
        let response = client.get(url).headers(pc_headers(cookie)?).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), url));
        }
        Ok(response.text().await?)
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let body = Self::get(&client, &now_publish_url(&link.uid), cookie).await?;
        let live = parse_now_publish(&body)?;

        let live_id = live
            .as_ref()
            .map(|live| live.live_id.clone())
            .or_else(|| link.live_id.clone());
        let share = share_url(&link.uid, live_id.as_deref(), now_secs());
        let nick = match Self::get(&client, &share, cookie).await {
            Ok(body) => parse_share(&body, &link.uid),
            Err(error) => Err(error),
        };
        let nick = match nick {
            Ok(nick) => nick,
            // While live the nick is only shown, so a failed lookup does not
            // fail the check.
            Err(_) if live.is_some() => None,
            Err(error) => return Err(error),
        };
        Ok(build_live_info(&link, live, nick, options.quality))
    }
}

impl PlatformAdapter for InkeAdapter {
    fn platform(&self) -> Platform {
        Platform::Inke
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

    const NOW_LIVE: &str = include_str!("../../tests/fixtures/inke/now_publish_live.json");
    const NOW_OFFLINE: &str = include_str!("../../tests/fixtures/inke/now_publish_offline.json");
    const NOW_BAD_UID: &str = include_str!("../../tests/fixtures/inke/now_publish_bad_uid.json");
    const SHARE_LIVE: &str = include_str!("../../tests/fixtures/inke/share_live.json");
    const SHARE_ENDED: &str = include_str!("../../tests/fixtures/inke/share_ended.json");
    const SHARE_MISSING: &str = include_str!("../../tests/fixtures/inke/share_missing.json");
    const SHARE_NO_LIVE_BODY: &str = include_str!("../../tests/fixtures/inke/share_no_live.json");

    fn link(uid: &str, live_id: Option<&str>) -> InkeLink {
        InkeLink {
            uid: uid.to_string(),
            live_id: live_id.map(str::to_string),
        }
    }

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let adapter = InkeAdapter::new();
        let cases = [
            (
                "https://www.inke.cn/liveroom/index.html?uid=22954469&id=1720860391070904",
                "22954469",
                Some("1720860391070904"),
            ),
            (
                "  www.inke.cn/liveroom/index.html?uid=22954469 ",
                "22954469",
                None,
            ),
            (
                "https://mlive2.inke.cn/app/hot/live?uid=774870442&liveid=1795483117403604&ctime=1",
                "774870442",
                Some("1795483117403604"),
            ),
            (
                "http://h5.inke.cn/app/room/share?uid=1&liveid=2#x",
                "1",
                Some("2"),
            ),
        ];
        for (input, uid, live_id) in cases {
            let parsed = parse_link(input).unwrap();
            assert_eq!(parsed, link(uid, live_id), "{input}");
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            adapter
                .normalize_url(
                    "https://www.inke.cn/liveroom/index.html?uid=22954469&id=1720860391070904"
                )
                .as_deref(),
            Some("https://www.inke.cn/liveroom/index.html?uid=22954469")
        );
        let inke = super::super::adapter_for(
            "https://www.inke.cn/liveroom/index.html?uid=22954469&id=1720860391070904",
        )
        .unwrap();
        assert_eq!(inke.platform(), Platform::Inke);
        assert_eq!(Platform::Inke.key(), "inke");
        assert_eq!(Platform::Inke.display_name(), "映客直播");
    }

    #[test]
    fn rejects_links_without_an_anchor_and_other_sites() {
        let adapter = InkeAdapter::new();
        for input in [
            "https://www.inke.cn/",
            "https://www.inke.cn/liveroom/index.html?id=1720860391070904",
            "https://www.inke.cn/liveroom/index.html?uid=abc&id=1",
            "https://www.inke.cn/liveroom/index.html?uid=0",
            "https://www.inke.cn.evil.example/liveroom/index.html?uid=1",
            "https://notinke.cn/liveroom/index.html?uid=1",
            "ftp://www.inke.cn/liveroom/index.html?uid=1",
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
        // A bad live id is dropped; the anchor is enough.
        assert_eq!(
            parse_link("https://www.inke.cn/liveroom/index.html?uid=5&id=x").unwrap(),
            link("5", None)
        );
    }

    #[test]
    fn request_urls_carry_the_anchor_and_live_id() {
        assert_eq!(
            now_publish_url("22954469"),
            "https://service.inke.cn/api/live/now_publish?id=22954469"
        );
        assert_eq!(
            share_url("22954469", Some("1720860391070904"), 1700000000),
            "https://webapi.busi.inke.cn/web/live_share_pc?uid=22954469&id=1720860391070904&_t=1700000000"
        );
        assert_eq!(
            share_url("1", None, 2),
            "https://webapi.busi.inke.cn/web/live_share_pc?uid=1&_t=2"
        );
    }

    #[test]
    fn live_anchor_records_the_signed_flv_for_every_quality() {
        let live = parse_now_publish(NOW_LIVE).unwrap().unwrap();
        assert_eq!(live.live_id, "1795483117403604");
        assert_eq!(live.title.as_deref(), Some("测试🎤标题"));
        let nick = parse_share(SHARE_LIVE, "100001").unwrap();
        assert_eq!(nick.as_deref(), Some("测试主播🧚"));
        // The link's old live id is replaced by the current broadcast.
        let old = link("100001", Some("1720860391070904"));
        for quality in Quality::ALL {
            let info = build_live_info(&old, Some(live.clone()), nick.clone(), quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Inke);
            assert_eq!(info.room_id, "100001");
            assert_eq!(info.anchor_id.as_deref(), Some("100001"));
            assert_eq!(info.anchor_name, "测试主播🧚");
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://www.inke.cn/liveroom/index.html?uid=100001&id=1795483117403604")
            );
            assert_eq!(info.variants.len(), 1);
            assert_eq!(info.variants[0].name, "原画");
            assert_eq!(info.variants[0].quality, Some(Quality::Od));
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert_eq!(
                selected.record_url,
                "https://pull.example.inke.cn/live/1795483117403604_t.flv?stream_id=1795483117403604_t&wsSecret=0123&wsABStime=6b051325"
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
    fn offline_anchor_keeps_the_nick_of_the_linked_broadcast() {
        assert_eq!(parse_now_publish(NOW_OFFLINE).unwrap(), None);
        let nick = parse_share(SHARE_ENDED, "22954469").unwrap();
        let sample = link("22954469", Some("1720860391070904"));
        let info = build_live_info(&sample, None, nick, Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        assert_eq!(
            info.anchor_name,
            "万万啊\u{1f9da}\u{1f3fb}\u{200d}\u{2640}\u{fe0f}"
        );
        assert_eq!(info.title, None);
        assert_eq!(
            info.live_url.as_deref(),
            Some("https://www.inke.cn/liveroom/index.html?uid=22954469&id=1720860391070904")
        );

        // Without a live id Inke has no nick for an offline anchor.
        assert_eq!(parse_share(SHARE_NO_LIVE_BODY, "22954469").unwrap(), None);
        let info = build_live_info(&link("22954469", None), None, None, Quality::Od);
        assert_eq!(info.anchor_name, "映客 22954469");
        assert_eq!(
            info.live_url.as_deref(),
            Some("https://www.inke.cn/liveroom/index.html?uid=22954469")
        );
    }

    #[test]
    fn a_placeholder_or_other_user_means_the_link_is_wrong() {
        let missing = parse_share(SHARE_MISSING, "999999999").unwrap_err();
        assert_eq!(missing.kind(), "invalid_url", "{missing}");
        assert!(missing.message().contains("没有找到"), "{missing}");
        // The live id belongs to someone else.
        assert_eq!(
            parse_share(SHARE_ENDED, "774870442").unwrap_err().kind(),
            "invalid_url"
        );
        let bad_uid = parse_now_publish(NOW_BAD_UID).unwrap_err();
        assert_eq!(bad_uid.kind(), "invalid_url", "{bad_uid}");
    }

    #[test]
    fn broken_answers_map_to_error_kinds() {
        for (body, kind) in [
            ("", "rate_limited"),
            ("<html>", "rate_limited"),
            ("{\"dm_error\":", "parse"),
            ("[]", "parse"),
            (r#"{"dm_error":500,"error_msg":"x"}"#, "parse"),
            (
                r#"{"dm_error":0,"live":{"status":1,"id":"1","stream_addr":""}}"#,
                "parse",
            ),
            (
                r#"{"dm_error":0,"live":{"status":1,"stream_addr":"https://a/b.flv"}}"#,
                "parse",
            ),
        ] {
            assert_eq!(parse_now_publish(body).unwrap_err().kind(), kind, "{body}");
        }
        // A broadcast that is not publishing counts as offline.
        assert_eq!(
            parse_now_publish(r#"{"dm_error":0,"live":{"status":0,"id":"1"}}"#).unwrap(),
            None
        );
        for (body, kind) in [
            ("<html>", "rate_limited"),
            (
                r#"{"error_code":9000000,"message":"illegal request"}"#,
                "parse",
            ),
            (r#"{"error_code":0,"data":null}"#, "parse"),
        ] {
            assert_eq!(parse_share(body, "1").unwrap_err().kind(), kind, "{body}");
        }
    }

    #[test]
    fn page_headers_carry_the_user_cookie_only_when_set() {
        assert!(pc_headers(None).unwrap().get(COOKIE).is_none());
        assert_eq!(
            pc_headers(Some("uid=1")).unwrap().get(COOKIE).unwrap(),
            "uid=1"
        );
        assert_eq!(pc_headers(Some("a=1\nb=2")).unwrap_err().kind(), "config");
    }
}
