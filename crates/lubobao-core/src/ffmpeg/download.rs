//! 按下载源顺序流式下载 tarball，支持 HTTP Range 续传。
//!
//! `.part` 旁边的 `.part.src` 记录这份部分文件来自哪个地址，只有同一地址才续传，
//! 避免不同下载源的前后两段拼在一起。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use super::error::InstallError;

const MAX_ERROR_CHARS: usize = 160;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// 建立连接的超时。
    pub connect_timeout: Duration,
    /// 等待响应头或下一块数据的超时。
    pub read_timeout: Duration,
    /// 每个下载源连续多少次没有拿到新数据后换下一个源。
    pub attempts_per_source: u32,
    /// 同一下载源两次尝试之间的等待时间，按尝试次数递增。
    pub retry_delay: Duration,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            attempts_per_source: 3,
            retry_delay: Duration::from_secs(2),
        }
    }
}

/// 下载用的 client。关闭自动解压：要按 Content-Length 计数并对原始字节算 SHA-256，
/// 源站若给 `.tgz` 加上 `Content-Encoding: gzip`，解压后的内容会校验失败。
pub fn build_client(options: &DownloadOptions) -> Client {
    Client::builder()
        .no_gzip()
        .connect_timeout(options.connect_timeout)
        .user_agent(concat!("lubobao/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("HTTP client configuration is valid")
}

/// 下载源的显示名称：URL 中的主机名。
pub fn source_name(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// 进度回调：已下载字节数、总字节数、当前下载源。
pub type Progress<'a> = &'a mut (dyn FnMut(u64, u64, &str) + Send);

/// 记录 `.part` 来源地址的文件。
pub fn source_record(part: &Path) -> PathBuf {
    let mut name = OsString::from(part.as_os_str());
    name.push(".src");
    PathBuf::from(name)
}

/// 删除部分文件和它的来源记录。
pub async fn discard_part(part: &Path) {
    let _ = fs::remove_file(part).await;
    let _ = fs::remove_file(source_record(part)).await;
}

async fn read_source(part: &Path) -> Option<String> {
    let text = fs::read_to_string(source_record(part)).await.ok()?;
    Some(text.trim().to_string())
}

/// 只统计普通文件；目录或不存在都算 0。
async fn part_len(path: &Path) -> u64 {
    match fs::metadata(path).await {
        Ok(meta) if meta.is_file() => meta.len(),
        _ => 0,
    }
}

/// 把 `total` 字节下载到 `part`，返回完成下载的地址。
///
/// `part` 已有的内容若来自列表中的某个地址，先从那个地址续传；
/// 换到别的地址时从头下载。本地读写失败直接返回，不再换源。
pub async fn download(
    client: &Client,
    options: &DownloadOptions,
    urls: &[String],
    part: &Path,
    total: u64,
    progress: Progress<'_>,
) -> Result<String, InstallError> {
    let mut order: Vec<&String> = urls.iter().collect();
    if part_len(part).await > 0
        && let Some(recorded) = read_source(part).await
        && let Some(index) = order.iter().position(|url| **url == recorded)
    {
        let url = order.remove(index);
        order.insert(0, url);
    }

    let mut received_any = false;
    let mut last_error = String::from("没有可用的下载地址");

    for url in order {
        let source = source_name(url);
        let mut stalls = 0;
        while stalls < options.attempts_per_source.max(1) {
            let mut written = 0;
            let result = fetch_once(
                client,
                options,
                url,
                &source,
                part,
                total,
                &mut written,
                &mut *progress,
            )
            .await;
            match result {
                Ok(()) => return Ok(url.clone()),
                Err(FetchError::Local(err)) => return Err(err),
                Err(FetchError::Network(err)) => last_error = format!("{source}：{err}"),
            }
            if written > 0 {
                received_any = true;
                stalls = 0;
            } else {
                stalls += 1;
                if stalls < options.attempts_per_source {
                    tokio::time::sleep(options.retry_delay * stalls).await;
                }
            }
        }
    }

    let downloaded = part_len(part).await;
    if received_any || downloaded > 0 {
        Err(InstallError::Interrupted {
            downloaded,
            total,
            last_error,
        })
    } else {
        Err(InstallError::SourcesUnreachable {
            sources: urls.len(),
            last_error,
        })
    }
}

enum FetchError {
    /// 网络或服务器的问题，可以重试或换源。
    Network(String),
    /// 本地读写失败，换源也没用。
    Local(InstallError),
}

impl From<String> for FetchError {
    fn from(message: String) -> Self {
        Self::Network(message)
    }
}

impl From<&str> for FetchError {
    fn from(message: &str) -> Self {
        Self::Network(message.to_string())
    }
}

fn local(err: std::io::Error) -> FetchError {
    FetchError::Local(InstallError::io("写入下载的临时文件", err))
}

/// 一次 HTTP 请求。`written` 累计本次写入的字节数。
#[allow(clippy::too_many_arguments)]
async fn fetch_once(
    client: &Client,
    options: &DownloadOptions,
    url: &str,
    source: &str,
    part: &Path,
    total: u64,
    written: &mut u64,
    progress: Progress<'_>,
) -> Result<(), FetchError> {
    let mut offset = part_len(part).await;
    if offset > 0 && (offset > total || read_source(part).await.as_deref() != Some(url)) {
        discard_part(part).await;
        offset = 0;
    }
    if offset == total && total > 0 {
        return Ok(());
    }

    let mut request = client.get(url);
    if offset > 0 {
        request = request.header(RANGE, format!("bytes={offset}-"));
    }
    let mut response = match tokio::time::timeout(
        options.connect_timeout + options.read_timeout,
        request.send(),
    )
    .await
    {
        Err(_) => return Err("连接超时".into()),
        Ok(Err(err)) => return Err(describe_request_error(&err).into()),
        Ok(Ok(response)) => response,
    };

    let append = match response.status() {
        StatusCode::PARTIAL_CONTENT if offset > 0 => {
            let range = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range);
            match range {
                Some((_, Some(size))) if size != total => {
                    return Err(size_mismatch(size, total));
                }
                Some((start, _)) if start == offset => true,
                _ => return Err("服务器返回的续传范围不正确".into()),
            }
        }
        StatusCode::OK => {
            if let Some(size) = response.content_length()
                && size != total
            {
                return Err(size_mismatch(size, total));
            }
            offset = 0;
            false
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            discard_part(part).await;
            return Err("已下载的部分无法续传，将重新下载".into());
        }
        other => return Err(format!("服务器返回 HTTP {}", other.as_u16()).into()),
    };

    if !append {
        fs::write(source_record(part), url).await.map_err(local)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(part)
        .await
        .map_err(local)?;

    progress(offset, total, source);
    loop {
        let chunk = match tokio::time::timeout(options.read_timeout, response.chunk()).await {
            Err(_) => {
                file.flush().await.map_err(local)?;
                return Err("读取超时".into());
            }
            Ok(Err(err)) => {
                file.flush().await.map_err(local)?;
                return Err(describe_request_error(&err).into());
            }
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => chunk,
        };
        let len = chunk.len() as u64;
        if offset + len > total {
            drop(file);
            discard_part(part).await;
            return Err("下载的数据超出预期大小".into());
        }
        file.write_all(&chunk).await.map_err(local)?;
        offset += len;
        *written += len;
        progress(offset, total, source);
    }
    file.flush().await.map_err(local)?;
    file.sync_all().await.map_err(local)?;

    if offset == total {
        Ok(())
    } else {
        Err("连接提前结束".into())
    }
}

fn size_mismatch(size: u64, total: u64) -> FetchError {
    format!("服务器上的文件大小（{size} 字节）与清单（{total} 字节）不一致").into()
}

/// 错误类别加上底层原因链，例如“无法连接：tls handshake eof”。
fn describe_request_error(err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "连接超时"
    } else if err.is_connect() {
        "无法连接"
    } else {
        "网络错误"
    };
    let mut details: Vec<String> = Vec::new();
    let mut cause = std::error::Error::source(err);
    while let Some(inner) = cause {
        let text = inner.to_string();
        if !text.is_empty() && !details.iter().any(|seen| seen.contains(&text)) {
            details.push(text);
        }
        cause = inner.source();
    }
    let text = if details.is_empty() {
        kind.to_string()
    } else {
        format!("{kind}：{}", details.join("："))
    };
    if text.chars().count() > MAX_ERROR_CHARS {
        let mut short: String = text.chars().take(MAX_ERROR_CHARS).collect();
        short.push('…');
        short
    } else {
        text
    }
}

/// 解析 `bytes <start>-<end>/<total>`，返回起始位置和总长度（`*` 时为 `None`）。
fn parse_content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let range = value.trim().strip_prefix("bytes")?.trim_start();
    let (span, size) = range.split_once('/')?;
    let (start, _) = span.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let size = match size.trim() {
        "*" => None,
        size => Some(size.parse().ok()?),
    };
    Some((start, size))
}

/// 删除 `temp` 中不属于 `keep` 的部分文件（旧版本留下的 `.part` 和 `.part.src`）。
pub async fn remove_foreign_parts(temp: &Path, keep: &Path) {
    let keep_record = source_record(keep);
    let Ok(mut entries) = fs::read_dir(temp).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_part = name.ends_with(".part") || name.ends_with(".part.src");
        if is_part && path != keep && path != keep_record {
            let _ = fs::remove_file(&path).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(
            parse_content_range("bytes 100-199/200"),
            Some((100, Some(200)))
        );
        assert_eq!(parse_content_range("bytes 5-9/*"), Some((5, None)));
        assert_eq!(parse_content_range("bytes */200"), None);
        assert_eq!(parse_content_range("items 1-2/3"), None);
    }

    #[test]
    fn source_name_is_host() {
        assert_eq!(
            source_name("https://registry.npmmirror.com/@seankit/ffmpeg-win32-x64/-/x.tgz"),
            "registry.npmmirror.com"
        );
    }

    #[test]
    fn source_record_sits_next_to_part() {
        assert_eq!(
            source_record(Path::new("a/b.tgz.part")),
            PathBuf::from("a/b.tgz.part.src")
        );
    }

    #[tokio::test]
    async fn connection_error_includes_cause() {
        let client = build_client(&DownloadOptions::default());
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", closed.local_addr().unwrap());
        drop(closed);
        let err = client.get(url).send().await.unwrap_err();
        let text = describe_request_error(&err);
        assert!(text.starts_with("无法连接："), "{text}");
        assert!(
            text.chars().count() > "无法连接：".chars().count() + 3,
            "{text}"
        );
    }
}
