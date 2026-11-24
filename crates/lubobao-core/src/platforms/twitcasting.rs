//! TwitCasting (ツイキャス) adapter, following StreamCap's
//! `TwitcastingHandler` and StreamGet v4.0.10 `TwitCastingLiveStream`.
//!
//! Ported from StreamGet (https://github.com/ihmily/streamget, v4.0.10),
//! `streamget/platforms/twitcasting/live_stream.py`, MIT License, Copyright
//! (c) 2025 Hmily. The mobile request headers and default cookie, reading the
//! live flag (`data-is-onlive`), the title and the name from the user page,
//! the `streamserver.php` request and ordering the `tc-hls` streams
//! `high`, `medium`, `low` follow that file; see `THIRD_PARTY_NOTICES.md`.
//!
//! Flow:
//!
//! * `twitcasting.tv/{user}` is the page. A user that does not exist is
//!   redirected to the home page, which has no `data-is-onlive`.
//! * `streamserver.php?target={user}&mode=client&player=pc_web` returns the
//!   streams of the running broadcast. `tc-hls.streams` holds one media
//!   playlist per tier (`high`, `medium`, `low`; a broadcast may offer fewer).
//!
//! StreamGet logs in with a username and password when the page lacks the
//! data. LuBoBao only supports the user's cookie; password-protected or
//! members-only broadcasts without usable HLS streams are `auth_required`.

use std::collections::BTreeMap;

use reqwest::Url;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, COOKIE, HeaderMap, HeaderValue, REFERER, USER_AGENT,
};
use serde_json::Value;

use super::{
    BoxFuture, ClientCache, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError,
    Quality, StreamVariant, classify_http_status, cookie_header, select_variant_index,
    selected_stream, user_cookie,
};

/// `TwitCastingLiveStream._get_mobile_headers` User-Agent.
pub const USER_AGENT_VALUE: &str = "ios/7.830 (ios 17.0; ; iPhone 15 (A2846/A3089/A3090/A3092))";
/// Cookie StreamGet sends when the user has none.
const DEFAULT_COOKIE: &str = "hl=zh; did=377eda93b5320f104357ab1bc98dfe4d; _ga=GA1.1.869052351.1747879503; keep=1; chid=relay_trade_jp;";
const REFERER_VALUE: &str =
    "https://twitcasting.tv/indexcaslogin.php?redir=%2Findexloginwindow.php%3Fnext%3D%252F&keep=1";
const PAGE: &str = "https://twitcasting.tv";
/// Quality names of `tc-hls.streams`, best first.
const TIERS: [&str; 3] = ["high", "medium", "low"];

/// First path segments of twitcasting.tv pages that are not users.
const RESERVED_PATHS: [&str; 24] = [
    "search",
    "ranking",
    "rankingindex",
    "login",
    "signup",
    "about",
    "privacy",
    "terms",
    "rss",
    "tag",
    "hashtag",
    "category",
    "explore",
    "settings",
    "support",
    "help",
    "live",
    "store",
    "gift",
    "maintenance",
    "api",
    "ads",
    "company",
    "show",
];

/// User ids: letters, digits, `_`, and the `c:`/`f:`/`g:`/`tw:` prefix of
/// accounts created through another service.
fn is_user_id(text: &str) -> bool {
    (2..=40).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':')
        && !text.starts_with(':')
}

/// A recognized TwitCasting user; the id is lowercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwitcastingLink {
    pub user_id: String,
}

impl TwitcastingLink {
    /// Canonical URL used to detect duplicate tasks.
    pub fn normalized_url(&self) -> String {
        user_url(&self.user_id)
    }
}

/// User page.
pub fn user_url(user_id: &str) -> String {
    format!("{PAGE}/{user_id}")
}

