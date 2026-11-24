//! YY adapter, following StreamCap's `YYHandler` and StreamGet v4.0.10
//! `YYLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/yy/live_stream.py`, MIT License, Copyright (c) 2025
//! Hmily. The request flow, headers and the `stream-manager` request body
//! follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * The room page `www.yy.com/{sid}/{ssid}` carries `pageInfo` with the
//!   anchor's nick and uid and the channel id (`sid`). Unknown channels get
//!   YY's 404 page (served with status 200).
//! * `stream-manager.yy.com/v3/channel/streams` answers the stream request
//!   (StreamGet asks for `gear: 4`). A live room has `avp_info_res`, whose
//!   first `stream_line_addr` entry holds one FLV URL; an offline room has
//!   no `avp_info_res`.
//! * `www.yy.com/live/detail` gives the room title (`roomName`).
//!
//! YY answers with a single stream whatever the requested quality, so every
//! tier records the same FLV, as StreamGet does.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::header::{ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, SelectedStream, StreamVariant, classify_http_status, cookie_header, user_cookie,
};

/// `YYLiveStream.pc_headers` User-Agent, also used to pull streams.
pub const USER_AGENT_VALUE: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:109.0) Gecko/20100101 Firefox/115.0";
const ACCEPT_LANGUAGE_VALUE: &str = "zh-CN,zh;q=0.8,zh-TW;q=0.7,zh-HK;q=0.5,en-US;q=0.3,en;q=0.2";
const ROOM_PAGE: &str = "https://www.yy.com/";
const WEB_REFERER: &str = "https://www.yy.com/";
const STREAMS_API: &str = "https://stream-manager.yy.com/v3/channel/streams";
const DETAIL_API: &str = "https://www.yy.com/live/detail";
/// Fixed sequence StreamGet sends with the stream request.
const SEQUENCE: &str = "1701869217590";
/// Name of the only variant when the stream carries no gear name.
const DEFAULT_VARIANT_NAME: &str = "原画";

/// A recognized YY room link: the top channel (`sid`) and, when given, the
/// sub-channel (`ssid`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YyLink {
    pub sid: String,
    pub ssid: Option<String>,
}

impl YyLink {
    /// Room page as given, keeping the sub-channel.
    pub fn page_url(&self) -> String {
        match &self.ssid {
            Some(ssid) => format!("{ROOM_PAGE}{}/{ssid}", self.sid),
            None => format!("{ROOM_PAGE}{}", self.sid),
        }
    }

    /// Canonical URL for duplicate detection. A sub-channel equal to the top
    /// channel is the channel's own room, so `/{sid}/{sid}` and `/{sid}` match.
    pub fn normalized_url(&self) -> String {
        match &self.ssid {
            Some(ssid) if *ssid != self.sid => format!("{ROOM_PAGE}{}/{ssid}", self.sid),
            _ => format!("{ROOM_PAGE}{}", self.sid),
        }
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Recognize and normalize a YY link without any network access.
///
/// StreamCap registers `https://.*\.yy\.com/` and StreamGet requests the
/// page as given; its README example is `www.yy.com/22490906/22490906`.
/// Only room pages are accepted: `www.yy.com/{sid}` or
/// `www.yy.com/{sid}/{ssid}` with numeric channel ids.
pub fn parse_link(input: &str) -> Result<YyLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid = || PlatformError::InvalidUrl(format!("不是可识别的 YY 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if !matches!(host.as_str(), "www.yy.com" | "yy.com") {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        [sid] if is_digits(sid) => Ok(YyLink {
            sid: sid.to_string(),
            ssid: None,
        }),
        [sid, ssid] if is_digits(sid) && is_digits(ssid) => Ok(YyLink {
            sid: sid.to_string(),
            ssid: Some(ssid.to_string()),
        }),
        _ => Err(invalid()),
    }
}

/// Room facts from the page's `pageInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInfo {
    /// Top channel id; StreamGet uses it as both `cid` and `sid`.
    pub sid: String,
    pub ssid: Option<String>,
    /// The anchor on the mic (`uid`).
    pub anchor_id: Option<String>,
    pub anchor_name: String,
}

