//! 录制引擎的本地样本测试。
//!
//! 用 ffmpeg 的 lavfi `testsrc` 和 `sine` 生成一段带音视频的 FLV，再起一个只监听
//! `127.0.0.1` 的 HTTP 服务，按 FLV 时间戳实时吐出数据来模拟直播。需要真实 ffmpeg 的测试
//! 读取环境变量 `LUBOBAO_TEST_FFMPEG`，没设置时打印提示并跳过。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};

use super::*;

const SAMPLE_SECS: u32 = 30;
const TEST_USER_AGENT: &str = "LuBoBaoTest/1.0";
const TEST_REFERER: &str = "https://live.example.com/";

fn test_ffmpeg() -> Option<PathBuf> {
    match std::env::var_os("LUBOBAO_TEST_FFMPEG") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => {
            eprintln!("跳过：未设置 LUBOBAO_TEST_FFMPEG（ffmpeg 可执行文件路径）");
            None
        }
    }
}

/// 生成测试用 FLV：320×240、25 fps、每秒一个关键帧的 H.264，加 44.1 kHz 正弦波 AAC。
fn make_sample(ffmpeg: &Path, dir: &Path) -> PathBuf {
    let path = dir.join("sample.flv");
    let output = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=size=320x240:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100"])
        .args(["-t", &SAMPLE_SECS.to_string()])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-g", "25",
        ])
        .args(["-c:a", "aac", "-b:a", "96k", "-f", "flv"])
        .arg(&path)
        .output()
        .expect("run ffmpeg to create sample");
    assert!(
        output.status.success(),
        "sample generation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}

/// 解码整个文件，确认没有任何错误输出。
fn assert_playable(ffmpeg: &Path, file: &Path) {
    let output = std::process::Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg to decode");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.trim().is_empty(),
        "{} is not cleanly playable: {stderr}",
        file.display()
    );
}

/// 媒体时长（秒），从解码输出的最后一个 `time=` 读取。
fn media_duration(ffmpeg: &Path, file: &Path) -> f64 {
    let output = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg to measure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .split(['\r', '\n'])
        .filter_map(progress::parse_progress_line)
        .filter_map(|progress| progress.duration_secs)
        .next_back()
        .unwrap_or(0.0)
}

/// FLV 文件头（含 PreviousTagSize0）和按时间戳排列的各个 tag。
struct FlvSample {
    header: Vec<u8>,
    tags: Vec<(u32, Vec<u8>)>,
}

fn parse_flv(bytes: &[u8]) -> FlvSample {
    assert_eq!(&bytes[..3], b"FLV");
    let header_len = u32::from_be_bytes(bytes[5..9].try_into().unwrap()) as usize + 4;
    let mut tags = Vec::new();
    let mut offset = header_len;
    while offset + 11 <= bytes.len() {
        let size = u32::from_be_bytes([0, bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
            as usize;
        let timestamp = u32::from_be_bytes([
            bytes[offset + 7],
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
        ]);
        let end = offset + 11 + size + 4;
        if end > bytes.len() {
            break;
        }
        tags.push((timestamp, bytes[offset..end].to_vec()));
        offset = end;
    }
    FlvSample {
        header: bytes[..header_len].to_vec(),
        tags,
    }
}

#[derive(Clone, Copy)]
enum ServeMode {
    /// 一直发送到样本结束或客户端断开。
    Live,
    /// 发送到媒体时间戳达到指定秒数后，在 tag 边界正常关闭连接，并不再接受新连接。
    CutAfter(u32),
    /// 发送到指定秒数后保持连接但不再发送数据。
    StallAfter(u32),
}

struct LiveServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_server(sample: &Path, mode: ServeMode) -> LiveServer {
    let flv = Arc::new(parse_flv(&std::fs::read(sample).unwrap()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/live/test.flv", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match socket.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&request).into_owned());
            let response =
                b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nConnection: close\r\n\r\n";
            if socket.write_all(response).await.is_err()
                || socket.write_all(&flv.header).await.is_err()
            {
                continue;
            }
            let limit_ms = match mode {
                ServeMode::Live => u32::MAX,
                ServeMode::CutAfter(secs) | ServeMode::StallAfter(secs) => secs * 1000,
            };
            let begin = Instant::now();
            let mut client_gone = false;
            for (timestamp, tag) in &flv.tags {
                if *timestamp >= limit_ms {
                    break;
                }
                // 和 CDN 一样先给约 1 秒的缓存，之后按实时速率发送。
                let due = Duration::from_millis(u64::from(timestamp.saturating_sub(1000)));
                let elapsed = begin.elapsed();
                if due > elapsed {
                    sleep(due - elapsed).await;
                }
                if socket.write_all(tag).await.is_err() {
                    client_gone = true;
                    break;
                }
            }
            match mode {
                ServeMode::CutAfter(_) if !client_gone => {
                    let _ = socket.shutdown().await;
                    drop(socket);
                    // 不再接受新连接：后续连接会被拒绝。
                    return;
                }
                ServeMode::StallAfter(_) => {
                    sleep(Duration::from_secs(600)).await;
                }
                _ => {}
            }
        }
    });
    LiveServer {
        url,
        requests,
        task,
    }
}

fn test_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("User-Agent".to_string(), TEST_USER_AGENT.to_string()),
        ("Referer".to_string(), TEST_REFERER.to_string()),
    ])
}