/// Recognize a TwitCasting link without any network access.
///
/// StreamCap registers `twitcasting.tv`; StreamGet takes the first path
/// segment as the user (`url.split('/')[3]`). `/{user}/movie/{id}` and other
/// sub pages name the same user.
pub fn parse_link(input: &str) -> Result<TwitcastingLink, PlatformError> {
    let trimmed = input.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let invalid =
        || PlatformError::InvalidUrl(format!("不是可识别的 TwitCasting 直播链接: {trimmed}"));
    let url = Url::parse(&with_scheme).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != "twitcasting.tv" && !host.ends_with(".twitcasting.tv") {
        return Err(invalid());
    }
    let user_id = url
        .path_segments()
        .and_then(|mut segments| segments.find(|segment| !segment.is_empty()))
        .map(|segment| segment.replace("%3A", ":").replace("%3a", ":"))
        .map(|segment| segment.to_ascii_lowercase())
        .filter(|id| is_user_id(id) && !RESERVED_PATHS.contains(&id.as_str()));
    match user_id {
        Some(user_id) => Ok(TwitcastingLink { user_id }),
        None => Err(PlatformError::InvalidUrl(format!(
            "TwitCasting 链接里没有用户名，请复制形如 twitcasting.tv/用户名 的链接: {trimmed}"
        ))),
    }
}

/// What the user page says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub user_id: String,
    pub name: String,
    pub title: Option<String>,
    pub is_live: bool,
}

fn attribute_value(html: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=\"");
    let start = html.find(&marker)? + marker.len();
    let end = html[start..].find('"')?;
    Some(html[start..start + end].to_string())
}

fn unescape(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
}

/// Parse the user page. `missing` means the request was redirected to the
/// home page (the user does not exist).
pub fn parse_page(html: &str, link: &TwitcastingLink) -> Result<Page, PlatformError> {
    let onlive = attribute_value(html, "data-is-onlive");
    let title_tag = html.find("<title>").and_then(|start| {
        let rest = &html[start + "<title>".len()..];
        rest.find("</title>").map(|end| rest[..end].to_string())
    });
    let (Some(onlive), Some(title_tag)) = (onlive, title_tag) else {
        return Err(PlatformError::InvalidUrl(format!(
            "TwitCasting 没有找到用户「{}」，请检查链接",
            link.user_id
        )));
    };
    // `NAME (@id)  的直播 - Twitcast` — the suffix depends on the language.
    let (name, screen_id) = match title_tag.rfind(" (@") {
        Some(position) => {
            let name = title_tag[..position].trim().to_string();
            let rest = &title_tag[position + 3..];
            let screen_id = rest.split(')').next().unwrap_or_default().to_string();
            (name, screen_id)
        }
        None => (title_tag.trim().to_string(), link.user_id.clone()),
    };
    let title = attribute_value(html, "name=\"twitter:title\" content")
        .map(|title| unescape(&title))
        .filter(|title| !title.trim().is_empty());
    Ok(Page {
        user_id: if screen_id.is_empty() {
            link.user_id.clone()
        } else {
            screen_id.to_ascii_lowercase()
        },
        name: unescape(&name),
        title,
        is_live: onlive == "true",
    })
}