/// Value of a `key : "value"` line inside `block`, with JavaScript string
/// escapes undone. Only the first match counts, like StreamGet's regexes.
fn js_string(block: &str, key: &str) -> Option<String> {
    let mut rest = block;
    while let Some(position) = rest.find(key) {
        let before_ok = rest[..position]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == '{' || c == ',');
        let after = &rest[position + key.len()..];
        let value = after.trim_start_matches([' ', '\t']);
        if before_ok
            && let Some(value) = value.strip_prefix(':')
            && let Some(value) = value.trim_start_matches([' ', '\t']).strip_prefix('"')
        {
            let mut out = String::new();
            let mut chars = value.chars();
            while let Some(c) = chars.next() {
                match c {
                    '"' => return Some(out),
                    '\\' => match chars.next()? {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        other => out.push(other),
                    },
                    '\n' => return None,
                    other => out.push(other),
                }
            }
            return None;
        }
        rest = after;
    }
    None
}

/// Parse a room page.
pub fn parse_page(html: &str) -> Result<PageInfo, PlatformError> {
    let Some(start) = html.find("var pageInfo") else {
        if html.contains("yycom_404") {
            return Err(PlatformError::InvalidUrl(
                "YY 没有找到这个直播间，请检查链接".to_string(),
            ));
        }
        if html.trim().is_empty() {
            return Err(PlatformError::RateLimited(
                "YY 返回空页面，可能触发风控".to_string(),
            ));
        }
        return Err(PlatformError::Parse(
            "YY 直播间页面缺少 pageInfo，页面结构可能已变化".to_string(),
        ));
    };
    // The room's own fields come before the channel owner's `owInfo`.
    let block = &html[start..];
    let block = &block[..block.find("owInfo").unwrap_or(block.len())];
    let sid = js_string(block, "sid")
        .filter(|sid| is_digits(sid))
        .ok_or_else(|| PlatformError::Parse("YY 直播间页面缺少频道号".to_string()))?;
    let ssid = js_string(block, "ssid").filter(|ssid| is_digits(ssid));
    let anchor_id = js_string(block, "uid").filter(|uid| is_digits(uid) && uid != "0");
    let anchor_name = js_string(block, "nick")
        .map(|nick| nick.trim().to_string())
        .filter(|nick| !nick.is_empty())
        // Channels with nobody on the mic have an empty nick.
        .unwrap_or_else(|| format!("YY {sid}"));
    Ok(PageInfo {
        sid,
        ssid,
        anchor_id,
        anchor_name,
    })
}

/// Body of the stream request, StreamGet's JSON with the channel id filled in.
pub fn streams_request_body(cid: &str) -> String {
    format!(
        concat!(
            r#"{{"head":{{"seq":1701869217590,"appidstr":"0","bidstr":"121","cidstr":"{cid}","sidstr":"{cid}","#,
            r#""uid64":0,"client_type":108,"client_ver":"5.17.0","stream_sys_ver":1,"app":"yylive_web","#,
            r#""playersdk_ver":"5.17.0","thundersdk_ver":"0","streamsdk_ver":"5.17.0"}},"#,
            r#""client_attribute":{{"client":"web","model":"web0","cpu":"","graphics_card":"","#,
            r#""os":"chrome","osversion":"0","vsdk_version":"","app_identify":"","app_version":"","#,
            r#""business":"","width":"1920","height":"1080","scale":"","client_type":8,"h265":0}},"#,
            r#""avp_parameter":{{"version":1,"client_type":8,"service_type":0,"imsi":0,"send_time":1701869217,"#,
            r#""line_seq":-1,"gear":4,"ssl":1,"stream_format":0}}}}"#
        ),
        cid = cid
    )
}

/// URL of the stream request.
pub fn streams_url(cid: &str) -> String {
    let mut url = Url::parse(STREAMS_API).expect("valid streams API URL");
    url.query_pairs_mut()
        .append_pair("uid", "0")
        .append_pair("cid", cid)
        .append_pair("sid", cid)
        .append_pair("appid", "0")
        .append_pair("sequence", SEQUENCE)
        .append_pair("encode", "json");
    url.to_string()
}

