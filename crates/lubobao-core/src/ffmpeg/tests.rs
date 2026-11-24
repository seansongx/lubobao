//! 用自己生成的小 tgz 和本地 HTTP 服务测试下载、校验和安装。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::install::sha256_bytes;
use super::*;

const BINARY: &[u8] = b"#!fake ffmpeg binary\nffmpeg version test\n";

fn make_tgz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, path, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// 包含 `package/bin/ffmpeg` 和一个无关文件的 tarball。
fn sample_tgz() -> Vec<u8> {
    let filler: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    make_tgz(&[
        ("package/package.json", &b"{}"[..]),
        ("package/README.md", &filler[..]),
        ("package/bin/ffmpeg", BINARY),
    ])
}

fn component_for(tgz: &[u8], urls: Vec<String>) -> Component {
    Component {
        ffmpeg_version: "9.0.2".into(),
        package_version: "9.0.1".into(),
        platform: "test".into(),
        package: PlatformPackage {
            file: "ffmpeg-test-9.0.1.tgz".into(),
            size: tgz.len() as u64,
            sha256: sha256_bytes(tgz),
            binary: "bin/ffmpeg".into(),
            binary_sha256: sha256_bytes(BINARY),
            binary_size: BINARY.len() as u64,
            rust_targets: vec![],
            urls,
        },
    }
}

fn with_versions(mut component: Component, ffmpeg: &str, package: &str) -> Component {
    component.ffmpeg_version = ffmpeg.into();
    component.package_version = package.into();
    component
}

#[derive(Clone, Default)]
struct ProbeLog(Arc<AtomicUsize>);

impl ProbeLog {
    fn probe(&self) -> Probe {
        let calls = Arc::clone(&self.0);
        Arc::new(move |path: &Path| -> Result<String, String> {
            calls.fetch_add(1, Ordering::SeqCst);
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            if bytes == BINARY {
                Ok("ffmpeg version test".into())
            } else {
                Err("unexpected binary".into())
            }
        })
    }

    fn calls(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    root: PathBuf,
    probe: ProbeLog,
    events: Arc<Mutex<Vec<FfmpegStatus>>>,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("components").join("ffmpeg");
        Self {
            _dir: dir,
            root,
            probe: ProbeLog::default(),
            events: Arc::default(),
        }
    }

    fn manager(&self, component: Component) -> FfmpegManager {
        self.manager_with_probe(component, self.probe.probe())
    }

    fn manager_with_probe(&self, component: Component, probe: Probe) -> FfmpegManager {
        let events = Arc::clone(&self.events);
        FfmpegManager::new(
            ManagerConfig {
                root: self.root.clone(),
                component: Ok(component),
                download: DownloadOptions {
                    connect_timeout: Duration::from_secs(5),
                    read_timeout: Duration::from_secs(5),
                    attempts_per_source: 2,
                    retry_delay: Duration::ZERO,
                },
                probe,
            },
            Box::new(move |status: &FfmpegStatus| events.lock().unwrap().push(status.clone())),
        )
    }

    fn installed(&self) -> PathBuf {
        self.root.join("9.0.2-9.0.1").join("ffmpeg")
    }

    fn temp_files(&self) -> Vec<String> {
        let mut names = Vec::new();
        collect_files(&self.root.join(".tmp"), &mut names);
        names.retain(|name| !name.ends_with("lock"));
        names
    }
}

fn collect_files(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path.display().to_string());
        }
    }
}

// ---------- 本地 HTTP 服务 ----------

enum Route {
    /// 完整提供文件，支持 Range。
    File(Vec<u8>),
    /// 第一次请求只发送前 `cut` 个字节就断开，之后正常提供。
    CutOnce { body: Vec<u8>, cut: usize },
    /// 第一次请求只发送前 `cut` 个字节就断开，之后返回 HTTP 500。
    CutThenFail { body: Vec<u8>, cut: usize },
    /// 固定返回某个状态码。
    Status(u16),
    /// 第 n 次请求按 `steps[n - 1]` 响应，超出后重复最后一步。
    Script { body: Vec<u8>, steps: Vec<Step> },
}

#[derive(Debug, Clone, Copy)]
enum Step {
    /// 正常提供，支持 Range。
    Serve,
    /// 从请求的位置开始只发送 `n` 个字节就断开。
    Cut(usize),
    /// 返回某个状态码。
    Status(u16),
    /// 忽略 Range，返回 200 和完整文件。
    IgnoreRange,
    /// 返回 206，但 Content-Range 的起点比请求的位置多 1。
    WrongStart,
    /// 返回 200，Content-Length 比文件少 1。
    ShortLength,
    /// 返回 200 和完整文件，另加 `Content-Encoding: gzip`（有的 CDN 这样提供 `.tgz`）。
    GzipEncoding,
}