fn request(ffmpeg: &Path, url: &str, root: &Path, options: RecordOptions) -> RecordingRequest {
    RecordingRequest {
        ffmpeg_path: ffmpeg.to_path_buf(),
        source_url: url.to_string(),
        source_format: SourceFormat::Flv,
        headers: test_headers(),
        // 中文和空格路径。
        output_dir: root.join("录像 测试"),
        anchor_name: "测试主播：小明".to_string(),
        platform: "抖音".to_string(),
        options,
    }
}

fn unsegmented() -> RecordOptions {
    RecordOptions {
        segment_enabled: false,
        disk_threshold_bytes: 0,
        ..RecordOptions::default()
    }
}

/// 收集所有事件，便于检查顺序。
fn recording_listener() -> (EventListener, Arc<Mutex<Vec<RecorderEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let listener: EventListener = Arc::new(move |event: &RecorderEvent| {
        sink.lock().unwrap().push(event.clone());
    });
    (listener, events)
}

fn quiet_listener() -> EventListener {
    Arc::new(|_: &RecorderEvent| {})
}

async fn wait_for_duration(handle: &RecordingHandle, secs: f64) {
    timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = handle.snapshot();
            assert!(
                snapshot.outcome.is_none(),
                "recording ended early: {:?}",
                snapshot.outcome
            );
            if snapshot
                .progress
                .and_then(|progress| progress.duration_secs)
                .is_some_and(|duration| duration >= secs)
            {
                return;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("recording did not make progress");
}

async fn wait_outcome(handle: &RecordingHandle, limit_secs: u64) -> RecordingOutcome {
    timeout(Duration::from_secs(limit_secs), handle.wait())
        .await
        .expect("recording did not finish in time")
}

#[tokio::test]
async fn start_writes_recording_file_with_stream_headers() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::Live).await;

    let handle = start(
        request(&ffmpeg, &server.url, temp.path(), unsegmented()),
        quiet_listener(),
    )
    .await;
    wait_for_duration(&handle, 3.0).await;

    let snapshot = handle.snapshot();
    assert_eq!(snapshot.state, RecorderState::Recording);
    let output = snapshot.output_path.unwrap();
    let expected_dir = temp
        .path()
        .join("录像 测试")
        .join("抖音")
        .join("测试主播_小明");
    assert_eq!(output.parent().unwrap(), expected_dir);
    let name = output.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("测试主播_小明_") && name.ends_with(".ts"),
        "{name}"
    );
    let size = std::fs::metadata(&output)
        .expect("file exists while recording")
        .len();
    assert!(size > 0, "file is empty while recording");
    let progress = handle.snapshot().progress.unwrap();
    assert!(progress.size_bytes.unwrap() > 0);
    assert!(progress.bitrate_kbps.unwrap() > 0.0);

    let request_text = server.requests.lock().unwrap()[0].clone();
    assert!(
        request_text.contains(&format!("User-Agent: {TEST_USER_AGENT}")),
        "{request_text}"
    );
    assert!(
        request_text.contains(&format!("Referer: {TEST_REFERER}")),
        "{request_text}"
    );

    handle.stop_and_wait().await;
}