/// The stream YY offers, or `None` when the room is offline.
pub fn parse_streams(body: &str) -> Result<Option<StreamVariant>, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited("YY 取流接口返回的不是 JSON，可能触发风控".to_string())
        } else {
            PlatformError::Parse("YY 取流接口返回的数据无法解析".to_string())
        }
    })?;
    if !json.is_object() {
        return Err(PlatformError::Parse(
            "YY 取流接口返回的数据无法解析".to_string(),
        ));
    }
    // StreamGet: live when `avp_info_res` is present, recording the URL of
    // the first `stream_line_addr` entry.
    let Some(avp) = json.get("avp_info_res") else {
        return Ok(None);
    };
    let Some((key, line)) = avp
        .get("stream_line_addr")
        .and_then(Value::as_object)
        .and_then(|lines| lines.iter().next())
    else {
        return Ok(None);
    };
    let url = line
        .get("cdn_info")
        .and_then(|cdn| cdn.get("url"))
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| PlatformError::Parse("YY 取流接口没有返回流地址".to_string()))?;
    Ok(Some(StreamVariant {
        name: gear_name(&json, key).unwrap_or_else(|| DEFAULT_VARIANT_NAME.to_string()),
        quality: Some(Quality::Od),
        flv_url: Some(url.to_string()),
        hls_url: None,
    }))
}

/// Gear name (e.g. 高清) of the channel stream a `stream_line_addr` key
/// (`{appid}_{stream_name}`) points at.
fn gear_name(json: &Value, line_key: &str) -> Option<String> {
    let streams = json
        .get("channel_stream_info")?
        .get("streams")?
        .as_array()?;
    streams.iter().find_map(|stream| {
        let name = stream.get("stream_name")?.as_str()?;
        let appid = match stream.get("appid")? {
            Value::Number(number) => number.to_string(),
            Value::String(text) => text.clone(),
            _ => return None,
        };
        if line_key != format!("{appid}_{name}") {
            return None;
        }
        let extra: Value = serde_json::from_str(stream.get("json")?.as_str()?).ok()?;
        extra
            .get("gear_info")?
            .get("name")?
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    })
}

/// Title from a `live/detail` response (`data.roomName`).
pub fn parse_detail(body: &str) -> Option<String> {
    let json: Value = serde_json::from_str(body).ok()?;
    json.get("data")?
        .get("roomName")?
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
}

/// Turn the page, stream answer and title into `LiveInfo`.
pub fn build_live_info(
    page: PageInfo,
    stream: Option<StreamVariant>,
    title: Option<String>,
    quality: Quality,
) -> LiveInfo {
    let selected = stream.as_ref().and_then(|variant| {
        // StreamGet: `record_url = flv_url`, whatever the requested quality.
        Some(SelectedStream {
            requested: quality,
            variant: variant.clone(),
            record_url: variant.flv_url.clone()?,
        })
    });
    let live_url = match &page.ssid {
        Some(ssid) => format!("{ROOM_PAGE}{}/{ssid}", page.sid),
        None => format!("{ROOM_PAGE}{}", page.sid),
    };
    LiveInfo {
        platform: Platform::Yy,
        room_id: page.sid,
        anchor_id: page.anchor_id,
        live_url: Some(live_url),
        anchor_name: page.anchor_name,
        title,
        is_live: selected.is_some(),
        variants: stream.into_iter().collect(),
        selected,
        stream_headers: stream_headers(),
    }
}

/// Headers for pulling YY streams: User-Agent and the web Referer. User
/// cookies are never forwarded to CDN hosts.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), USER_AGENT_VALUE.to_string()),
        ("Referer".to_string(), WEB_REFERER.to_string()),
    ])
}

/// `YYLiveStream.pc_headers`: User-Agent, Accept-Language and the user cookie.
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

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

/// YY adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct YyAdapter {
    clients: ClientCache,
}

impl YyAdapter {
    pub fn new() -> Self {
        YyAdapter::default()
    }