impl Route {
    fn step(&self, hit: usize) -> (&[u8], Step) {
        let pick = |steps: &[Step]| steps[(hit - 1).min(steps.len() - 1)];
        match self {
            Route::File(body) => (body, Step::Serve),
            Route::CutOnce { body, cut } => (body, pick(&[Step::Cut(*cut), Step::Serve])),
            Route::CutThenFail { body, cut } => (body, pick(&[Step::Cut(*cut), Step::Status(500)])),
            Route::Status(code) => (&[], Step::Status(*code)),
            Route::Script { body, steps } => (body, pick(steps)),
        }
    }
}

#[derive(Debug, Clone)]
struct Request {
    path: String,
    range: Option<String>,
}

#[derive(Clone)]
struct TestServer {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Request>>>,
}

impl TestServer {
    async fn start(routes: Vec<(&str, Route)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes: Arc<HashMap<String, Route>> = Arc::new(
            routes
                .into_iter()
                .map(|(path, route)| (path.to_string(), route))
                .collect(),
        );
        let log: Arc<Mutex<Vec<Request>>> = Arc::default();
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let server_log = Arc::clone(&log);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let routes = Arc::clone(&routes);
                let log = Arc::clone(&server_log);
                let hits = Arc::clone(&hits);
                tokio::spawn(async move {
                    let _ = serve(stream, &routes, &log, &hits).await;
                });
            }
        });
        Self { addr, log }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn requests(&self) -> Vec<Request> {
        self.log.lock().unwrap().clone()
    }
}

async fn serve(
    mut stream: TcpStream,
    routes: &HashMap<String, Route>,
    log: &Mutex<Vec<Request>>,
    hits: &Mutex<HashMap<String, usize>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let text = String::from_utf8_lossy(&buffer).to_string();
    let mut lines = text.lines();
    let path = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let range = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("range"))
        .map(|(_, value)| value.trim().to_string());
    log.lock().unwrap().push(Request {
        path: path.clone(),
        range: range.clone(),
    });
    let hit = {
        let mut hits = hits.lock().unwrap();
        let count = hits.entry(path.clone()).or_default();
        *count += 1;
        *count
    };

    let Some(route) = routes.get(&path) else {
        return respond_status(&mut stream, 404).await;
    };
    let (body, step) = route.step(hit);
    let requested = range
        .as_deref()
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|r| r.parse::<usize>().ok());
    let (start, cut) = match step {
        Step::Status(code) => return respond_status(&mut stream, code).await,
        Step::IgnoreRange | Step::ShortLength | Step::GzipEncoding => (None, None),
        Step::Cut(cut) => (requested, Some(cut)),
        Step::Serve | Step::WrongStart => (requested, None),
    };
    let head = match start {
        Some(start) if start >= body.len() => return respond_status(&mut stream, 416).await,
        Some(start) => {
            let claimed = if matches!(step, Step::WrongStart) {
                start + 1
            } else {
                start
            };
            format!(
                "HTTP/1.1 206 Partial Content
Content-Length: {}
Content-Range: bytes {}-{}/{}
Connection: close

",
                body.len() - start,
                claimed,
                body.len() - 1,
                body.len()
            )
        }
        None => {
            let length = if matches!(step, Step::ShortLength) {
                body.len() - 1
            } else {
                body.len()
            };
            let encoding = if matches!(step, Step::GzipEncoding) {
                "Content-Encoding: gzip\r\n"
            } else {
                ""
            };
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nAccept-Ranges: bytes\r\n{encoding}Connection: close\r\n\r\n"
            )
        }
    };
    let start = start.unwrap_or(0);
    let mut end = match cut {
        Some(cut) => (start + cut).min(body.len()),
        None => body.len(),
    };
    if matches!(step, Step::ShortLength) {
        end -= 1;
    }
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body[start..end]).await?;
    stream.flush().await?;
    stream.shutdown().await
}

async fn respond_status(stream: &mut TcpStream, code: u16) -> std::io::Result<()> {
    let head = format!("HTTP/1.1 {code} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.shutdown().await
}

// ---------- 测试 ----------

#[tokio::test]
async fn installs_from_first_source_and_reports_progress() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
    assert_eq!(manager.ffmpeg_path(), Some(harness.installed()));
    assert_eq!(harness.probe.calls(), 1);
    assert!(
        harness.temp_files().is_empty(),
        "{:?}",
        harness.temp_files()
    );
    let events = harness.events.lock().unwrap().clone();
    // 初始状态就是“检查中”，不会重复发事件。
    assert_eq!(events.first(), Some(&FfmpegStatus::NotInstalled));
    assert!(events.contains(&FfmpegStatus::Downloading {
        downloaded: tgz.len() as u64,
        total: tgz.len() as u64,
        source: "127.0.0.1".into(),
    }));
    assert!(events.contains(&FfmpegStatus::Verifying));
    assert!(events.last().unwrap().is_ready());
}

