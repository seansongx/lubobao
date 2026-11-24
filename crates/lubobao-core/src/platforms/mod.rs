//! Platform adapters: turn a live-room link into a normalized snapshot of the
//! room (anchor, title, live state, stream variants) and the stream to record.
//!
//! Behaviour follows StreamCap's platform handlers and the StreamGet library
//! they call (`streamget` v4.0.10). Each adapter is stateless apart from its
//! HTTP configuration, so the scheduler can call `fetch` concurrently.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub mod acfun;
pub mod baidu;
pub mod bilibili;
pub mod douyin;
pub mod douyu;
#[cfg(feature = "e2e")]
pub mod e2e;
pub mod huajiao;
pub mod huya;
pub mod inke;
pub mod kuaishou;
pub mod kugou;
pub mod maoer;
pub mod netease;
mod sign;
pub mod sixroom;
pub mod tiktok;
pub mod weibo;
pub mod xiaohongshu;
pub mod yy;
pub mod zhihu;

/// Boxed future returned by adapter methods so adapters can be used as trait objects.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 连接（DNS、TCP、TLS、代理握手）的时限。国内平台正常在 1 秒内连上，10 秒连不上
/// 基本就是网络或代理不通，早点失败好让调度器按网络错误重试。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 两次读之间最长停顿多久算失败（StreamGet 整请求 15–20 秒，这里改成按停顿计时）。
/// 慢网下大响应要持续下载很久，只要数据还在来就不应中断；20 秒一个字节都没有才算卡住。
pub const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// 单个请求从发出到读完的总上限，防止服务端一点点挤数据让请求永远不结束。
/// 60 秒够慢网下载几百 KB 的未压缩页面；调度器在请求结束后才排下一次检测，不会叠加。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Douyin,
    Kuaishou,
    Bilibili,
    Huya,
    Douyu,
    Xiaohongshu,
    Yy,
    Netease,
    Weibo,
    Acfun,
    Huajiao,
    Inke,
    Kugou,
    /// 六间房：平台键以数字开头，按 StreamCap 的 `6room` 序列化。
    #[serde(rename = "6room")]
    SixRoom,
    Baidu,
    Zhihu,
    Maoer,
    Tiktok,
    /// 端到端测试用的本地直播间（feature `e2e`），发布版没有。
    #[cfg(feature = "e2e")]
    E2e,
}

impl Platform {
    /// Stable key, matching StreamCap's `platform_key`.
    pub fn key(self) -> &'static str {
        match self {
            Platform::Douyin => "douyin",
            Platform::Kuaishou => "kuaishou",
            Platform::Bilibili => "bilibili",
            Platform::Huya => "huya",
            Platform::Douyu => "douyu",
            Platform::Xiaohongshu => "xiaohongshu",
            Platform::Yy => "yy",
            Platform::Netease => "netease",
            Platform::Weibo => "weibo",
            Platform::Acfun => "acfun",
            Platform::Huajiao => "huajiao",
            Platform::Inke => "inke",
            Platform::Kugou => "kugou",
            Platform::SixRoom => "6room",
            Platform::Baidu => "baidu",
            Platform::Zhihu => "zhihu",
            Platform::Maoer => "maoerfm",
            Platform::Tiktok => "tiktok",
            #[cfg(feature = "e2e")]
            Platform::E2e => "e2e",
        }
    }

    /// Display name, matching the `platform` field StreamGet reports.
    pub fn display_name(self) -> &'static str {
        match self {
            Platform::Douyin => "抖音",
            Platform::Kuaishou => "快手直播",
            Platform::Bilibili => "哔哩哔哩",
            Platform::Huya => "虎牙直播",
            Platform::Douyu => "斗鱼直播",
            Platform::Xiaohongshu => "小红书",
            Platform::Yy => "YY直播",
            Platform::Netease => "网易CC直播",
            Platform::Weibo => "微博直播",
            Platform::Acfun => "Acfun",
            Platform::Huajiao => "花椒直播",
            Platform::Inke => "映客直播",
            Platform::Kugou => "酷狗直播",
            Platform::SixRoom => "六间房直播",
            Platform::Baidu => "百度直播",
            Platform::Zhihu => "知乎直播",
            Platform::Maoer => "猫耳FM直播",
            Platform::Tiktok => "TikTok",
            #[cfg(feature = "e2e")]
            Platform::E2e => "测试直播间",
        }
    }
}