    async fn read(response: reqwest::Response, url: &str) -> Result<String, PlatformError> {
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), url));
        }
        Ok(response.text().await?)
    }

    async fn fetch_title(
        client: &reqwest::Client,
        page: &PageInfo,
        cookie: Option<&str>,
    ) -> Result<Option<String>, PlatformError> {
        let ssid = page.ssid.as_deref().unwrap_or(&page.sid);
        let mut url = Url::parse(DETAIL_API).expect("valid detail API URL");
        url.query_pairs_mut()
            .append_pair("uid", "")
            .append_pair("sid", &page.sid)
            .append_pair("ssid", ssid)
            .append_pair("_", &now_millis().to_string());
        let response = client
            .get(url.as_str())
            .headers(pc_headers(cookie)?)
            .send()
            .await?;
        Ok(parse_detail(&Self::read(response, url.as_str()).await?))
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);

        let page_url = link.page_url();
        let response = client
            .get(&page_url)
            .headers(pc_headers(cookie)?)
            .send()
            .await?;
        let page = parse_page(&Self::read(response, &page_url).await?)?;

        let api = streams_url(&page.sid);
        let response = client
            .post(&api)
            .headers(pc_headers(cookie)?)
            .body(streams_request_body(&page.sid))
            .send()
            .await?;
        let stream = parse_streams(&Self::read(response, &api).await?)?;

        // The title is only shown, so a failed detail request does not fail
        // the check (StreamGet would).
        let title = Self::fetch_title(&client, &page, cookie)
            .await
            .unwrap_or_default();
        Ok(build_live_info(page, stream, title, options.quality))
    }
}