#[tokio::test]
async fn gzip_content_encoding_keeps_raw_tarball_bytes() {
    // 工作区的 reqwest 打开了 gzip。源站给 .tgz 标上 Content-Encoding: gzip 时，
    // 下载必须保存原始字节，否则解压后的 tar 对不上 SHA-256。
    let tgz = sample_tgz();
    let server = TestServer::start(vec![(
        "/pkg.tgz",
        Route::Script {
            body: tgz.clone(),
            steps: vec![Step::GzipEncoding],
        },
    )])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
    assert_eq!(server.requests().len(), 1, "{:?}", server.requests());
}

#[tokio::test]
async fn tarball_hash_mismatch_does_not_extract_or_execute() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let mut component = component_for(&tgz, vec![server.url("/pkg.tgz")]);
    component.package.sha256 = "0".repeat(64);
    let manager = harness.manager(component);

    let status = manager.ensure().await;

    match status {
        FfmpegStatus::Failed { reason, retryable } => {
            assert!(reason.contains("校验不一致"), "{reason}");
            assert!(retryable);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(harness.probe.calls(), 0);
    assert!(!harness.installed().exists());
    assert!(
        harness.temp_files().is_empty(),
        "{:?}",
        harness.temp_files()
    );
}

#[tokio::test]
async fn binary_hash_mismatch_does_not_install() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let mut component = component_for(&tgz, vec![server.url("/pkg.tgz")]);
    component.package.binary_sha256 = "f".repeat(64);
    let manager = harness.manager(component);

    let status = manager.ensure().await;

    match status {
        FfmpegStatus::Failed { reason, .. } => {
            assert!(reason.contains("可执行文件校验不一致"), "{reason}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(harness.probe.calls(), 0);
    assert!(!harness.installed().exists());
    assert!(
        harness.temp_files().is_empty(),
        "{:?}",
        harness.temp_files()
    );
}

#[tokio::test]
async fn missing_binary_entry_fails_without_installing() {
    let tgz = make_tgz(&[("package/bin/other", BINARY)]);
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let status = manager.ensure().await;

    assert!(
        matches!(&status, FfmpegStatus::Failed { reason, .. } if reason.contains("package/bin/ffmpeg")),
        "{status:?}"
    );
    assert_eq!(harness.probe.calls(), 0);
    assert!(!harness.installed().exists());
}

#[tokio::test]
async fn failed_probe_does_not_install() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let probe: Probe =
        Arc::new(|_: &Path| -> Result<String, String> { Err("退出状态 1".to_string()) });
    let manager =
        harness.manager_with_probe(component_for(&tgz, vec![server.url("/pkg.tgz")]), probe);

    let status = manager.ensure().await;

    assert!(
        matches!(&status, FfmpegStatus::Failed { reason, .. } if reason.contains("无法正常运行")),
        "{status:?}"
    );
    assert!(!harness.installed().exists());
    assert!(
        harness.temp_files().is_empty(),
        "{:?}",
        harness.temp_files()
    );
}

#[tokio::test]
async fn falls_back_to_second_source_when_first_fails() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![
        ("/mirror.tgz", Route::Status(503)),
        ("/backup.tgz", Route::File(tgz.clone())),
    ])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(
        &tgz,
        vec![server.url("/mirror.tgz"), server.url("/backup.tgz")],
    ));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, ["/mirror.tgz", "/mirror.tgz", "/backup.tgz"]);
}

#[tokio::test]
async fn falls_back_when_first_source_cannot_connect() {
    let tgz = sample_tgz();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_url = format!("http://{}/pkg.tgz", closed.local_addr().unwrap());
    drop(closed);
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(
        &tgz,
        vec![closed_url, server.url("/pkg.tgz")],
    ));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
}

#[tokio::test]
async fn reports_both_sources_unreachable() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![
        ("/a.tgz", Route::Status(500)),
        ("/b.tgz", Route::Status(404)),
    ])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(
        &tgz,
        vec![server.url("/a.tgz"), server.url("/b.tgz")],
    ));

    let status = manager.ensure().await;

    match status {
        FfmpegStatus::Failed { reason, retryable } => {
            assert!(reason.starts_with("两个下载源都无法连接"), "{reason}");
            assert!(retryable);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(harness.probe.calls(), 0);
}

#[tokio::test]
async fn resumes_interrupted_download_with_range() {
    let tgz = sample_tgz();
    let cut = tgz.len() / 3;
    let server = TestServer::start(vec![(
        "/pkg.tgz",
        Route::CutOnce {
            body: tgz.clone(),
            cut,
        },
    )])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[0].range, None);
    assert_eq!(
        requests[1].range.as_deref(),
        Some(format!("bytes={cut}-").as_str())
    );
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
}