/// Recording quality tiers, identical to StreamCap's `VideoQuality`
/// (`app/models/media/video_quality_model.py`) and StreamGet's
/// `get_quality_index` mapping `OD=0, UHD=1, HD=2, SD=3, LD=4`.
///
/// StreamCap has no separate 蓝光 tier; its quality help text documents
/// index 0 as "原画或者蓝光".
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum Quality {
    /// Default, as StreamCap `StreamManager.DEFAULT_QUALITY = VideoQuality.OD`.
    #[default]
    Od,
    Uhd,
    Hd,
    Sd,
    Ld,
}

impl Quality {
    pub const ALL: [Quality; 5] = [
        Quality::Od,
        Quality::Uhd,
        Quality::Hd,
        Quality::Sd,
        Quality::Ld,
    ];

    /// Code stored in StreamCap's config, e.g. `"OD"`.
    pub fn code(self) -> &'static str {
        match self {
            Quality::Od => "OD",
            Quality::Uhd => "UHD",
            Quality::Hd => "HD",
            Quality::Sd => "SD",
            Quality::Ld => "LD",
        }
    }

    /// Chinese label from StreamCap's `locales/zh_CN.json`.
    pub fn label(self) -> &'static str {
        match self {
            Quality::Od => "原画",
            Quality::Uhd => "超清",
            Quality::Hd => "高清",
            Quality::Sd => "标清",
            Quality::Ld => "流畅",
        }
    }

    /// Position in StreamGet's quality list (0 = best).
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(index: usize) -> Option<Quality> {
        Quality::ALL.get(index).copied()
    }

    /// Parse a quality the way StreamGet's `get_quality_index` does: a code
    /// (case-insensitive) or a digit index. Empty input means `OD`; an
    /// unknown code also falls back to `OD`.
    pub fn parse_lenient(value: &str) -> Quality {
        let value = value.trim();
        if value.is_empty() {
            return Quality::Od;
        }
        if let Ok(index) = value.parse::<usize>() {
            return Quality::from_index(index).unwrap_or(Quality::Od);
        }
        let upper = value.to_ascii_uppercase();
        Quality::ALL
            .into_iter()
            .find(|quality| quality.code() == upper)
            .unwrap_or(Quality::Od)
    }
}

impl fmt::Display for Quality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// One stream variant offered by the room, in the platform's best-first order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamVariant {
    /// Platform key of the variant, e.g. Douyin `ORIGIN`, `FULL_HD1`, `SD2`.
    pub name: String,
    /// StreamCap tier this position maps to; `None` past the fifth variant.
    pub quality: Option<Quality>,
    pub flv_url: Option<String>,
    pub hls_url: Option<String>,
}

/// Stream container to hand to the recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    Flv,
    Hls,
}

/// Stream chosen for the requested quality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedStream {
    /// Quality the user asked for (StreamGet reports this, not the variant's own tier).
    pub requested: Quality,
    /// Variant actually chosen after padding and fallback.
    pub variant: StreamVariant,
    /// StreamGet `record_url`: HLS when present, otherwise FLV.
    pub record_url: String,
}

impl SelectedStream {
    /// Video codec carried in the FLV URL's `codec` query parameter, if any.
    pub fn flv_codec(&self) -> Option<String> {
        let flv_url = self.variant.flv_url.as_deref()?;
        let url = reqwest::Url::parse(flv_url).ok()?;
        url.query_pairs()
            .find(|(key, _)| key == "codec")
            .map(|(_, value)| value.into_owned())
    }

    /// Source URL StreamCap's `StreamManager._select_source_url` would record:
    /// FLV-preferred platforms use FLV unless the user chose HLS or the FLV
    /// stream is H.265; otherwise `record_url`.
    pub fn source_url(
        &self,
        flv_preferred_platform: bool,
        prefer_hls: bool,
    ) -> (SourceFormat, &str) {
        if flv_preferred_platform
            && !prefer_hls
            && self.flv_codec().as_deref() != Some("h265")
            && let Some(flv_url) = self.variant.flv_url.as_deref()
        {
            return (SourceFormat::Flv, flv_url);
        }
        match self.variant.hls_url.as_deref() {
            Some(hls_url) if hls_url == self.record_url => (SourceFormat::Hls, hls_url),
            _ => (SourceFormat::Flv, self.record_url.as_str()),
        }
    }
}

/// Normalized live-room snapshot returned by every adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveInfo {
    pub platform: Platform,
    /// Room identifier as the platform addresses it. For Douyin this is the
    /// `web_rid` when known; for `live.douyin.com/{抖音号}` links and profile
    /// links it is the 抖音号 (the web API does not return the `web_rid`); for
    /// app rooms without a `web_rid` it falls back to the anchor's `sec_uid`.
    /// It never uses the per-broadcast room id, but the same anchor can appear
    /// under different values here — deduplicate with `anchor_id`.
    pub room_id: String,
    /// Stable anchor identifier shared by every link form (Douyin `sec_uid`),
    /// when the response includes it.
    pub anchor_id: Option<String>,
    /// Canonical room page, when the platform exposes one.
    pub live_url: Option<String>,
    pub anchor_name: String,
    pub title: Option<String>,
    pub is_live: bool,
    /// All variants, best first. Empty when offline.
    pub variants: Vec<StreamVariant>,
    /// Stream for the requested quality. `None` when offline.
    pub selected: Option<SelectedStream>,
    /// Headers to send when pulling the stream. Douyin needs only User-Agent
    /// and Referer; user cookies are never included.
    pub stream_headers: BTreeMap<String, String>,
}