#[tokio::test]
async fn manual_stop_leaves_playable_file() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::Live).await;
    let (listener, events) = recording_listener();

    let handle = start(
        request(&ffmpeg, &server.url, temp.path(), unsegmented()),
        listener,
    )
    .await;
    wait_for_duration(&handle, 4.0).await;
    let stop_requested = Instant::now();
    let outcome = handle.stop_and_wait().await;

    assert!(
        stop_requested.elapsed() < Duration::from_secs(5),
        "graceful stop was slow"
    );
    assert_eq!(outcome.reason, EndReason::UserStopped);
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.files.len(), 1);
    assert!(outcome.bytes_written > 0);
    assert!(outcome.duration_secs.unwrap() >= 4.0);
    assert_playable(&ffmpeg, &outcome.files[0]);
    assert!(media_duration(&ffmpeg, &outcome.files[0]) >= 4.0);
    assert_eq!(handle.snapshot().state, RecorderState::Finished);

    let events = events.lock().unwrap();
    assert!(matches!(
        events.first(),
        Some(RecorderEvent::Started { .. })
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RecorderEvent::Progress { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RecorderEvent::Stopping {
            cause: StopCause::User
        }
    )));
    assert!(matches!(events.last(), Some(RecorderEvent::Finished { outcome: o }) if o == &outcome));
    let json = serde_json::to_value(events.last().unwrap()).unwrap();
    assert_eq!(json["type"], "finished");
    assert_eq!(json["outcome"]["reason"], "userStopped");
}

#[tokio::test]
async fn server_disconnect_reports_stream_end_with_playable_file() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::CutAfter(5)).await;

    let handle = start(
        request(&ffmpeg, &server.url, temp.path(), unsegmented()),
        quiet_listener(),
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;

    assert_eq!(outcome.reason, EndReason::StreamEnded, "{outcome:?}");
    assert_eq!(outcome.files.len(), 1);
    assert_playable(&ffmpeg, &outcome.files[0]);
    let duration = media_duration(&ffmpeg, &outcome.files[0]);
    assert!((4.0..=5.5).contains(&duration), "duration {duration}");
}

#[tokio::test]
async fn unreachable_source_reports_network_error() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    // 先占一个端口再释放，保证没有服务在监听。
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/live/test.flv");

    let begin = Instant::now();
    let handle = start(
        request(&ffmpeg, &url, temp.path(), unsegmented()),
        quiet_listener(),
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;

    assert_eq!(outcome.reason, EndReason::NetworkError, "{outcome:?}");
    assert!(outcome.files.is_empty());
    assert!(outcome.message.is_some());
    assert!(begin.elapsed() < Duration::from_secs(20));
}

#[tokio::test]
async fn segmented_recording_produces_playable_parts() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::CutAfter(9)).await;
    let options = RecordOptions {
        segment_enabled: true,
        segment_secs: 3,
        disk_threshold_bytes: 0,
        ..RecordOptions::default()
    };

    let handle = start(
        request(&ffmpeg, &server.url, temp.path(), options),
        quiet_listener(),
    )
    .await;
    let output_path = handle.snapshot().output_path.unwrap();
    assert!(output_path.to_string_lossy().ends_with("_%03d.ts"));
    wait_for_duration(&handle, 5.0).await;
    // 分段时 ffmpeg 不报告大小和码率，引擎用分段文件补全。
    let progress = handle.snapshot().progress.unwrap();
    assert!(progress.size_bytes.unwrap() > 0, "{progress:?}");
    assert!(progress.bitrate_kbps.unwrap() > 0.0, "{progress:?}");
    let outcome = wait_outcome(&handle, 40).await;

    assert_eq!(outcome.reason, EndReason::StreamEnded, "{outcome:?}");
    assert!(outcome.files.len() >= 3, "files: {:?}", outcome.files);
    for (index, file) in outcome.files.iter().enumerate() {
        let name = file.file_name().unwrap().to_str().unwrap();
        assert!(name.ends_with(&format!("_{index:03}.ts")), "{name}");
        assert_playable(&ffmpeg, file);
        let duration = media_duration(&ffmpeg, file);
        assert!(duration > 0.5 && duration < 4.5, "{name}: {duration}");
    }
}