#[tokio::test]
async fn retry_after_failure_resumes_from_partial_file() {
    let tgz = sample_tgz();
    let cut = tgz.len() / 2;
    let server = TestServer::start(vec![
        (
            "/flaky.tgz",
            Route::Script {
                body: tgz.clone(),
                steps: vec![
                    Step::Cut(cut),
                    Step::Status(500),
                    Step::Status(500),
                    Step::Serve,
                ],
            },
        ),
        ("/good.tgz", Route::File(tgz.clone())),
    ])
    .await;
    let harness = Harness::new();

    // 第一次：A 源下了一半后失败。
    let first = harness.manager(component_for(&tgz, vec![server.url("/flaky.tgz")]));
    let status = first.ensure().await;
    match status {
        FfmpegStatus::Failed { reason, retryable } => {
            assert!(reason.contains("下载中断"), "{reason}");
            assert!(retryable);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(!harness.installed().exists());
    let part = harness.root.join(".tmp").join("ffmpeg-test-9.0.1.tgz.part");
    assert_eq!(std::fs::metadata(&part).unwrap().len(), cut as u64);

    // 重试时 A 源恢复：优先回到 A 源续传，不从 B 源重新开始。
    let second = harness.manager(component_for(
        &tgz,
        vec![server.url("/good.tgz"), server.url("/flaky.tgz")],
    ));
    let status = second.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let flaky: Vec<Request> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/flaky.tgz")
        .collect();
    let last = flaky.last().unwrap();
    assert_eq!(
        last.range.as_deref(),
        Some(format!("bytes={cut}-").as_str())
    );
    assert!(server.requests().iter().all(|r| r.path != "/good.tgz"));
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
}

#[tokio::test]
async fn partial_file_from_another_source_is_not_resumed() {
    let tgz = sample_tgz();
    let cut = tgz.len() / 2;
    let server = TestServer::start(vec![
        (
            "/a.tgz",
            Route::CutThenFail {
                body: tgz.clone(),
                cut,
            },
        ),
        ("/b.tgz", Route::File(tgz.clone())),
    ])
    .await;
    let harness = Harness::new();
    let first = harness.manager(component_for(&tgz, vec![server.url("/a.tgz")]));
    assert!(matches!(first.ensure().await, FfmpegStatus::Failed { .. }));

    // 下一次只有 B 源：A 源留下的前半段不能拼上 B 源的后半段。
    let second = harness.manager(component_for(&tgz, vec![server.url("/b.tgz")]));
    let status = second.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let b: Vec<Request> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/b.tgz")
        .collect();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].range, None);
}

#[tokio::test]
async fn restarts_when_partial_file_cannot_be_resumed() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    // 比真实文件还长的残留文件：直接丢弃后重新下载。
    std::fs::create_dir_all(harness.root.join(".tmp")).unwrap();
    std::fs::write(
        harness.root.join(".tmp").join("ffmpeg-test-9.0.1.tgz.part"),
        vec![0u8; tgz.len() + 10],
    )
    .unwrap();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    assert_eq!(server.requests()[0].range, None);
}

#[tokio::test]
async fn reuses_installed_version_without_network() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let first = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));
    assert!(first.ensure().await.is_ready());
    let requests_after_install = server.requests().len();
    harness.events.lock().unwrap().clear();

    // 第二次启动，下载地址仍指向测试服务器：已安装时不应发出任何请求。
    let second = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));
    let status = second.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    assert_eq!(server.requests().len(), requests_after_install);
    let events = harness.events.lock().unwrap().clone();
    assert!(
        events
            .iter()
            .all(|s| matches!(s, FfmpegStatus::Ready { .. })),
        "{events:?}"
    );
    assert_eq!(harness.probe.calls(), 1);
}

#[tokio::test]
async fn retry_when_ready_stays_ready() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));
    assert!(manager.ensure().await.is_ready());
    harness.events.lock().unwrap().clear();

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    assert!(harness.events.lock().unwrap().is_empty());
    assert_eq!(manager.ffmpeg_path(), Some(harness.installed()));
}

#[tokio::test]
async fn verified_record_is_invalidated_when_file_changes() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));
    assert!(manager.ensure().await.is_ready());
    assert!(
        harness
            .root
            .join("9.0.2-9.0.1")
            .join("verified.json")
            .exists()
    );

    // 同样大小、不同内容，并让修改时间变化。
    let mut tampered = BINARY.to_vec();
    tampered[0] ^= 0xff;
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(harness.installed(), &tampered).unwrap();
    let component = component_for(&tgz, vec![server.url("/pkg.tgz")]);
    let binary = harness.installed();
    let record = harness.root.join("9.0.2-9.0.1").join("verified.json");
    assert_eq!(
        install::check_installed(
            &binary,
            &record,
            &component.package,
            install::Versions {
                ffmpeg: "9.0.2",
                package: "9.0.1",
            },
            false,
        ),
        Ok(false)
    );

    // 再次检查会重新下载并替换被改动的文件。
    let status = manager.ensure().await;
    assert!(status.is_ready(), "{status:?}");
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
}