/// Options for one `fetch` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchOptions {
    /// Raw `Cookie` header value supplied by the user.
    pub cookies: Option<String>,
    /// Proxy address, e.g. `127.0.0.1:7890`, `http://127.0.0.1:7890` or
    /// `socks5://127.0.0.1:7891`.
    pub proxy: Option<String>,
    pub quality: Quality,
}

/// Why a fetch failed, grouped so the UI can explain the cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum PlatformError {
    /// Connection, DNS, TLS, timeout or proxy failure.
    Network(String),
    /// Link not recognized, or the room/user does not exist.
    InvalidUrl(String),
    /// The platform requires cookies or a logged-in account.
    AuthRequired(String),
    /// Rate limited or blocked by risk control.
    RateLimited(String),
    /// Response shape changed and could not be parsed.
    Parse(String),
    /// User settings cannot be used, e.g. a cookie with line breaks or an
    /// unsupported proxy scheme.
    Config(String),
}

impl PlatformError {
    pub fn kind(&self) -> &'static str {
        match self {
            PlatformError::Network(_) => "network",
            PlatformError::InvalidUrl(_) => "invalid_url",
            PlatformError::AuthRequired(_) => "auth_required",
            PlatformError::RateLimited(_) => "rate_limited",
            PlatformError::Parse(_) => "parse",
            PlatformError::Config(_) => "config",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            PlatformError::Network(message)
            | PlatformError::InvalidUrl(message)
            | PlatformError::AuthRequired(message)
            | PlatformError::RateLimited(message)
            | PlatformError::Parse(message)
            | PlatformError::Config(message) => message,
        }
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind(), self.message())
    }
}

impl std::error::Error for PlatformError {}

impl From<reqwest::Error> for PlatformError {
    fn from(error: reqwest::Error) -> Self {
        if let Some(status) = error.status() {
            return classify_http_status(status.as_u16(), &error.to_string());
        }
        let host = error
            .url()
            .and_then(|url| url.host_str())
            .map(|host| format!("（{host}）"))
            .unwrap_or_default();
        // 超时在 reqwest 里可能报成「error decoding response body」（读 body 时到时），
        // 先按超时归类，给出看得懂的原因。
        if error.is_timeout() {
            return PlatformError::Network(format!(
                "请求超时{host}：网络较慢或平台没有响应，稍后会自动重试"
            ));
        }
        if error.is_connect() {
            return PlatformError::Network(format!(
                "无法连接平台{host}：请检查网络或代理设置（{}）",
                error_chain(&error)
            ));
        }
        if error.is_decode() {
            // `.json()` 解析失败才是平台返回的数据变了；其余是读 body 时连接中断。
            if has_source::<serde_json::Error>(&error) {
                return PlatformError::Parse(error.to_string());
            }
            return PlatformError::Network(format!(
                "读取平台响应时连接中断{host}：网络不稳定（{}）",
                error_chain(&error)
            ));
        }
        PlatformError::Network(error.to_string())
    }
}

/// Whether the error's source chain contains an error of type `T`.
fn has_source<T: std::error::Error + 'static>(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = error.source();
    while let Some(source) = current {
        if source.is::<T>() {
            return true;
        }
        current = source.source();
    }
    false
}

/// The error and its sources joined with「: 」, since reqwest's own message
/// (such as "error sending request") hides the actual cause.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut current = error.source();
    while let Some(source) = current {
        message.push_str(": ");
        message.push_str(&source.to_string());
        current = source.source();
    }
    message
}

/// Map an HTTP error status to an error kind.
pub fn classify_http_status(status: u16, context: &str) -> PlatformError {
    let message = format!("HTTP {status}: {context}");
    match status {
        401 => PlatformError::AuthRequired(message),
        403 | 429 => PlatformError::RateLimited(message),
        404 | 410 => PlatformError::InvalidUrl(message),
        _ => PlatformError::Network(message),
    }
}

/// Adapter for one platform.
pub trait PlatformAdapter: Send + Sync {
    fn platform(&self) -> Platform;