#[tokio::test]
async fn low_disk_space_during_recording_stops_gracefully() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::Live).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    // 录制前和录制中的前两次检查空间充足，之后不足。
    let probe: SpaceProbe = Arc::new(move |_: &Path| {
        let call = counter.fetch_add(1, Ordering::SeqCst);
        Ok(if call < 3 { 10 << 30 } else { 1 << 30 })
    });
    let options = RecordOptions {
        segment_enabled: false,
        disk_check_interval_secs: 1,
        ..RecordOptions::default()
    };

    let handle = start_with_probe(
        request(&ffmpeg, &server.url, temp.path(), options),
        quiet_listener(),
        probe,
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;

    assert_eq!(outcome.reason, EndReason::DiskFull, "{outcome:?}");
    assert_eq!(outcome.files.len(), 1);
    assert_playable(&ffmpeg, &outcome.files[0]);
}

#[tokio::test]
async fn stalled_source_is_stopped_by_watchdog() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::StallAfter(3)).await;
    let options = RecordOptions {
        stall_timeout_secs: 4,
        stop_timeout_secs: 3,
        ..unsegmented()
    };

    let handle = start(
        request(&ffmpeg, &server.url, temp.path(), options),
        quiet_listener(),
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;

    assert_eq!(outcome.reason, EndReason::NetworkError, "{outcome:?}");
    assert_eq!(outcome.files.len(), 1);
    assert_playable(&ffmpeg, &outcome.files[0]);
}

// 以下测试不需要 ffmpeg。

#[tokio::test]
async fn low_disk_space_before_start_does_not_launch_ffmpeg() {
    let temp = tempfile::tempdir().unwrap();
    let probe: SpaceProbe = Arc::new(|_: &Path| Ok(1 << 30));
    let (listener, events) = recording_listener();
    let missing = temp.path().join("no-ffmpeg.exe");
    let handle = start_with_probe(
        request(
            &missing,
            "http://127.0.0.1:1/x.flv",
            temp.path(),
            RecordOptions::default(),
        ),
        listener,
        probe,
    )
    .await;

    assert!(handle.is_finished());
    let outcome = handle.wait().await;
    assert_eq!(outcome.reason, EndReason::DiskFull);
    assert!(outcome.message.unwrap().contains("1.00 GB"));
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], RecorderEvent::Finished { .. }));
}

#[tokio::test]
async fn space_probe_errors_do_not_block_recording() {
    let temp = tempfile::tempdir().unwrap();
    let probe: SpaceProbe = Arc::new(|_: &Path| Err(io::Error::other("unsupported")));
    let missing = temp.path().join("no-ffmpeg.exe");
    let handle = start_with_probe(
        request(
            &missing,
            "http://127.0.0.1:1/x.flv",
            temp.path(),
            RecordOptions::default(),
        ),
        quiet_listener(),
        probe,
    )
    .await;
    // 磁盘检查被跳过，失败在启动 ffmpeg 这一步。
    assert_eq!(handle.wait().await.reason, EndReason::FfmpegFailed);
}

#[tokio::test]
async fn missing_ffmpeg_reports_launch_failure() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("no-ffmpeg.exe");
    let handle = start(
        request(
            &missing,
            "http://127.0.0.1:1/x.flv",
            temp.path(),
            unsegmented(),
        ),
        quiet_listener(),
    )
    .await;
    let outcome = handle.wait().await;
    assert_eq!(outcome.reason, EndReason::FfmpegFailed);
    assert!(outcome.message.unwrap().contains("无法启动 ffmpeg"));
    // 目录已经建好，但没有录像文件。
    assert!(outcome.output_dir.unwrap().is_dir());
    assert!(outcome.files.is_empty());
}

#[tokio::test]
async fn socks_proxy_is_rejected_before_launch() {
    let temp = tempfile::tempdir().unwrap();
    let options = RecordOptions {
        proxy: Some("socks5://127.0.0.1:7891".to_string()),
        ..unsegmented()
    };
    let missing = temp.path().join("no-ffmpeg.exe");
    let handle = start(
        request(&missing, "http://127.0.0.1:1/x.flv", temp.path(), options),
        quiet_listener(),
    )
    .await;
    let outcome = handle.wait().await;
    assert_eq!(outcome.reason, EndReason::FfmpegFailed);
    assert!(outcome.message.unwrap().contains("HTTP 代理"));
}