#[tokio::test]
async fn failed_install_keeps_existing_files() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    // 旧版本目录，以及当前版本路径上已有的文件。
    let old_dir = harness.root.join("9.0.1-9.0.0");
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(old_dir.join("ffmpeg"), b"old ffmpeg").unwrap();
    std::fs::create_dir_all(harness.installed().parent().unwrap()).unwrap();
    std::fs::write(harness.installed(), b"previous file").unwrap();

    let mut component = component_for(&tgz, vec![server.url("/pkg.tgz")]);
    component.package.binary_sha256 = "a".repeat(64);
    let manager = harness.manager(component);
    let status = manager.ensure().await;

    assert!(matches!(status, FfmpegStatus::Failed { .. }), "{status:?}");
    assert_eq!(
        std::fs::read(old_dir.join("ffmpeg")).unwrap(),
        b"old ffmpeg"
    );
    assert_eq!(
        std::fs::read(harness.installed()).unwrap(),
        b"previous file"
    );
    assert_eq!(harness.probe.calls(), 0);
}

#[tokio::test]
async fn concurrent_ensure_runs_a_single_install() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let (a, b) = tokio::join!(manager.ensure(), manager.ensure());

    assert!(a.is_ready() || b.is_ready(), "{a:?} {b:?}");
    assert_eq!(server.requests().len(), 1);
    assert_eq!(harness.probe.calls(), 1);
    // 运行结束后可以再次进入（重试入口）。
    assert!(manager.ensure().await.is_ready());
}

#[tokio::test]
async fn unsupported_platform_fails_without_retry() {
    let harness = Harness::new();
    let events = Arc::clone(&harness.events);
    let manager = FfmpegManager::new(
        ManagerConfig {
            root: harness.root.clone(),
            component: Err("当前平台（test）没有可用的 FFmpeg 组件，暂不支持自动安装".into()),
            download: DownloadOptions::default(),
            probe: harness.probe.probe(),
        },
        Box::new(move |status: &FfmpegStatus| events.lock().unwrap().push(status.clone())),
    );

    let status = manager.ensure().await;

    assert_eq!(
        status,
        FfmpegStatus::Failed {
            reason: "当前平台（test）没有可用的 FFmpeg 组件，暂不支持自动安装".into(),
            retryable: false,
        }
    );
}

#[tokio::test]
async fn resume_restarts_when_server_ignores_range() {
    let tgz = sample_tgz();
    let cut = tgz.len() / 3;
    let server = TestServer::start(vec![(
        "/pkg.tgz",
        Route::Script {
            body: tgz.clone(),
            steps: vec![Step::Cut(cut), Step::IgnoreRange],
        },
    )])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(requests[1].range.is_some());
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
}

#[tokio::test]
async fn resume_restarts_after_416() {
    let tgz = sample_tgz();
    let cut = tgz.len() / 3;
    let server = TestServer::start(vec![(
        "/pkg.tgz",
        Route::Script {
            body: tgz.clone(),
            steps: vec![Step::Cut(cut), Step::Status(416), Step::Serve],
        },
    )])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert!(requests[1].range.is_some());
    assert_eq!(requests[2].range, None);
}

#[tokio::test]
async fn resume_rejects_wrong_content_range_start() {
    let tgz = sample_tgz();
    let cut = tgz.len() / 3;
    let server = TestServer::start(vec![(
        "/pkg.tgz",
        Route::Script {
            body: tgz.clone(),
            steps: vec![Step::Cut(cut), Step::WrongStart, Step::Serve],
        },
    )])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    let expected = format!("bytes={cut}-");
    assert_eq!(requests[1].range.as_deref(), Some(expected.as_str()));
    assert_eq!(requests[2].range.as_deref(), Some(expected.as_str()));
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
}

#[tokio::test]
async fn wrong_content_length_switches_source_early() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![
        (
            "/mirror.tgz",
            Route::Script {
                body: tgz.clone(),
                steps: vec![Step::ShortLength],
            },
        ),
        ("/backup.tgz", Route::File(tgz.clone())),
    ])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(
        &tgz,
        vec![server.url("/mirror.tgz"), server.url("/backup.tgz")],
    ));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, ["/mirror.tgz", "/mirror.tgz", "/backup.tgz"]);
}

#[tokio::test]
async fn tarball_mismatch_redownloads_from_next_source() {
    let tgz = sample_tgz();
    let mut corrupted = tgz.clone();
    let middle = corrupted.len() / 2;
    corrupted[middle] ^= 0xff;
    let server = TestServer::start(vec![
        ("/mirror.tgz", Route::File(corrupted)),
        ("/backup.tgz", Route::File(tgz.clone())),
    ])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(
        &tgz,
        vec![server.url("/mirror.tgz"), server.url("/backup.tgz")],
    ));

    let status = manager.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let requests = server.requests();
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["/mirror.tgz", "/backup.tgz"]);
    assert_eq!(requests[1].range, None);
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
    assert_eq!(harness.probe.calls(), 1);
}

