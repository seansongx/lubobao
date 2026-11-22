//! 按下载源顺序流式下载 tarball，支持 HTTP Range 续传。

use std::path::Path;
use std::time::Duration;

use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use super::error::InstallError;

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

pub fn build_client(options: &DownloadOptions) -> Client {
    Client::builder()
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

/// 把 `total` 字节下载到 `part`。`part` 已有的内容视为之前下载的前缀并续传。
pub async fn download(
    client: &Client,
    options: &DownloadOptions,
    urls: &[String],
    part: &Path,
    total: u64,
    progress: Progress<'_>,
) -> Result<(), InstallError> {
    let mut received_any = false;
    let mut last_error = String::from("没有可用的下载地址");

    for url in urls {
        let source = source_name(url);
        let mut stalls = 0;
        while stalls < options.attempts_per_source.max(1) {
            let before = file_len(part).await;
            let result =
                fetch_once(client, options, url, &source, part, total, &mut *progress).await;
            let after = file_len(part).await;
            match result {
                Ok(()) => return Ok(()),
                Err(err) => last_error = format!("{source}：{err}"),
            }
            if after > before {
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

    let downloaded = file_len(part).await;
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

async fn file_len(path: &Path) -> u64 {
    fs::metadata(path).await.map(|meta| meta.len()).unwrap_or(0)
}

/// 一次 HTTP 请求。返回的错误是给用户看的简短说明。
async fn fetch_once(
    client: &Client,
    options: &DownloadOptions,
    url: &str,
    source: &str,
    part: &Path,
    total: u64,
    progress: Progress<'_>,
) -> Result<(), String> {
    let mut offset = file_len(part).await;
    if offset > total {
        let _ = fs::remove_file(part).await;
        offset = 0;
    }
    if offset == total && total > 0 {
        return Ok(());
    }

    let mut request = client.get(url);
    if offset > 0 {
        request = request.header(RANGE, format!("bytes={offset}-"));
    }
    let response = match tokio::time::timeout(
        options.connect_timeout + options.read_timeout,
        request.send(),
    )
    .await
    {
        Err(_) => return Err("连接超时".into()),
        Ok(Err(err)) => return Err(describe_request_error(&err)),
        Ok(Ok(response)) => response,
    };

    let status = response.status();
    let append = match status {
        StatusCode::PARTIAL_CONTENT if offset > 0 => {
            let start = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(content_range_start);
            if start != Some(offset) {
                return Err("服务器返回的续传范围不正确".into());
            }
            true
        }
        StatusCode::OK => {
            offset = 0;
            false
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let _ = fs::remove_file(part).await;
            return Err("已下载的部分无法续传，将重新下载".into());
        }
        other => return Err(format!("服务器返回 HTTP {}", other.as_u16())),
    };

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(part)
        .await
        .map_err(|err| format!("无法写入临时文件：{err}"))?;

    progress(offset, total, source);
    let mut response = response;
    loop {
        let chunk = match tokio::time::timeout(options.read_timeout, response.chunk()).await {
            Err(_) => {
                let _ = file.flush().await;
                return Err("读取超时".into());
            }
            Ok(Err(err)) => {
                let _ = file.flush().await;
                return Err(describe_request_error(&err));
            }
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => chunk,
        };
        let len = chunk.len() as u64;
        if offset + len > total {
            drop(file);
            let _ = fs::remove_file(part).await;
            return Err("下载的数据超出预期大小".into());
        }
        file.write_all(&chunk)
            .await
            .map_err(|err| format!("无法写入临时文件：{err}"))?;
        offset += len;
        progress(offset, total, source);
    }
    file.flush()
        .await
        .map_err(|err| format!("无法写入临时文件：{err}"))?;
    file.sync_all()
        .await
        .map_err(|err| format!("无法写入临时文件：{err}"))?;

    if offset == total {
        Ok(())
    } else {
        Err("连接提前结束".into())
    }
}

fn describe_request_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "连接超时".into()
    } else if err.is_connect() {
        "无法连接".into()
    } else {
        format!("网络错误：{err}")
    }
}

/// 解析 `bytes <start>-<end>/<total>` 中的起始位置。
fn content_range_start(value: &str) -> Option<u64> {
    let range = value.trim().strip_prefix("bytes")?.trim_start();
    let (start, _) = range.split_once('-')?;
    start.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range_start() {
        assert_eq!(content_range_start("bytes 100-199/200"), Some(100));
        assert_eq!(content_range_start("bytes */200"), None);
        assert_eq!(content_range_start("items 1-2/3"), None);
    }

    #[test]
    fn source_name_is_host() {
        assert_eq!(
            source_name("https://registry.npmmirror.com/@seankit/ffmpeg-win32-x64/-/x.tgz"),
            "registry.npmmirror.com"
        );
    }
}