#[test]
fn default_options_match_streamcap() {
    let options = RecordOptions::default();
    assert_eq!(options.format, OutputFormat::Ts);
    assert!(options.segment_enabled);
    assert_eq!(options.segment_secs, 1800);
    assert_eq!(options.disk_threshold_bytes, 2 * 1024 * 1024 * 1024);
    assert!(options.folder_by_platform && options.folder_by_anchor);
    let parsed: RecordOptions = serde_json::from_str(r#"{"segmentSecs": 600}"#).unwrap();
    assert_eq!(parsed.segment_secs, 600);
    assert!(parsed.segment_enabled);
}

#[test]
fn default_space_probe_reads_real_disk() {
    let free = default_space_probe()(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    assert!(free > 0);
}

/// 真机录制：用抖音适配器解析在播直播间，按默认选项录制后手动停止。
///
/// 运行：`cargo test --manifest-path src-tauri/Cargo.toml live_douyin -- --ignored --nocapture`。
/// 需要 `LUBOBAO_TEST_FFMPEG`；可用 `LUBOBAO_LIVE_URL`（默认 `https://live.douyin.com/78092960160`）、
/// `LUBOBAO_LIVE_SECS`（默认 60）和 `LUBOBAO_LIVE_OUT`（保留录像的目录，默认临时目录）调整。
#[tokio::test]
#[ignore = "需要网络和一个正在直播的抖音直播间"]
async fn live_douyin_recording() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let url = std::env::var("LUBOBAO_LIVE_URL")
        .unwrap_or_else(|_| "https://live.douyin.com/78092960160".to_string());
    let secs: u64 = std::env::var("LUBOBAO_LIVE_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60);
    let temp = tempfile::tempdir().unwrap();
    let root = std::env::var_os("LUBOBAO_LIVE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| temp.path().to_path_buf());

    let adapter = crate::platforms::adapter_for(&url).expect("supported link");
    let info = adapter
        .fetch(&url, &crate::platforms::FetchOptions::default())
        .await
        .expect("fetch live info");
    assert!(
        info.is_live,
        "{} 未开播，换一个在播的直播间",
        info.anchor_name
    );
    let selected = info.selected.as_ref().expect("selected stream");
    let (source_format, source_url) = selected.source_url(true, false);
    println!(
        "主播：{}，画质：{}，源格式：{source_format:?}",
        info.anchor_name, selected.variant.name
    );

    let handle = start(
        RecordingRequest {
            ffmpeg_path: ffmpeg.clone(),
            source_url: source_url.to_string(),
            source_format,
            headers: info.stream_headers.clone(),
            output_dir: root,
            anchor_name: info.anchor_name.clone(),
            platform: info.platform.display_name().to_string(),
            options: RecordOptions::default(),
        },
        quiet_listener(),
    )
    .await;
    let begin = Instant::now();
    while begin.elapsed() < Duration::from_secs(secs) {
        if let Some(outcome) = handle.snapshot().outcome {
            panic!("recording ended early: {outcome:?}");
        }
        sleep(Duration::from_secs(10)).await;
        if let Some(progress) = handle.snapshot().progress {
            println!("进度：{progress:?}");
        }
    }
    let last_progress = handle.snapshot().progress;
    let outcome = handle.stop_and_wait().await;
    println!("结果：{outcome:?}");
    println!("最后进度：{last_progress:?}");

    assert_eq!(outcome.reason, EndReason::UserStopped);
    assert!(!outcome.files.is_empty());
    for file in &outcome.files {
        assert_playable(&ffmpeg, file);
        let size = std::fs::metadata(file).unwrap().len();
        let duration = media_duration(&ffmpeg, file);
        println!(
            "文件：{}，大小 {size} 字节（{:.2} MiB），时长 {duration:.2} 秒，平均码率 {:.0} kbit/s",
            file.display(),
            size as f64 / 1024.0 / 1024.0,
            size as f64 * 8.0 / duration / 1000.0
        );
    }
}