    /// Whether this adapter handles the link (offline check, no requests).
    fn matches(&self, url: &str) -> bool;

    /// Canonical form of a link this adapter matches, used to detect duplicate
    /// tasks (offline, no requests). `None` lets the caller fall back to a
    /// generic URL normalization.
    fn normalize_url(&self, _url: &str) -> Option<String> {
        None
    }

    /// Resolve the link and fetch the room's current state.
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        options: &'a FetchOptions,
    ) -> BoxFuture<'a, Result<LiveInfo, PlatformError>>;
}

/// All built-in adapters.
pub fn adapters() -> Vec<Box<dyn PlatformAdapter>> {
    #[allow(unused_mut)]
    let mut list: Vec<Box<dyn PlatformAdapter>> = vec![
        Box::new(douyin::DouyinAdapter::new()),
        Box::new(kuaishou::KuaishouAdapter::new()),
        Box::new(bilibili::BilibiliAdapter::new()),
        Box::new(huya::HuyaAdapter::new()),
        Box::new(douyu::DouyuAdapter::new()),
        Box::new(xiaohongshu::XiaohongshuAdapter::new()),
        Box::new(yy::YyAdapter::new()),
        Box::new(netease::NeteaseAdapter::new()),
        Box::new(weibo::WeiboAdapter::new()),
        Box::new(acfun::AcfunAdapter::new()),
        Box::new(huajiao::HuajiaoAdapter::new()),
        Box::new(inke::InkeAdapter::new()),
        Box::new(kugou::KugouAdapter::new()),
        Box::new(sixroom::SixRoomAdapter::new()),
        Box::new(baidu::BaiduAdapter::new()),
        Box::new(zhihu::ZhihuAdapter::new()),
        Box::new(maoer::MaoerAdapter::new()),
        Box::new(tiktok::TiktokAdapter::new()),
    ];
    #[cfg(feature = "e2e")]
    list.push(Box::new(e2e::E2eAdapter));
    list
}

/// Characters allowed in a URL (RFC 3986 unreserved, reserved and `%`).
fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// Drop punctuation that ends a sentence rather than the link. A closing
/// bracket stays when the link opened it.
fn trim_link_end(mut link: &str) -> &str {
    loop {
        let Some(last) = link.chars().last() else {
            return link;
        };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' => link.matches('(').count() < link.matches(')').count(),
            ']' => link.matches('[').count() < link.matches(']').count(),
            _ => false,
        };
        if !strip {
            return link;
        }
        link = &link[..link.len() - 1];
    }
}

/// First `http(s)://…` link in the text (also `e2e+http://` test links).
fn find_scheme_link(text: &str) -> Option<&str> {
    for (index, _) in text.match_indices("://") {
        let before = &text[..index];
        let scheme_start = before
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
            .last()
            .map_or(index, |(start, _)| start);
        let scheme = before[scheme_start..].to_ascii_lowercase();
        let start =
            if scheme.contains('+') && (scheme.ends_with("+http") || scheme.ends_with("+https")) {
                scheme_start
            } else if scheme.ends_with("https") {
                index - "https".len()
            } else if scheme.ends_with("http") {
                index - "http".len()
            } else {
                continue;
            };
        let rest = &text[index + 3..];
        let end = rest.find(|c: char| !is_url_char(c)).unwrap_or(rest.len());
        let link = trim_link_end(&text[start..index + 3 + end]);
        if link.len() > index + 3 - start {
            return Some(link);
        }
    }
    None
}

/// Whether a token without a scheme looks like `host.tld/…`.
fn looks_like_host_path(token: &str) -> bool {
    let host = token.split(['/', '?', '#']).next().unwrap_or_default();
    !token.is_empty()
        && token.chars().all(is_url_char)
        && host.contains('.')
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host.chars().any(|c| c.is_ascii_alphabetic())
}

/// The link in what the user pasted. App share texts wrap the link in other
/// words (`【xxx的直播】 https://v.douyin.com/abc/ 复制此链接…`), so take the
/// first `http(s)://` link, ending at whitespace, full-width punctuation or
/// any character a URL cannot hold, without trailing sentence punctuation.
/// Without a scheme, the first token that looks like `host.tld/path` (such as
/// `live.douyin.com/123`). `None` when the text holds no link at all.
///
/// StreamCap only strips the input; extracting the link is an intentional
/// extension.
pub fn extract_link(input: &str) -> Option<&str> {
    let text = input.trim();
    if let Some(link) = find_scheme_link(text) {
        return Some(link);
    }
    text.split(|c: char| c.is_whitespace() || !c.is_ascii())
        .map(trim_link_end)
        .find(|token| looks_like_host_path(token))
}