#[tokio::test]
async fn tarball_mismatch_on_every_source_fails() {
    let tgz = sample_tgz();
    let mut corrupted = tgz.clone();
    corrupted[10] ^= 0xff;
    let server = TestServer::start(vec![
        ("/a.tgz", Route::File(corrupted.clone())),
        ("/b.tgz", Route::File(corrupted)),
    ])
    .await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(
        &tgz,
        vec![server.url("/a.tgz"), server.url("/b.tgz")],
    ));

    let status = manager.ensure().await;

    assert!(
        matches!(&status, FfmpegStatus::Failed { reason, .. } if reason.contains("校验不一致")),
        "{status:?}"
    );
    assert_eq!(server.requests().len(), 2);
    assert_eq!(harness.probe.calls(), 0);
    assert!(
        harness.temp_files().is_empty(),
        "{:?}",
        harness.temp_files()
    );
}

#[tokio::test]
async fn local_write_failure_reports_io_error_without_switching_source() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    // 部分文件的位置被一个目录占住，写入必然失败。
    std::fs::create_dir_all(harness.root.join(".tmp").join("ffmpeg-test-9.0.1.tgz.part")).unwrap();
    let manager = harness.manager(component_for(
        &tgz,
        vec![server.url("/pkg.tgz"), server.url("/pkg.tgz")],
    ));

    let status = manager.ensure().await;

    match status {
        FfmpegStatus::Failed { reason, .. } => {
            assert!(reason.starts_with("写入下载的临时文件失败"), "{reason}");
            assert!(!reason.contains("无法连接"), "{reason}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn failed_install_falls_back_to_older_verified_version() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let old = with_versions(
        component_for(&tgz, vec![server.url("/pkg.tgz")]),
        "9.0.1",
        "9.0.0",
    );
    assert!(harness.manager(old).ensure().await.is_ready());
    let old_path = harness.root.join("9.0.1-9.0.0").join("ffmpeg");

    let mut current = component_for(&tgz, vec![server.url("/pkg.tgz")]);
    current.package.binary_sha256 = "b".repeat(64);
    let manager = harness.manager(current.clone());
    let status = manager.ensure().await;

    match &status {
        FfmpegStatus::Ready {
            path,
            ffmpeg_version,
            package_version,
            outdated,
            install_error,
        } => {
            assert_eq!(path, &old_path);
            assert_eq!(ffmpeg_version, "9.0.1");
            assert_eq!(package_version, "9.0.0");
            assert!(*outdated);
            let error = install_error.as_deref().unwrap();
            assert!(error.contains("校验不一致"), "{error}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(manager.ffmpeg_path(), Some(old_path.clone()));
    assert!(!harness.installed().exists());

    // 旧版本被改动后不再当作可用。
    let mut tampered = BINARY.to_vec();
    tampered[1] ^= 0xff;
    std::fs::write(&old_path, &tampered).unwrap();
    let manager = harness.manager(current);
    let status = manager.ensure().await;
    assert!(matches!(status, FfmpegStatus::Failed { .. }), "{status:?}");
    assert_eq!(manager.ffmpeg_path(), None);
}

#[tokio::test]
async fn two_instances_sharing_a_data_dir_install_once() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    // 两个管理器各有自己的进程内锁，只能靠文件锁互斥，相当于两个进程。
    let a = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));
    let b = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let (first, second) = tokio::join!(a.ensure(), b.ensure());

    assert!(first.is_ready(), "{first:?}");
    assert!(second.is_ready(), "{second:?}");
    assert_eq!(server.requests().len(), 1);
    assert_eq!(harness.probe.calls(), 1);
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
    assert!(
        harness.temp_files().is_empty(),
        "{:?}",
        harness.temp_files()
    );
}

#[tokio::test]
async fn removes_partial_files_of_other_packages() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let temp = harness.root.join(".tmp");
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::write(temp.join("ffmpeg-test-9.0.0.tgz.part"), b"old").unwrap();
    std::fs::write(temp.join("ffmpeg-test-9.0.0.tgz.part.src"), b"http://old").unwrap();
    std::fs::write(temp.join("keep.txt"), b"x").unwrap();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    assert!(manager.ensure().await.is_ready());

    assert!(!temp.join("ffmpeg-test-9.0.0.tgz.part").exists());
    assert!(!temp.join("ffmpeg-test-9.0.0.tgz.part.src").exists());
    assert!(temp.join("keep.txt").exists());
}

