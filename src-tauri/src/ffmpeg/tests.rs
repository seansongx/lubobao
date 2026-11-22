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

    let (body, cut) = match routes.get(&path) {
        None => return respond_status(&mut stream, 404).await,
        Some(Route::Status(code)) => return respond_status(&mut stream, *code).await,
        Some(Route::File(body)) => (body, None),
        Some(Route::CutOnce { body, cut }) => (body, (hit == 1).then_some(*cut)),
        Some(Route::CutThenFail { body, cut }) => {
            if hit > 1 {
                return respond_status(&mut stream, 500).await;
            }
            (body, Some(*cut))
        }
    };

    let start = range
        .as_deref()
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|r| r.parse::<usize>().ok());
    let head = match start {
        Some(start) if start >= body.len() => return respond_status(&mut stream, 416).await,
        Some(start) => format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
            body.len() - start,
            start,
            body.len() - 1,
            body.len()
        ),
        None => format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            body.len()
        ),
    };
    let start = start.unwrap_or(0);
    let end = match cut {
        Some(cut) => (start + cut).min(body.len()),
        None => body.len(),
    };
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
            Route::CutThenFail {
                body: tgz.clone(),
                cut,
            },
        ),
        ("/good.tgz", Route::File(tgz.clone())),
    ])
    .await;
    let harness = Harness::new();

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

    let second = harness.manager(component_for(&tgz, vec![server.url("/good.tgz")]));
    let status = second.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    let good: Vec<Request> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/good.tgz")
        .collect();
    assert_eq!(good.len(), 1);
    assert_eq!(
        good[0].range.as_deref(),
        Some(format!("bytes={cut}-").as_str())
    );
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

    // 第二次启动：下载地址无效，也不应发出任何请求。
    let offline = harness.manager(component_for(
        &tgz,
        vec!["http://127.0.0.1:9/unreachable.tgz".into()],
    ));
    let status = offline.ensure().await;

    assert!(status.is_ready(), "{status:?}");
    assert_eq!(server.requests().len(), requests_after_install);
    assert!(
        !harness
            .events
            .lock()
            .unwrap()
            .iter()
            .skip_while(|s| !s.is_ready())
            .any(|s| matches!(s, FfmpegStatus::Downloading { .. })),
        "offline check must not download"
    );
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
    assert!(!install::check_installed(
        &binary,
        &record,
        &component.package
    ));

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