/// Follow a share short link hop by hop and return the first redirect target
/// `accept` takes, without requesting it. When the redirects end elsewhere,
/// returns the URL they ended at so the caller can report it.
pub(crate) async fn follow_short_link(
    proxy: Option<&str>,
    url: &str,
    headers: reqwest::header::HeaderMap,
    accept: impl Fn(&reqwest::Url) -> bool,
) -> Result<String, PlatformError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy) = normalize_proxy(proxy.unwrap_or_default())? {
        let proxy = reqwest::Proxy::all(&proxy)
            .map_err(|error| PlatformError::Config(format!("代理地址无效 {proxy}: {error}")))?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|error| PlatformError::Network(format!("failed to build HTTP client: {error}")))?;
    let mut current = reqwest::Url::parse(url)
        .map_err(|_| PlatformError::InvalidUrl(format!("分享链接无效: {url}")))?;
    for _ in 0..10 {
        let response = client
            .get(current.clone())
            .headers(headers.clone())
            .send()
            .await?;
        let status = response.status();
        if !status.is_redirection() {
            if status.is_success() {
                return Ok(current.to_string());
            }
            return Err(classify_http_status(status.as_u16(), current.as_str()));
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| PlatformError::Parse(format!("分享链接跳转缺少目标地址: {url}")))?;
        let next = current
            .join(location)
            .map_err(|_| PlatformError::Parse(format!("分享链接跳转地址无效: {location}")))?;
        if accept(&next) {
            return Ok(next.to_string());
        }
        current = next;
    }
    Err(PlatformError::Network(format!(
        "分享链接跳转次数过多: {url}"
    )))
}

/// First adapter that accepts the link.
pub fn adapter_for(url: &str) -> Option<Box<dyn PlatformAdapter>> {
    adapters().into_iter().find(|adapter| adapter.matches(url))
}

/// Pick the variant for `quality` the way StreamGet does for list-based
/// platforms: pad the best-first list to five entries by repeating the last
/// one, then take the tier's index. Returns `None` when the list is empty.
pub fn select_variant_index(variant_count: usize, quality: Quality) -> Option<usize> {
    if variant_count == 0 {
        return None;
    }
    Some(quality.index().min(variant_count - 1))
}

/// Index StreamGet's Douyin handler retries when the chosen stream cannot be
/// reached: the next lower tier, or the one above when already at `LD`.
/// Works on the padded five-entry list; the result is clamped like
/// `select_variant_index`.
pub fn fallback_variant_index(variant_count: usize, quality: Quality) -> Option<usize> {
    if variant_count == 0 {
        return None;
    }
    let padded_index = quality.index();
    let fallback = if padded_index < 4 {
        padded_index + 1
    } else {
        padded_index - 1
    };
    Some(fallback.min(variant_count - 1))
}

/// Neighbour of an already chosen variant, for platforms that do not pick by
/// padded position (Kuaishou picks by bitrate): the next lower variant, or
/// the one above when the chosen variant is already the lowest. Mirrors
/// `fallback_variant_index` on the real list.
pub fn neighbour_variant_index(variant_count: usize, index: usize) -> Option<usize> {
    if variant_count == 0 {
        return None;
    }
    let index = index.min(variant_count - 1);
    if index + 1 < variant_count {
        Some(index + 1)
    } else {
        Some(index.saturating_sub(1))
    }
}

/// Build a `SelectedStream` for a variant, using StreamGet's `record_url = m3u8 or flv`.
pub fn selected_stream(requested: Quality, variant: &StreamVariant) -> Option<SelectedStream> {
    let record_url = variant
        .hls_url
        .clone()
        .filter(|url| !url.is_empty())
        .or_else(|| variant.flv_url.clone().filter(|url| !url.is_empty()))?;
    Some(SelectedStream {
        requested,
        variant: variant.clone(),
        record_url,
    })
}

/// Proxy schemes reqwest can use with the `socks` feature enabled.
const PROXY_SCHEMES: [&str; 6] = ["http", "https", "socks4", "socks4a", "socks5", "socks5h"];

/// Normalize a proxy address the way StreamGet's `handle_proxy_addr` does
/// (add `http://` when no scheme is given) and reject schemes the HTTP
/// client cannot use.
pub fn normalize_proxy(proxy: &str) -> Result<Option<String>, PlatformError> {
    let proxy = proxy.trim();
    if proxy.is_empty() {
        return Ok(None);
    }
    let Some((scheme, _)) = proxy.split_once("://") else {
        return Ok(Some(format!("http://{proxy}")));
    };
    if PROXY_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str()) {
        Ok(Some(proxy.to_string()))
    } else {
        Err(PlatformError::Config(format!(
            "不支持的代理协议 {scheme}，请使用 http、https 或 socks5"
        )))
    }
}