#[test]
fn extract_ignores_other_entries_and_checks_size() {
    let dir = tempfile::tempdir().unwrap();
    let tgz = make_tgz(&[
        ("package/bin/ffmpeg.bak", &b"nope"[..]),
        ("package/bin/ffmpeg", BINARY),
    ]);
    let tarball = dir.path().join("pkg.tgz");
    std::fs::write(&tarball, &tgz).unwrap();
    let mut package = component_for(&tgz, vec![]).package;
    let dest = dir.path().join("out");

    install::extract_binary(&tarball, &package, &dest).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), BINARY);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    package.binary_size -= 1;
    let err = install::extract_binary(&tarball, &package, &dest).unwrap_err();
    assert_eq!(err, InstallError::BinaryMismatch);
    assert!(!dest.exists());
}

/// 改动已安装文件的内容，保持大小和修改时间不变，校验缓存因此仍然匹配。
fn tamper_keeping_metadata(path: &Path) {
    let modified = std::fs::metadata(path).unwrap().modified().unwrap();
    let mut tampered = BINARY.to_vec();
    tampered[0] ^= 0xff;
    std::fs::write(path, &tampered).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(modified).unwrap();
}

/// 当前版本的校验记录。
fn record_path(harness: &Harness) -> PathBuf {
    harness.root.join("9.0.2-9.0.1").join("verified.json")
}

/// 去掉校验记录里的版本号。大小、修改时间和哈希不变，普通检查仍然命中缓存；
/// 只有重新计算哈希后才会把整条记录（连同版本号）重新写回。
fn strip_record_versions(harness: &Harness) {
    let path = record_path(harness);
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let object = record.as_object_mut().unwrap();
    assert!(object.remove("ffmpegVersion").is_some());
    object.remove("packageVersion");
    std::fs::write(&path, record.to_string()).unwrap();
}

fn record_has_versions(harness: &Harness) -> bool {
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(record_path(harness)).unwrap()).unwrap();
    record.get("ffmpegVersion").is_some()
}

#[tokio::test]
async fn reverify_shows_verifying_then_ready_without_network() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));
    assert!(manager.ensure().await.is_ready());
    let requests_after_install = server.requests().len();
    strip_record_versions(&harness);

    // 对照：普通检查信任缓存，不重新计算哈希，记录保持原样。
    assert!(manager.ensure().await.is_ready());
    assert!(!record_has_versions(&harness));
    harness.events.lock().unwrap().clear();

    let status = manager.reverify().await;

    assert!(status.is_ready(), "{status:?}");
    // 记录被重新写入，说明这次没有走缓存，而是重新计算了哈希。
    assert!(record_has_versions(&harness));
    let events = harness.events.lock().unwrap().clone();
    assert!(
        matches!(
            events.as_slice(),
            [
                FfmpegStatus::Verifying,
                FfmpegStatus::Ready {
                    outdated: false,
                    ..
                }
            ]
        ),
        "{events:?}"
    );
    assert_eq!(server.requests().len(), requests_after_install);
    assert_eq!(manager.ffmpeg_path(), Some(harness.installed()));
}

/// 以独占方式打开文件（Windows）或去掉读权限（Unix），让哈希计算读不出来。
/// 返回值在作用域内保持占用；以 root 运行等读权限无效的环境返回 `None`。
fn make_unreadable(path: &Path) -> Option<Box<dyn std::any::Any>> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap();
        Some(Box::new(file))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(path).is_ok() {
            return None;
        }
        Some(Box::new(()))
    }
}

#[tokio::test]
async fn reverify_keeps_current_version_when_file_cannot_be_read() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    // 旧版本也在：读不出当前版本时不应该退回它。
    let old = with_versions(
        component_for(&tgz, vec![server.url("/pkg.tgz")]),
        "9.0.1",
        "9.0.0",
    );
    assert!(harness.manager(old).ensure().await.is_ready());
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));
    assert!(manager.ensure().await.is_ready());
    let requests_before = server.requests().len();
    let Some(guard) = make_unreadable(&harness.installed()) else {
        return;
    };
    assert!(std::fs::read(harness.installed()).is_err());
    harness.events.lock().unwrap().clear();

    let status = manager.reverify().await;
    drop(guard);

    // 一直停在当前版本：没有切到旧版本，也没有“未安装”或下载。
    let events = harness.events.lock().unwrap().clone();
    assert!(
        matches!(
            events.as_slice(),
            [
                FfmpegStatus::Verifying,
                FfmpegStatus::Ready {
                    outdated: false,
                    ..
                }
            ]
        ),
        "{events:?}"
    );

    match &status {
        FfmpegStatus::Ready {
            path,
            outdated,
            install_error,
            ..
        } => {
            assert_eq!(path, &harness.installed());
            assert!(!*outdated);
            assert_eq!(install_error, &None);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(manager.ffmpeg_path(), Some(harness.installed()));
    assert_eq!(server.requests().len(), requests_before);
    assert!(record_path(&harness).exists());
}

#[tokio::test]
async fn reverify_ignores_cache_and_reinstalls_tampered_file() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));
    assert!(manager.ensure().await.is_ready());
    tamper_keeping_metadata(&harness.installed());

    // 普通检查信任缓存，发现不了改动。
    assert!(manager.ensure().await.is_ready());
    assert_ne!(std::fs::read(harness.installed()).unwrap(), BINARY);
    let requests_before = server.requests().len();

    let status = manager.reverify().await;

    assert!(
        matches!(
            status,
            FfmpegStatus::Ready {
                outdated: false,
                ..
            }
        ),
        "{status:?}"
    );
    assert_eq!(std::fs::read(harness.installed()).unwrap(), BINARY);
    assert!(server.requests().len() > requests_before);
}