/// The HLS media playlists of a running broadcast, best first (`high`,
/// `medium`, `low`).
pub fn parse_streams(body: &str) -> Result<Vec<(String, String)>, PlatformError> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        if body.trim().is_empty() || body.trim_start().starts_with('<') {
            PlatformError::RateLimited(
                "TwitCasting 接口返回的不是 JSON，可能触发了风控".to_string(),
            )
        } else {
            PlatformError::Parse("TwitCasting 接口返回的数据无法解析".to_string())
        }
    })?;
    let streams = json.pointer("/tc-hls/streams").and_then(Value::as_object);
    let list: Vec<(String, String)> = streams
        .map(|streams| {
            TIERS
                .iter()
                .filter_map(|tier| {
                    let url = streams.get(*tier)?.as_str()?.trim();
                    (!url.is_empty()).then(|| (tier.to_string(), url.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    if list.is_empty() {
        return Err(PlatformError::AuthRequired(
            "TwitCasting 没有给出播放地址，这个直播可能设了密码或只限成员，请在设置里填写登录后的 Cookie"
                .to_string(),
        ));
    }
    Ok(list)
}

/// Turn the page and its streams into `LiveInfo`.
pub fn build_live_info(page: &Page, streams: &[(String, String)], quality: Quality) -> LiveInfo {
    let variants: Vec<StreamVariant> = streams
        .iter()
        .enumerate()
        .map(|(index, (name, url))| StreamVariant {
            name: name.clone(),
            quality: Quality::from_index(index),
            flv_url: None,
            hls_url: Some(url.clone()),
        })
        .collect();
    let selected = select_variant_index(variants.len(), quality)
        .and_then(|index| variants.get(index))
        .filter(|_| page.is_live)
        .and_then(|variant| selected_stream(quality, variant));
    LiveInfo {
        platform: Platform::Twitcasting,
        room_id: page.user_id.clone(),
        anchor_id: Some(page.user_id.clone()),
        live_url: Some(user_url(&page.user_id)),
        anchor_name: page.name.clone(),
        title: page.title.clone(),
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

/// Headers for pulling TwitCasting streams: the User-Agent only.
pub fn stream_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("User-Agent".to_string(), USER_AGENT_VALUE.to_string())])
}

/// `_get_mobile_headers`: the user cookie, or StreamGet's default cookie.
pub fn mobile_headers(cookie: Option<&str>) -> Result<HeaderMap, PlatformError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
        ),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6"),
    );
    headers.insert(REFERER, HeaderValue::from_static(REFERER_VALUE));
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    headers.insert(COOKIE, cookie_header(cookie.unwrap_or(DEFAULT_COOKIE))?);
    Ok(headers)
}

/// `streamserver.php` URL for a user.
pub fn streamserver_url(user_id: &str) -> String {
    format!("{PAGE}/streamserver.php?target={user_id}&mode=client&player=pc_web")
}

/// TwitCasting adapter. Holds one HTTP client per proxy setting.
#[derive(Debug, Default)]
pub struct TwitcastingAdapter {
    clients: ClientCache,
}

impl TwitcastingAdapter {
    pub fn new() -> Self {
        TwitcastingAdapter::default()
    }

    async fn fetch_inner(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<LiveInfo, PlatformError> {
        let link = parse_link(url)?;
        let client = self.clients.get(options.proxy.as_deref())?;
        let cookie = user_cookie(options);
        let page_url = user_url(&link.user_id);
        let response = client
            .get(&page_url)
            .headers(mobile_headers(cookie)?)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), &page_url));
        }
        let redirected_home = response.url().path() == "/";
        let html = response.text().await?;
        if redirected_home {
            return Err(PlatformError::InvalidUrl(format!(
                "TwitCasting 没有找到用户「{}」，请检查链接",
                link.user_id
            )));
        }
        let page = parse_page(&html, &link)?;
        if !page.is_live {
            return Ok(build_live_info(&page, &[], options.quality));
        }
        let api = streamserver_url(&link.user_id);
        let response = client
            .get(&api)
            .headers(mobile_headers(cookie)?)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_http_status(status.as_u16(), &api));
        }
        let streams = parse_streams(&response.text().await?)?;
        Ok(build_live_info(&page, &streams, options.quality))
    }
}

