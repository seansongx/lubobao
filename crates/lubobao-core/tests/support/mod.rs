//! Helpers shared by the live tests of platforms that need a proxy from
//! mainland China (TikTok, Twitch, YouTube).
//!
//! `LUBOBAO_TEST_PROXY` names the HTTP proxy to use (for example
//! `http://127.0.0.1:7890`). The tests do not hand it to the adapter and
//! FFmpeg directly: they start a local [`ProxyRelay`] that forwards every
//! connection to it and records the request line, then pass the relay's
//! address as the proxy. A transparent (TUN) proxy would let direct
//! connections succeed as well, so only the relay log shows that detection,
//! the playlist and the media segments really went through the configured
//! proxy.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The upstream proxy from `LUBOBAO_TEST_PROXY`, when set.
pub fn upstream_proxy() -> Option<String> {
    std::env::var("LUBOBAO_TEST_PROXY")
        .ok()
        .map(|proxy| proxy.trim().to_string())
        .filter(|proxy| !proxy.is_empty())
}

/// Local HTTP proxy that relays each connection to an upstream HTTP proxy
/// and logs the first request line (`CONNECT host:443 HTTP/1.1` or
/// `GET http://host/… HTTP/1.1`).
pub struct ProxyRelay {
    pub url: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl ProxyRelay {
    pub async fn start(upstream: &str) -> ProxyRelay {
        let upstream = upstream
            .trim()
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        assert!(
            !upstream.contains("://"),
            "LUBOBAO_TEST_PROXY must be an HTTP proxy: {upstream}"
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let relay_log = log.clone();
        tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    return;
                };
                let upstream = upstream.clone();
                let log = relay_log.clone();
                tokio::spawn(async move {
                    let _ = relay(client, &upstream, &log).await;
                });
            }
        });
        ProxyRelay { url, log }
    }

    /// Request lines seen so far.
    pub fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// Number of connections seen so far.
    pub fn count(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// Hosts of the requests from position `from` on.
    pub fn hosts_since(&self, from: usize) -> Vec<String> {
        self.requests()
            .into_iter()
            .skip(from)
            .filter_map(|line| request_host(&line))
            .collect()
    }

    /// Whether a request to `host` was relayed at position `from` or later.
    pub fn saw_host_since(&self, from: usize, host: &str) -> bool {
        self.hosts_since(from).iter().any(|seen| seen == host)
    }
}

/// Host of a proxy request line.
pub fn request_host(line: &str) -> Option<String> {
    let target = line.split_whitespace().nth(1)?;
    let authority = match target.split_once("://") {
        Some((_, rest)) => rest.split('/').next()?,
        None => target,
    };
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    Some(host.to_ascii_lowercase())
}

async fn relay(
    mut client: TcpStream,
    upstream: &str,
    log: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = client.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..read]);
        if head.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let line = String::from_utf8_lossy(&head)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    log.lock().unwrap().push(line);
    let mut server = TcpStream::connect(upstream).await?;
    server.write_all(&head).await?;
    tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    Ok(())
}

/// Host of the first media segment in an HLS media playlist, fetched through
/// `proxy`. `None` when the playlist cannot be read.
pub async fn first_segment_host(
    playlist_url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    proxy: Option<&str>,
) -> Option<String> {
    let mut builder = reqwest::Client::builder();
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).ok()?);
    }
    let client = builder.build().ok()?;
    let mut request = client.get(playlist_url);
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    let body = request.send().await.ok()?.text().await.ok()?;
    let base = reqwest::Url::parse(playlist_url).ok()?;
    let segment = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))?;
    let url = base.join(segment).ok()?;
    url.host_str().map(str::to_ascii_lowercase)
}

/// Host of a URL.
pub fn url_host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

/// FFmpeg from `LUBOBAO_TEST_FFMPEG` or `LUBOBAO_FFMPEG`, falling back to the
/// app's installed component under `%LOCALAPPDATA%`.
pub fn ffmpeg_path() -> PathBuf {
    for var in ["LUBOBAO_TEST_FFMPEG", "LUBOBAO_FFMPEG"] {
        if let Ok(path) = std::env::var(var) {
            return PathBuf::from(path);
        }
    }
    let base = std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA or LUBOBAO_TEST_FFMPEG");
    PathBuf::from(base)
        .join("com.xiaofany.lubobao")
        .join("components")
        .join("ffmpeg")
        .join("9.0.2-9.0.1")
        .join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        })
}

/// Decode the whole file with FFmpeg and fail on any error. Repeated source
/// timestamps are reported by the null muxer but every frame decodes; they
/// are allowed, same rule as `huya_live.rs`.
pub fn assert_decodes(ffmpeg: &Path, file: &Path) {
    let check = std::process::Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg check");
    let stderr = String::from_utf8_lossy(&check.stderr);
    const SOURCE_DTS: &str = "non monotonically increasing dts";
    let repeated = stderr
        .lines()
        .filter(|line| line.contains(SOURCE_DTS))
        .count();
    // FFmpeg folds repeats of the same message into a summary line.
    let errors: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.contains(SOURCE_DTS) && !line.contains("Last message repeated"))
        .collect();
    println!(
        "check exit={:?} stderr_lines={} repeated_source_dts={repeated}",
        check.status.code(),
        stderr.lines().count()
    );
    assert!(check.status.success(), "ffmpeg check failed: {stderr}");
    assert!(errors.is_empty(), "ffmpeg reported errors: {errors:#?}");
}

/// Print the codec, size and duration of the recording.
pub fn describe_file(ffmpeg: &Path, file: &Path) {
    let output = std::process::Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-i")
        .arg(file)
        .output();
    if let Ok(output) = output {
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            let line = line.trim();
            if line.starts_with("Duration:") || line.starts_with("Stream #") {
                println!("     {line}");
            }
        }
    }
}