impl PlatformAdapter for YyAdapter {
    fn platform(&self) -> Platform {
        Platform::Yy
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

    const PAGE_LIVE: &str = include_str!("../../tests/fixtures/yy/page_live.html");
    const PAGE_MISSING: &str = include_str!("../../tests/fixtures/yy/page_missing.html");
    const STREAMS_LIVE: &str = include_str!("../../tests/fixtures/yy/streams_live.json");
    const STREAMS_OFFLINE: &str = include_str!("../../tests/fixtures/yy/streams_offline.json");
    const DETAIL_LIVE: &str = include_str!("../../tests/fixtures/yy/detail_live.json");

    #[test]
    fn recognizes_and_normalizes_room_links() {
        let adapter = YyAdapter::new();
        let cases = [
            (
                "https://www.yy.com/22490906/22490906",
                "22490906",
                Some("22490906"),
            ),
            ("  www.yy.com/54880976 ", "54880976", None),
            ("http://yy.com/1/2/?from=share#x", "1", Some("2")),
            ("https://WWW.YY.COM/30000000", "30000000", None),
        ];
        for (input, sid, ssid) in cases {
            let link = parse_link(input).unwrap();
            assert_eq!(link.sid, sid, "{input}");
            assert_eq!(link.ssid.as_deref(), ssid, "{input}");
            assert!(adapter.matches(input), "{input}");
        }
        assert_eq!(
            adapter
                .normalize_url("https://www.yy.com/22490906/22490906?tempId=1")
                .as_deref(),
            Some("https://www.yy.com/22490906")
        );
        assert_eq!(
            adapter.normalize_url("https://www.yy.com/1/2").as_deref(),
            Some("https://www.yy.com/1/2")
        );
        assert_eq!(
            parse_link("https://www.yy.com/22490906/22490906")
                .unwrap()
                .page_url(),
            "https://www.yy.com/22490906/22490906"
        );
        assert_eq!(
            adapter.normalize_url("yy.com/54880976/").as_deref(),
            Some("https://www.yy.com/54880976")
        );
        let yy = super::super::adapter_for("https://www.yy.com/22490906/22490906").unwrap();
        assert_eq!(yy.platform(), Platform::Yy);
        assert_eq!(Platform::Yy.key(), "yy");
        assert_eq!(Platform::Yy.display_name(), "YY直播");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        let adapter = YyAdapter::new();
        for input in [
            "https://www.yy.com/",
            "https://www.yy.com/category/zonghe",
            "https://www.yy.com/22490906/abc",
            "https://www.yy.com/1/2/3",
            "https://m.yy.com/room/22490906",
            "https://www.yy.com.evil.example/22490906",
            "ftp://www.yy.com/22490906",
            "https://www.douyu.com/3637778",
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
    fn page_gives_channel_and_anchor() {
        assert_eq!(
            parse_page(PAGE_LIVE).unwrap(),
            PageInfo {
                sid: "100001".to_string(),
                ssid: Some("100001".to_string()),
                anchor_id: Some("200001".to_string()),
                anchor_name: "测试主播-Replay🥟".to_string(),
            }
        );
        // An empty nick (nobody on the mic) falls back to the channel id.
        let empty = PAGE_LIVE.replace("nick: \"测试主播-Replay🥟\"", "nick: \"\"");
        assert_eq!(parse_page(&empty).unwrap().anchor_name, "YY 100001");
        // Escaped quotes in the nick are undone.
        let quoted = PAGE_LIVE.replace("测试主播-Replay🥟", r#"a\"b"#);
        assert_eq!(parse_page(&quoted).unwrap().anchor_name, "a\"b");
    }

    #[test]
    fn page_errors_map_to_error_kinds() {
        let missing = parse_page(PAGE_MISSING).unwrap_err();
        assert_eq!(missing.kind(), "invalid_url", "{missing}");
        assert!(missing.message().contains("没有找到"), "{missing}");
        assert_eq!(parse_page("  ").unwrap_err().kind(), "rate_limited");
        assert_eq!(
            parse_page("<html><body>维护中</body></html>")
                .unwrap_err()
                .kind(),
            "parse"
        );
        let no_sid = PAGE_LIVE.replace("sid : \"100001\"", "sid : \"\"");
        assert_eq!(parse_page(&no_sid).unwrap_err().kind(), "parse");
    }

    #[test]
    fn stream_request_matches_streamget() {
        let body = streams_request_body("22490906");
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["head"]["cidstr"], "22490906");
        assert_eq!(json["head"]["sidstr"], "22490906");
        assert_eq!(json["head"]["app"], "yylive_web");
        assert_eq!(json["avp_parameter"]["gear"], 4);
        assert_eq!(json["client_attribute"]["h265"], 0);
        assert_eq!(
            streams_url("22490906"),
            "https://stream-manager.yy.com/v3/channel/streams?uid=0&cid=22490906&sid=22490906&appid=0&sequence=1701869217590&encode=json"
        );
    }

    #[test]
    fn live_room_records_the_first_stream_line() {
        let page = parse_page(PAGE_LIVE).unwrap();
        let stream = parse_streams(STREAMS_LIVE).unwrap();
        let title = parse_detail(DETAIL_LIVE);
        assert_eq!(title.as_deref(), Some("测试🎥直播间"));
        for quality in Quality::ALL {
            let info = build_live_info(page.clone(), stream.clone(), title.clone(), quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Yy);
            assert_eq!(info.room_id, "100001");
            assert_eq!(info.anchor_id.as_deref(), Some("200001"));
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://www.yy.com/100001/100001")
            );
            assert_eq!(info.variants.len(), 1);
            assert_eq!(info.variants[0].name, "高清");
            assert_eq!(info.variants[0].quality, Some(Quality::Od));
            let selected = info.selected.unwrap();
            assert_eq!(selected.requested, quality);
            assert!(selected.record_url.starts_with(
                "https://flv.example.yy.com/live/15013_xv_100001_100001_0_0_0-15013_xa_100001_100001_0_0_0"
            ));
            assert_eq!(
                selected.source_url(true, true),
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
    fn a_stream_without_gear_info_is_named_original() {
        let body = STREAMS_LIVE.replace("gear_info", "gear_x");
        let stream = parse_streams(&body).unwrap().unwrap();
        assert_eq!(stream.name, "原画");
    }

    #[test]
    fn offline_rooms_and_bad_stream_answers() {
        assert_eq!(parse_streams(STREAMS_OFFLINE).unwrap(), None);
        let info = build_live_info(parse_page(PAGE_LIVE).unwrap(), None, None, Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        assert_eq!(info.anchor_name, "测试主播-Replay🥟");

        assert_eq!(
            parse_streams(r#"{"avp_info_res":{"stream_line_addr":{}}}"#).unwrap(),
            None
        );
        for (body, kind) in [
            ("", "rate_limited"),
            ("<html>", "rate_limited"),
            ("{\"head\":", "parse"),
            ("[]", "parse"),
            (
                r#"{"avp_info_res":{"stream_line_addr":{"k":{"cdn_info":{"url":""}}}}}"#,
                "parse",
            ),
        ] {
            assert_eq!(parse_streams(body).unwrap_err().kind(), kind, "{body}");
        }
        assert_eq!(parse_detail("{\"resultCode\":1}"), None);
        assert_eq!(parse_detail("<html>"), None);
    }

    #[test]
    fn page_headers_carry_the_user_cookie_only_when_set() {
        assert!(pc_headers(None).unwrap().get(COOKIE).is_none());
        assert_eq!(
            pc_headers(Some("yyuid=1")).unwrap().get(COOKIE).unwrap(),
            "yyuid=1"
        );
        assert_eq!(pc_headers(Some("a=1\nb=2")).unwrap_err().kind(), "config");
    }
}