#[tokio::test]
async fn reverify_failure_keeps_older_version_usable() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![
        ("/pkg.tgz", Route::File(tgz.clone())),
        ("/gone.tgz", Route::Status(404)),
    ])
    .await;
    let harness = Harness::new();
    let old = with_versions(
        component_for(&tgz, vec![server.url("/pkg.tgz")]),
        "9.0.1",
        "9.0.0",
    );
    assert!(harness.manager(old).ensure().await.is_ready());
    let old_path = harness.root.join("9.0.1-9.0.0").join("ffmpeg");
    assert!(
        harness
            .manager(component_for(&tgz, vec![server.url("/pkg.tgz")]))
            .ensure()
            .await
            .is_ready()
    );
    tamper_keeping_metadata(&harness.installed());

    // 下载源不可用时重新校验失败，退回旧版本。
    let manager = harness.manager(component_for(&tgz, vec![server.url("/gone.tgz")]));
    let status = manager.reverify().await;

    match &status {
        FfmpegStatus::Ready {
            path,
            outdated,
            install_error,
            ..
        } => {
            assert_eq!(path, &old_path);
            assert!(*outdated);
            assert!(install_error.is_some());
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(manager.ffmpeg_path(), Some(old_path));
}

#[tokio::test]
async fn reverify_while_install_runs_returns_current_status() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = Arc::new(harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")])));

    let (installed, reverified) = tokio::join!(manager.ensure(), manager.reverify());

    // 安装先拿到锁，重新校验不会再启动一次，返回当时的“检查中”。
    assert!(installed.is_ready(), "{installed:?}");
    assert_eq!(reverified, FfmpegStatus::Checking);
    assert_eq!(harness.probe.calls(), 1);
    // 只下载、只校验了一次：安装途中出现一次“校验中”，之后重新校验没有再跑一轮
    // （再跑会在“可用”之后再出现一次“校验中”）。
    assert_eq!(server.requests().len(), 1);
    let events = harness.events.lock().unwrap().clone();
    let verifying = events
        .iter()
        .filter(|status| **status == FfmpegStatus::Verifying)
        .count();
    assert_eq!(verifying, 1, "{events:?}");
    assert!(
        events.last().is_some_and(FfmpegStatus::is_ready),
        "{events:?}"
    );
}

#[tokio::test]
async fn check_local_reports_not_installed_without_network() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let manager = harness.manager(component_for(&tgz, vec![server.url("/pkg.tgz")]));

    let status = manager.check_local().await;

    assert_eq!(status, FfmpegStatus::NotInstalled);
    assert!(
        server.requests().is_empty(),
        "check_local must not download"
    );
    assert!(!harness.installed().exists());
}

#[tokio::test]
async fn check_local_finds_current_and_older_versions() {
    let tgz = sample_tgz();
    let server = TestServer::start(vec![("/pkg.tgz", Route::File(tgz.clone()))]).await;
    let harness = Harness::new();
    let old = with_versions(
        component_for(&tgz, vec![server.url("/pkg.tgz")]),
        "9.0.1",
        "9.0.0",
    );
    assert!(harness.manager(old).ensure().await.is_ready());
    let requests = server.requests().len();

    // 只有旧版本：报告旧版本可用（outdated），不下载新版本。
    let current = component_for(&tgz, vec![server.url("/pkg.tgz")]);
    let status = harness.manager(current.clone()).check_local().await;
    assert!(
        matches!(&status, FfmpegStatus::Ready { outdated: true, ffmpeg_version, .. } if ffmpeg_version == "9.0.1"),
        "{status:?}"
    );
    assert_eq!(server.requests().len(), requests);

    // 装好当前版本后报告当前版本。
    assert!(harness.manager(current.clone()).ensure().await.is_ready());
    let status = harness.manager(current).check_local().await;
    assert!(
        matches!(&status, FfmpegStatus::Ready { outdated: false, path, .. } if path == &harness.installed()),
        "{status:?}"
    );
}