/// Build an HTTP client that follows redirects and uses the given proxy.
pub(crate) fn build_client(proxy: Option<&str>) -> Result<reqwest::Client, PlatformError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(10));
    if let Some(proxy) = normalize_proxy(proxy.unwrap_or_default())? {
        let proxy = reqwest::Proxy::all(&proxy)
            .map_err(|error| PlatformError::Config(format!("代理地址无效 {proxy}: {error}")))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|error| PlatformError::Network(format!("failed to build HTTP client: {error}")))
}

/// One HTTP client per proxy setting, so repeated polls reuse connections.
/// The client is rebuilt when the proxy changes.
#[derive(Debug, Default)]
pub(crate) struct ClientCache(Mutex<Option<(Option<String>, reqwest::Client)>>);

impl ClientCache {
    pub(crate) fn get(&self, proxy: Option<&str>) -> Result<reqwest::Client, PlatformError> {
        let key = proxy
            .map(str::trim)
            .filter(|proxy| !proxy.is_empty())
            .map(str::to_string);
        let mut cached = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached_key, client)) = cached.as_ref()
            && *cached_key == key
        {
            return Ok(client.clone());
        }
        let client = build_client(key.as_deref())?;
        *cached = Some((key, client.clone()));
        Ok(client)
    }
}

/// The user's cookie for this platform, when set.
pub(crate) fn user_cookie(options: &FetchOptions) -> Option<&str> {
    options
        .cookies
        .as_deref()
        .map(str::trim)
        .filter(|cookie| !cookie.is_empty())
}