impl PlatformAdapter for TwitcastingAdapter {
    fn platform(&self) -> Platform {
        Platform::Twitcasting
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

    const PAGE_LIVE: &str = include_str!("../../tests/fixtures/twitcasting/page_live.html");
    const PAGE_OFFLINE: &str = include_str!("../../tests/fixtures/twitcasting/page_offline.html");
    const PAGE_HOME: &str = include_str!("../../tests/fixtures/twitcasting/page_home.html");
    const STREAMS: &str = include_str!("../../tests/fixtures/twitcasting/streamserver_live.json");
    const NO_HLS: &str = include_str!("../../tests/fixtures/twitcasting/streamserver_no_hls.json");

    fn link(user: &str) -> TwitcastingLink {
        parse_link(&format!("https://twitcasting.tv/{user}")).unwrap()
    }

    #[test]
    fn recognizes_and_normalizes_links() {
        let adapter = TwitcastingAdapter::new();
        for (input, user) in [
            ("https://twitcasting.tv/25popo25", "25popo25"),
            ("https://twitcasting.tv/Airi_Traveling/", "airi_traveling"),
            (
                "https://twitcasting.tv/25popo25/movie/843339766",
                "25popo25",
            ),
            ("twitcasting.tv/c:07120712?login=true", "c:07120712"),
            ("https://twitcasting.tv/c%3A07120712", "c:07120712"),
            ("https://ja.twitcasting.tv/25popo25", "25popo25"),
        ] {
            assert_eq!(parse_link(input).unwrap().user_id, user, "{input}");
            assert!(adapter.matches(input), "{input}");
            assert_eq!(
                adapter.normalize_url(input),
                Some(format!("https://twitcasting.tv/{user}")),
                "{input}"
            );
        }
        let twitcasting = super::super::adapter_for("https://twitcasting.tv/25popo25").unwrap();
        assert_eq!(twitcasting.platform(), Platform::Twitcasting);
        assert_eq!(Platform::Twitcasting.key(), "twitcasting");
        assert_eq!(Platform::Twitcasting.display_name(), "TwitCasting");
    }

    #[test]
    fn rejects_other_pages_and_sites() {
        for input in [
            "https://twitcasting.tv/",
            "https://twitcasting.tv/rankingindex.php",
            "https://twitcasting.tv/search",
            "https://twitcasting.tv.evil.example/25popo25",
            "https://nottwitcasting.tv/25popo25",
            "ftp://twitcasting.tv/25popo25",
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
    fn live_user_maps_streams_to_qualities() {
        let page = parse_page(PAGE_LIVE, &link("25popo25")).unwrap();
        assert!(page.is_live);
        assert_eq!(page.name, "じょじ");
        assert_eq!(page.user_id, "25popo25");
        assert_eq!(page.title.as_deref(), Some("一軒家でカブトムシと同棲中。"));
        let streams = parse_streams(STREAMS).unwrap();
        let names: Vec<&str> = streams.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["high", "medium", "low"]);
        for (quality, name, id) in [
            (Quality::Od, "high", "672.96"),
            (Quality::Uhd, "medium", "671.96"),
            (Quality::Hd, "low", "480.64"),
            (Quality::Sd, "low", "480.64"),
            (Quality::Ld, "low", "480.64"),
        ] {
            let info = build_live_info(&page, &streams, quality);
            assert!(info.is_live);
            assert_eq!(info.platform, Platform::Twitcasting);
            assert_eq!(info.room_id, "25popo25");
            assert_eq!(info.anchor_name, "じょじ");
            assert_eq!(
                info.live_url.as_deref(),
                Some("https://twitcasting.tv/25popo25")
            );
            let selected = info.selected.unwrap();
            assert_eq!(selected.variant.name, name);
            assert_eq!(selected.requested, quality);
            assert_eq!(
                selected.record_url,
                format!(
                    "https://202-218-175-163.twitcasting.tv/tc.livehls/v1/streams/843339766/hls/{id}/media.m3u8"
                )
            );
            assert_eq!(
                selected.source_url(false, false).0,
                super::super::SourceFormat::Hls
            );
        }
    }

    #[test]
    fn offline_missing_and_unplayable_broadcasts() {
        let page = parse_page(PAGE_OFFLINE, &link("twitcasting_jp")).unwrap();
        assert!(!page.is_live);
        assert_eq!(page.name, "ツイキャス公式");
        let info = build_live_info(&page, &[], Quality::Od);
        assert!(!info.is_live);
        assert!(info.selected.is_none());
        assert!(info.variants.is_empty());
        // Redirected to the home page: no `data-is-onlive`.
        assert_eq!(
            parse_page(PAGE_HOME, &link("zzzznotexist"))
                .unwrap_err()
                .kind(),
            "invalid_url"
        );
        // Live, but the stream server gives no HLS stream.
        assert_eq!(parse_streams(NO_HLS).unwrap_err().kind(), "auth_required");
        for (body, kind) in [("<html>", "rate_limited"), ("{", "parse")] {
            assert_eq!(parse_streams(body).unwrap_err().kind(), kind, "{body}");
        }
        // Live without streams is not reported as live.
        let live = parse_page(PAGE_LIVE, &link("25popo25")).unwrap();
        assert!(!build_live_info(&live, &[], Quality::Od).is_live);
        // A page in another language still names the user.
        let english = PAGE_LIVE.replace("的直播 - Twitcast", "'s live - Twitcast");
        assert_eq!(
            parse_page(&english, &link("25popo25")).unwrap().name,
            "じょじ"
        );
    }

    #[test]
    fn requests_carry_streamget_headers() {
        let headers = mobile_headers(None).unwrap();
        assert_eq!(headers.get(USER_AGENT).unwrap(), USER_AGENT_VALUE);
        assert_eq!(headers.get(COOKIE).unwrap(), DEFAULT_COOKIE);
        assert_eq!(
            mobile_headers(Some("tc_ss=1"))
                .unwrap()
                .get(COOKIE)
                .unwrap(),
            "tc_ss=1"
        );
        assert_eq!(mobile_headers(Some("a\nb")).unwrap_err().kind(), "config");
        assert_eq!(
            streamserver_url("25popo25"),
            "https://twitcasting.tv/streamserver.php?target=25popo25&mode=client&player=pc_web"
        );
    }
}