/// A `Cookie` header value, rejecting values that cannot be sent.
pub(crate) fn cookie_header(value: &str) -> Result<reqwest::header::HeaderValue, PlatformError> {
    reqwest::header::HeaderValue::from_str(value).map_err(|_| {
        PlatformError::Config("Cookie 含有换行等无法发送的字符，请检查平台 Cookie 设置".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(name: &str, hls: Option<&str>, flv: Option<&str>) -> StreamVariant {
        StreamVariant {
            name: name.to_string(),
            quality: None,
            flv_url: flv.map(str::to_string),
            hls_url: hls.map(str::to_string),
        }
    }

    #[test]
    fn quality_parses_codes_digits_and_defaults_like_streamget() {
        assert_eq!(Quality::parse_lenient("od"), Quality::Od);
        assert_eq!(Quality::parse_lenient("UHD"), Quality::Uhd);
        assert_eq!(Quality::parse_lenient("2"), Quality::Hd);
        assert_eq!(Quality::parse_lenient(""), Quality::Od);
        assert_eq!(Quality::parse_lenient("BD"), Quality::Od);
        assert_eq!(Quality::parse_lenient("9"), Quality::Od);
        assert_eq!(Quality::default(), Quality::Od);
        assert_eq!(Quality::Sd.label(), "标清");
    }

    #[test]
    fn select_variant_pads_short_lists_with_the_last_variant() {
        assert_eq!(select_variant_index(5, Quality::Hd), Some(2));
        assert_eq!(select_variant_index(3, Quality::Ld), Some(2));
        assert_eq!(select_variant_index(1, Quality::Uhd), Some(0));
        assert_eq!(select_variant_index(0, Quality::Od), None);
    }

    #[test]
    fn fallback_moves_one_tier_down_or_up_from_ld() {
        assert_eq!(fallback_variant_index(5, Quality::Od), Some(1));
        assert_eq!(fallback_variant_index(5, Quality::Sd), Some(4));
        assert_eq!(fallback_variant_index(5, Quality::Ld), Some(3));
        assert_eq!(fallback_variant_index(2, Quality::Uhd), Some(1));
    }

    #[test]
    fn neighbour_moves_down_or_up_from_the_lowest_variant() {
        assert_eq!(neighbour_variant_index(4, 0), Some(1));
        assert_eq!(neighbour_variant_index(4, 2), Some(3));
        assert_eq!(neighbour_variant_index(4, 3), Some(2));
        assert_eq!(neighbour_variant_index(4, 9), Some(2));
        assert_eq!(neighbour_variant_index(1, 0), Some(0));
        assert_eq!(neighbour_variant_index(0, 0), None);
    }

    #[test]
    fn record_url_prefers_hls_then_flv() {
        let both = variant("A", Some("https://h/a.m3u8"), Some("https://f/a.flv"));
        assert_eq!(
            selected_stream(Quality::Od, &both).unwrap().record_url,
            "https://h/a.m3u8"
        );
        let flv_only = variant("B", Some(""), Some("https://f/b.flv"));
        assert_eq!(
            selected_stream(Quality::Od, &flv_only).unwrap().record_url,
            "https://f/b.flv"
        );
        assert!(selected_stream(Quality::Od, &variant("C", None, None)).is_none());
    }

    #[test]
    fn source_url_prefers_flv_unless_hls_requested_or_h265() {
        let h264 = selected_stream(
            Quality::Od,
            &variant(
                "ORIGIN",
                Some("https://h/a.m3u8?codec=h264"),
                Some("https://f/a.flv?x=1&codec=h264"),
            ),
        )
        .unwrap();
        assert_eq!(
            h264.source_url(true, false),
            (SourceFormat::Flv, "https://f/a.flv?x=1&codec=h264")
        );
        assert_eq!(
            h264.source_url(true, true),
            (SourceFormat::Hls, "https://h/a.m3u8?codec=h264")
        );
        assert_eq!(
            h264.source_url(false, false),
            (SourceFormat::Hls, "https://h/a.m3u8?codec=h264")
        );

        let h265 = selected_stream(
            Quality::Od,
            &variant(
                "ORIGIN",
                Some("https://h/a.m3u8?codec=h265"),
                Some("https://f/a.flv?codec=h265"),
            ),
        )
        .unwrap();
        assert_eq!(h265.flv_codec().as_deref(), Some("h265"));
        assert_eq!(
            h265.source_url(true, false),
            (SourceFormat::Hls, "https://h/a.m3u8?codec=h265")
        );
    }

    #[test]
    fn proxy_gets_http_scheme_when_missing() {
        assert_eq!(
            normalize_proxy("127.0.0.1:7890").unwrap().as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            normalize_proxy("socks5://127.0.0.1:1080")
                .unwrap()
                .as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
        assert_eq!(normalize_proxy("  ").unwrap(), None);
    }

    #[test]
    fn socks_proxies_build_and_unknown_schemes_are_config_errors() {
        for proxy in [
            "socks5://127.0.0.1:7891",
            "socks5h://127.0.0.1:7891",
            "127.0.0.1:7890",
        ] {
            assert!(build_client(Some(proxy)).is_ok(), "{proxy}");
        }
        for proxy in ["ftp://127.0.0.1:21", "quic://127.0.0.1:1"] {
            assert_eq!(
                normalize_proxy(proxy).unwrap_err().kind(),
                "config",
                "{proxy}"
            );
            assert_eq!(
                build_client(Some(proxy)).unwrap_err().kind(),
                "config",
                "{proxy}"
            );
        }
    }

    #[test]
    fn client_is_reused_until_the_proxy_changes() {
        let cache = ClientCache::default();
        let cached_key = || cache.0.lock().unwrap().as_ref().unwrap().0.clone();
        cache.get(None).unwrap();
        cache.get(Some("  ")).unwrap();
        assert_eq!(cached_key(), None);
        cache.get(Some("127.0.0.1:7890")).unwrap();
        assert_eq!(cached_key().as_deref(), Some("127.0.0.1:7890"));
        assert_eq!(
            cache.get(Some("ftp://127.0.0.1:21")).unwrap_err().kind(),
            "config"
        );
    }

    #[test]
    fn http_statuses_map_to_error_kinds() {
        assert_eq!(classify_http_status(401, "x").kind(), "auth_required");
        assert_eq!(classify_http_status(403, "x").kind(), "rate_limited");
        assert_eq!(classify_http_status(429, "x").kind(), "rate_limited");
        assert_eq!(classify_http_status(404, "x").kind(), "invalid_url");
        assert_eq!(classify_http_status(502, "x").kind(), "network");
    }

    /// One-shot local HTTP server: answers the first request with `head` and
    /// `body`, then keeps the connection open for `linger` before closing.
    /// Returns the URL and the request head it received.
    async fn one_shot_server(
        head: String,
        body: Vec<u8>,
        linger: Duration,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let _ = sender.send(String::from_utf8_lossy(&request).to_string());
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(linger).await;
        });
        (format!("http://{addr}/api"), receiver)
    }

    #[tokio::test]
    async fn platform_client_asks_for_gzip_and_decodes_it() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let json = format!(r#"{{"data":"{}"}}"#, "直播".repeat(2000));
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(json.as_bytes()).unwrap();
        let gzipped = encoder.finish().unwrap();
        assert!(gzipped.len() < json.len() / 10);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            gzipped.len()
        );
        let (url, request) = one_shot_server(head, gzipped, Duration::ZERO).await;

        let client = build_client(None).unwrap();
        let text = client.get(&url).send().await.unwrap().text().await.unwrap();

        assert_eq!(text, json);
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(
            request
                .lines()
                .any(|line| line.starts_with("accept-encoding:") && line.contains("gzip")),
            "{request}"
        );
    }

    #[tokio::test]
    async fn stalled_body_is_a_readable_network_timeout() {
        // 头已发出、body 卡住：reqwest 报「error decoding response body」，要归为网络超时。
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n";
        let (url, _request) =
            one_shot_server(head.into(), b"{\"a\":".to_vec(), Duration::from_secs(5)).await;
        let client = reqwest::Client::builder()
            .read_timeout(Duration::from_millis(200))
            .build()
            .unwrap();

        let error = client
            .get(&url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap_err();
        let error = PlatformError::from(error);

        assert_eq!(error.kind(), "network", "{error:?}");
        assert!(error.message().contains("请求超时"), "{error:?}");
        assert!(error.message().contains("127.0.0.1"), "{error:?}");
    }

    #[tokio::test]
    async fn truncated_body_is_a_network_error_not_parse() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n";
        let (url, _request) =
            one_shot_server(head.into(), b"{\"a\":".to_vec(), Duration::ZERO).await;

        let client = build_client(None).unwrap();
        let error = client
            .get(&url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap_err();
        let error = PlatformError::from(error);

        assert_eq!(error.kind(), "network", "{error:?}");
        assert!(error.message().contains("连接中断"), "{error:?}");
    }

    #[test]
    fn extracts_the_link_from_share_texts() {
        let cases = [
            (
                "【小明的直播】 https://v.douyin.com/iQFeBnt/ 复制此链接，打开Dou音搜索，直接观看直播！",
                "https://v.douyin.com/iQFeBnt/",
            ),
            (
                "8- #在抖音，记录美好生活#【小明】正在直播，来和我一起支持Ta吧。复制下方链接，打开【抖音】，直接观看直播！ https://v.douyin.com/CeiU5cbX/ 4@0.com :8pm",
                "https://v.douyin.com/CeiU5cbX/",
            ),
            (
                "https://live.douyin.com/745964462470 看直播",
                "https://live.douyin.com/745964462470",
            ),
            (
                "快来看https://b23.tv/oolIxoB，好看！",
                "https://b23.tv/oolIxoB",
            ),
            ("(https://www.huya.com/lpl).", "https://www.huya.com/lpl"),
            ("https://example.com/a_(b) end", "https://example.com/a_(b)"),
            ("HTTPS://LIVE.douyin.com/1。", "HTTPS://LIVE.douyin.com/1"),
            (
                "e2e+http://127.0.0.1:9/stream.flv",
                "e2e+http://127.0.0.1:9/stream.flv",
            ),
            (
                "  live.douyin.com/745964462470  ",
                "live.douyin.com/745964462470",
            ),
            (
                "看这个 live.bilibili.com/21452505。",
                "live.bilibili.com/21452505",
            ),
            (
                "https://live.douyin.com/?a=1&live_web_rid=2",
                "https://live.douyin.com/?a=1&live_web_rid=2",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(extract_link(input), Some(expected), "{input}");
        }
        for input in [
            "",
            "   ",
            "直播间",
            "abc",
            "https:// 空",
            "1.5 倍速",
            "ftp://x.com/1",
        ] {
            assert_eq!(extract_link(input), None, "{input}");
        }
    }

    /// Local server that answers each connection with the next response head.
    async fn sequential_server(heads: Vec<String>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for head in heads {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }
                let head = head.replace("{addr}", &addr.to_string());
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
        });
        format!("http://{addr}")
    }

    fn redirect(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    #[tokio::test]
    async fn short_link_stops_at_the_first_accepted_target_without_requesting_it() {
        let base = sequential_server(vec![
            redirect("/hop"),
            redirect("https://live.bilibili.com/21452505?share_source=COPY"),
        ])
        .await;
        let target = follow_short_link(
            None,
            &format!("{base}/abc"),
            reqwest::header::HeaderMap::new(),
            |url| url.host_str() == Some("live.bilibili.com"),
        )
        .await
        .unwrap();
        assert_eq!(
            target,
            "https://live.bilibili.com/21452505?share_source=COPY"
        );
    }

    #[tokio::test]
    async fn short_link_reports_where_unaccepted_redirects_end() {
        let base = sequential_server(vec![
            redirect("http://{addr}/landing"),
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        ])
        .await;
        let target = follow_short_link(
            None,
            &format!("{base}/abc"),
            reqwest::header::HeaderMap::new(),
            |_| false,
        )
        .await
        .unwrap();
        assert_eq!(target, format!("{base}/landing"));

        let base = sequential_server(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        ])
        .await;
        let error = follow_short_link(
            None,
            &format!("{base}/gone"),
            reqwest::header::HeaderMap::new(),
            |_| true,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), "invalid_url");
    }
}
