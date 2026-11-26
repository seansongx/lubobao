//! 录制引擎的本地样本测试。
//!
//! 用 ffmpeg 的 lavfi `testsrc` 和 `sine` 生成一段带音视频的 FLV，再起一个只监听
//! `127.0.0.1` 的 HTTP 服务，按 FLV 时间戳实时吐出数据来模拟直播。需要真实 ffmpeg 的测试
//! 读取环境变量 `LUBOBAO_TEST_FFMPEG`，没设置时打印提示并跳过。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::TimeZone;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};
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
    /// 模拟刚下播的 CDN：返回 200 和一个 HTML 页面。
    Html,
    /// 模拟刚下播的 CDN：返回 200 和空内容。
    Empty,
    /// 返回 404。
    NotFound,
    /// 先把 `/live/test.flv` 重定向到 `/live/moved.flv`，那里按 `Live` 发送。
    Redirect,
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

/// 起一个只监听 127.0.0.1 的服务，每个连接单独处理。
async fn start_server(sample: &Path, mode: ServeMode) -> LiveServer {
    let flv = Arc::new(parse_flv(&std::fs::read(sample).unwrap()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/live/test.flv", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        // JoinSet 随任务一起 drop，未结束的连接也会被中止。
        let mut connections = JoinSet::new();
        let stop_accepting = Arc::new(Notify::new());
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((socket, _)) = accepted else { return };
                    let flv = flv.clone();
                    let seen = seen.clone();
                    let stop_accepting = stop_accepting.clone();
                    connections.spawn(async move {
                        if serve_connection(socket, &flv, mode, &seen).await {
                            stop_accepting.notify_one();
                        }
                    });
                }
                // 关闭监听：后续连接会被拒绝。
                _ = stop_accepting.notified() => return,
            }
        }
    });
    LiveServer {
        url,
        requests,
        task,
    }
}

/// 处理一个连接。返回 `true` 表示之后不再接受新连接。
async fn serve_connection(
    mut socket: tokio::net::TcpStream,
    flv: &FlvSample,
    mode: ServeMode,
    seen: &Mutex<Vec<String>>,
) -> bool {
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => return false,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
    seen.lock()
        .unwrap()
        .push(String::from_utf8_lossy(&request).into_owned());

    let limit_ms = match mode {
        ServeMode::Html => {
            let body = "<!DOCTYPE html><html><body>直播已结束</body></html>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
            return false;
        }
        ServeMode::Empty => {
            let response = b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response).await;
            let _ = socket.shutdown().await;
            return false;
        }
        ServeMode::NotFound => {
            let response =
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response).await;
            let _ = socket.shutdown().await;
            return false;
        }
        ServeMode::Redirect if request.starts_with(b"GET /live/test.flv ") => {
            let response = b"HTTP/1.1 302 Found\r\nLocation: /live/moved.flv\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response).await;
            let _ = socket.shutdown().await;
            return false;
        }
        ServeMode::Live | ServeMode::Redirect => u32::MAX,
        ServeMode::CutAfter(secs) | ServeMode::StallAfter(secs) => secs * 1000,
    };
    let response = b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nConnection: close\r\n\r\n";
    if socket.write_all(response).await.is_err() || socket.write_all(&flv.header).await.is_err() {
        return false;
    }
    let begin = Instant::now();
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
            return false;
        }
    }
    match mode {
        ServeMode::CutAfter(_) => {
            let _ = socket.shutdown().await;
            true
        }
        ServeMode::StallAfter(_) => {
            sleep(Duration::from_secs(600)).await;
            false
        }
        _ => false,
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
        title: None,
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
    eprintln!("正常停止耗时 {:?}", stop_requested.elapsed());

    assert!(
        stop_requested.elapsed() < Duration::from_secs(5),
        "graceful stop was slow"
    );
    assert_eq!(outcome.reason, EndReason::UserStopped);
    // 退出码 0 说明是读到 `q` 正常收尾，而不是被强制结束。
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

/// 等推流卡住超过 [`STOP_IDLE_THRESHOLD`]：先录到 `secs` 秒，再等进度停住。
async fn wait_for_stall(handle: &RecordingHandle, secs: f64) {
    wait_for_duration(handle, secs).await;
    let idle = STOP_IDLE_THRESHOLD + Duration::from_secs(2);
    let duration = |handle: &RecordingHandle| {
        handle
            .snapshot()
            .progress
            .and_then(|progress| progress.duration_secs)
    };
    let mut last = duration(handle);
    let mut since = Instant::now();
    timeout(Duration::from_secs(30), async {
        while since.elapsed() < idle {
            sleep(Duration::from_millis(200)).await;
            let current = duration(handle);
            if current != last {
                last = current;
                since = Instant::now();
            }
        }
    })
    .await
    .expect("stream did not stall");
    assert!(!handle.is_finished());
}

#[tokio::test]
async fn stop_during_stall_kills_ffmpeg_quickly() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::StallAfter(4)).await;
    let (listener, events) = recording_listener();

    // 默认选项：强制结束要等 15 秒，看门狗要 90 秒。
    let handle = start(
        request(&ffmpeg, &server.url, temp.path(), unsegmented()),
        listener,
    )
    .await;
    wait_for_stall(&handle, 3.0).await;
    let stop_requested = Instant::now();
    let outcome = handle.stop_and_wait().await;
    let elapsed = stop_requested.elapsed();
    eprintln!("断流时停止耗时 {elapsed:?}");

    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    assert_eq!(outcome.reason, EndReason::UserStopped, "{outcome:?}");
    assert_eq!(outcome.files.len(), 1);
    assert_playable(&ffmpeg, &outcome.files[0]);
    assert!(media_duration(&ffmpeg, &outcome.files[0]) >= 3.0);
    assert!(events.lock().unwrap().iter().any(|event| matches!(
        event,
        RecorderEvent::Stopping {
            cause: StopCause::User
        }
    )));
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
    // 保存目录含 `%`，segment 封装器会把它当作格式串，引擎须转义。
    let root = temp.path().join("100% 录像").join("rec%d");

    let handle = start(
        request(&ffmpeg, &server.url, &root, options),
        quiet_listener(),
    )
    .await;
    let output_path = handle.snapshot().output_path.unwrap();
    assert!(output_path.to_string_lossy().ends_with("_%03d.ts"));
    assert!(output_path.starts_with(&root), "{}", output_path.display());
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
    // 录到 2 秒后再把磁盘切换为不足，不依赖查询次数和机器速度。
    let disk_low = Arc::new(AtomicBool::new(false));
    let flag = disk_low.clone();
    let probe: SpaceProbe = Arc::new(move |_: &Path| {
        Ok(if flag.load(Ordering::SeqCst) {
            1 << 30
        } else {
            10 << 30
        })
    });
    let options = RecordOptions {
        segment_enabled: false,
        disk_check_interval_secs: 1,
        ..RecordOptions::default()
    };
    let (listener, events) = recording_listener();

    let handle = start_with_probe(
        request(&ffmpeg, &server.url, temp.path(), options),
        listener,
        probe,
    )
    .await;
    wait_for_duration(&handle, 2.0).await;
    disk_low.store(true, Ordering::SeqCst);
    let outcome = wait_outcome(&handle, 30).await;

    assert_eq!(outcome.reason, EndReason::DiskFull, "{outcome:?}");
    assert_eq!(outcome.files.len(), 1);
    assert_playable(&ffmpeg, &outcome.files[0]);
    assert!(events.lock().unwrap().iter().any(|event| matches!(
        event,
        RecorderEvent::Stopping {
            cause: StopCause::DiskFull
        }
    )));
}

/// 看门狗测试用的选项：时限 3 秒（测试中把下限降到 1 秒），强制结束前等 3 秒。
fn watchdog_options() -> RecordOptions {
    RecordOptions {
        stall_timeout_secs: 3,
        stop_timeout_secs: 3,
        ..unsegmented()
    }
}

#[tokio::test]
async fn stalled_source_is_stopped_by_watchdog() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::StallAfter(2)).await;
    let (listener, events) = recording_listener();

    let begin = Instant::now();
    let handle = start_inner(
        request(&ffmpeg, &server.url, temp.path(), watchdog_options()),
        listener,
        default_space_probe(),
        1,
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;
    let elapsed = begin.elapsed();

    // ffmpeg 自己的 -rw_timeout 要 15 秒才会触发；约 12 秒内结束说明是看门狗停的。
    assert!(elapsed < Duration::from_secs(12), "took {elapsed:?}");
    assert_eq!(outcome.reason, EndReason::NetworkError, "{outcome:?}");
    assert_eq!(
        outcome.message.as_deref(),
        Some("长时间没有收到新数据，已停止录制")
    );
    assert!(events.lock().unwrap().iter().any(|event| matches!(
        event,
        RecorderEvent::Stopping {
            cause: StopCause::Stalled
        }
    )));
    assert_eq!(outcome.files.len(), 1);
    assert_playable(&ffmpeg, &outcome.files[0]);
}

#[tokio::test]
async fn user_stop_overrides_watchdog_stop() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::StallAfter(2)).await;
    let (listener, events) = recording_listener();

    let handle = start_inner(
        request(&ffmpeg, &server.url, temp.path(), watchdog_options()),
        listener,
        default_space_probe(),
        1,
    )
    .await;
    // 等看门狗开始停止，此时 ffmpeg 卡在读取上，还没退出。
    timeout(Duration::from_secs(20), async {
        while handle.snapshot().stop_cause != Some(StopCause::Stalled) {
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("watchdog did not fire");
    assert!(!handle.is_finished());
    let outcome = handle.stop_and_wait().await;

    assert_eq!(outcome.reason, EndReason::UserStopped, "{outcome:?}");
    assert!(!outcome.reason.should_recheck());
    let events = events.lock().unwrap();
    let causes: Vec<StopCause> = events
        .iter()
        .filter_map(|event| match event {
            RecorderEvent::Stopping { cause } => Some(*cause),
            _ => None,
        })
        .collect();
    assert_eq!(causes, vec![StopCause::Stalled, StopCause::User]);
}

#[tokio::test]
async fn html_or_empty_response_reports_stream_end() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    for mode in [ServeMode::Html, ServeMode::Empty] {
        let server = start_server(&sample, mode).await;
        let handle = start(
            request(&ffmpeg, &server.url, temp.path(), unsegmented()),
            quiet_listener(),
        )
        .await;
        let outcome = wait_outcome(&handle, 30).await;
        // 刚下播的 CDN：交给调度重新检测，而不是当作需要用户处理的故障。
        assert_eq!(outcome.reason, EndReason::StreamEnded, "{outcome:?}");
        assert!(outcome.reason.should_recheck());
        assert!(outcome.files.is_empty());
        eprintln!("{:?}: {:?}", outcome.reason, outcome.message);
    }
}

/// 子进程模式：由父测试通过环境变量 `LUBOBAO_HELPER_MODE` 启动，单独运行时直接返回。
#[tokio::test]
#[ignore = "由 orphan/stop_all 测试在子进程中启动"]
async fn subprocess_helper() {
    let Ok(mode) = std::env::var("LUBOBAO_HELPER_MODE") else {
        return;
    };
    let ffmpeg = test_ffmpeg().expect("LUBOBAO_TEST_FFMPEG");
    let url = std::env::var("LUBOBAO_HELPER_URL").unwrap();
    let root = PathBuf::from(std::env::var_os("LUBOBAO_HELPER_OUT").unwrap());
    match mode.as_str() {
        "orphan" => {
            let handle = start(
                request(&ffmpeg, &url, &root, unsegmented()),
                quiet_listener(),
            )
            .await;
            wait_for_duration(&handle, 1.0).await;
            println!("FFMPEG_PID={}", handle.snapshot().pid.unwrap());
            // 等父测试把本进程结束掉。
            sleep(Duration::from_secs(120)).await;
        }
        "stop_all" => {
            // 同一主播、同一秒开始两次录制，也验证文件名不会冲突。
            let first = start(
                request(&ffmpeg, &url, &root, unsegmented()),
                quiet_listener(),
            )
            .await;
            let second = start(
                request(&ffmpeg, &url, &root, unsegmented()),
                quiet_listener(),
            )
            .await;
            wait_for_duration(&first, 2.0).await;
            wait_for_duration(&second, 2.0).await;
            assert_eq!(active_recordings(), 2);
            let outcomes = stop_all().await;
            assert_eq!(outcomes.len(), 2);
            for outcome in &outcomes {
                assert_eq!(outcome.reason, EndReason::UserStopped, "{outcome:?}");
                assert_eq!(outcome.files.len(), 1);
                assert_playable(&ffmpeg, &outcome.files[0]);
            }
            assert_ne!(outcomes[0].files, outcomes[1].files);
            assert_eq!(active_recordings(), 0);
            assert!(first.is_finished() && second.is_finished());
            println!("STOP_ALL_OK");
        }
        "stop_all_stalled" => {
            let handle = start(
                request(&ffmpeg, &url, &root, unsegmented()),
                quiet_listener(),
            )
            .await;
            wait_for_stall(&handle, 3.0).await;
            let stop_requested = Instant::now();
            let outcomes = stop_all().await;
            let elapsed = stop_requested.elapsed();
            eprintln!("退出时断流录制收尾耗时 {elapsed:?}");
            assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
            assert_eq!(outcomes.len(), 1);
            assert_eq!(outcomes[0].reason, EndReason::UserStopped, "{outcomes:?}");
            assert_playable(&ffmpeg, &outcomes[0].files[0]);
            assert_eq!(active_recordings(), 0);
            println!("STOP_ALL_OK");
        }
        other => panic!("unknown helper mode {other}"),
    }
}

/// 在子进程中运行 [`subprocess_helper`]。
fn helper_command(mode: &str, url: &str, out: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "recorder::tests::subprocess_helper",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LUBOBAO_HELPER_MODE", mode)
        .env("LUBOBAO_HELPER_URL", url)
        .env("LUBOBAO_HELPER_OUT", out)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command
}

/// 进程是否还在运行。
fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: 只查询并关闭自己打开的句柄。
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(process, &mut code);
            CloseHandle(process);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: 信号 0 只检查进程是否存在。
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = pid;
        false
    }
}

/// 测试失败时清理残留的 ffmpeg。
fn kill_process(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output();
    #[cfg(target_os = "linux")]
    // SAFETY: 只向测试自己启动的进程发信号。
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = pid;
}

#[tokio::test]
async fn ffmpeg_dies_when_parent_process_is_killed() {
    if !cfg!(any(windows, target_os = "linux")) {
        eprintln!("跳过：此系统没有父进程退出时结束子进程的机制，依赖 stop_all");
        return;
    }
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::Live).await;

    let mut helper = helper_command("orphan", &server.url, temp.path())
        .spawn()
        .unwrap();
    let mut stdout = tokio::io::BufReader::new(helper.stdout.take().unwrap()).lines();
    let pid: u32 = timeout(Duration::from_secs(60), async {
        while let Some(line) = stdout.next_line().await.unwrap() {
            // libtest 会把测试名打印在同一行，所以在行内查找。
            if let Some((_, pid)) = line.split_once("FFMPEG_PID=") {
                return pid.trim().parse().unwrap();
            }
        }
        panic!("helper exited without starting ffmpeg");
    })
    .await
    .expect("helper did not report the ffmpeg pid");
    assert!(process_alive(pid), "ffmpeg should be running");

    // 模拟崩溃：直接结束父进程，不经过任何收尾代码。
    helper.kill().await.unwrap();
    let gone = timeout(Duration::from_secs(10), async {
        while process_alive(pid) {
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if gone.is_err() {
        // 清理孤儿进程，避免影响其他测试。
        kill_process(pid);
        panic!("ffmpeg {pid} kept running after its parent was killed");
    }
}

#[tokio::test]
async fn stop_all_stops_every_recording() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::Live).await;

    // stop_all 作用于整个进程，放到子进程里跑，避免停掉并行的其他测试。
    let output = timeout(
        Duration::from_secs(90),
        helper_command("stop_all", &server.url, temp.path()).output(),
    )
    .await
    .expect("helper timed out")
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("STOP_ALL_OK"),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn stop_all_finishes_stalled_recordings_quickly() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::StallAfter(4)).await;

    let output = timeout(
        Duration::from_secs(90),
        helper_command("stop_all_stalled", &server.url, temp.path()).output(),
    )
    .await
    .expect("helper timed out")
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("STOP_ALL_OK"),
        "stdout: {stdout}
stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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

fn naming_options() -> RecordOptions {
    RecordOptions {
        folder_by_date: true,
        folder_by_title: true,
        filename_includes_title: true,
        time_subtitle: true,
        ..unsegmented()
    }
}

#[tokio::test]
async fn recording_dir_gets_date_and_title_layers_and_failed_launch_leaves_no_subtitle() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("no-ffmpeg.exe");
    let handle = start(
        RecordingRequest {
            title: Some("今晚 唱歌：100%".to_string()),
            ..request(
                &missing,
                "http://127.0.0.1:1/x.flv",
                temp.path(),
                naming_options(),
            )
        },
        quiet_listener(),
    )
    .await;
    let outcome = handle.wait().await;
    assert_eq!(outcome.reason, EndReason::FfmpegFailed);
    let started = Local
        .timestamp_millis_opt(outcome.started_at_ms)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let dir = outcome.output_dir.unwrap();
    assert_eq!(
        dir,
        temp.path()
            .join("录像 测试")
            .join("抖音")
            .join("测试主播_小明")
            .join(started)
            .join("今晚_唱歌_100_测试主播_小明")
    );
    assert!(dir.is_dir());
    // 启动失败：目录里没有任何文件，尤其没有空的 .srt。
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[tokio::test]
async fn time_subtitle_is_written_next_to_the_unsegmented_recording() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::Live).await;
    let handle = start(
        RecordingRequest {
            title: Some("今晚唱歌".to_string()),
            ..request(&ffmpeg, &server.url, temp.path(), naming_options())
        },
        quiet_listener(),
    )
    .await;
    wait_for_duration(&handle, 3.0).await;
    let video = handle.snapshot().output_path.unwrap();
    let srt = video.with_extension("srt");
    // 录制进行中就能读到已写入的字幕。
    let live_text = std::fs::read_to_string(&srt).expect("srt exists while recording");
    assert!(
        live_text.starts_with("1\n00:00:00,000 --> 00:00:01,000\n20"),
        "{live_text}"
    );
    let outcome = handle.stop_and_wait().await;

    assert_eq!(outcome.files, vec![video.clone()]);
    let name = video.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("测试主播_小明_今晚唱歌_"), "{name}");
    let text = std::fs::read_to_string(&srt).unwrap();
    assert!(
        text.contains("\n2\n00:00:01,000 --> 00:00:02,000\n"),
        "{text}"
    );
    assert!(
        text.contains("\n3\n00:00:02,000 --> 00:00:03,000\n"),
        "{text}"
    );
    assert_playable(&ffmpeg, &video);
}

#[tokio::test]
async fn segmented_recording_does_not_write_a_time_subtitle() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::CutAfter(5)).await;
    let options = RecordOptions {
        segment_enabled: true,
        segment_secs: 3,
        time_subtitle: true,
        disk_threshold_bytes: 0,
        ..RecordOptions::default()
    };
    let handle = start(
        request(&ffmpeg, &server.url, temp.path(), options),
        quiet_listener(),
    )
    .await;
    let outcome = wait_outcome(&handle, 40).await;
    assert!(!outcome.files.is_empty(), "{outcome:?}");
    let dir = outcome.output_dir.unwrap();
    let srt_files = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "srt"))
        .count();
    assert_eq!(srt_files, 0);
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
fn stall_timeout_zero_disables_and_small_values_are_raised() {
    assert_eq!(effective_stall_timeout(0, MIN_STALL_TIMEOUT_SECS), None);
    assert_eq!(
        effective_stall_timeout(5, MIN_STALL_TIMEOUT_SECS),
        Some(Duration::from_secs(MIN_STALL_TIMEOUT_SECS))
    );
    assert_eq!(
        effective_stall_timeout(90, MIN_STALL_TIMEOUT_SECS),
        Some(Duration::from_secs(90))
    );
    assert_eq!(effective_stall_timeout(3, 1), Some(Duration::from_secs(3)));
}

#[test]
fn concurrent_starts_reserve_distinct_file_names() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let ts = naming::TS_EXTENSION;
    let (first, first_guard) = reserve_stem(dir, "主播_2026-11-22_10-00-00", true, ts);
    let (second, _second_guard) = reserve_stem(dir, "主播_2026-11-22_10-00-00", true, ts);
    assert_eq!(first, "主播_2026-11-22_10-00-00");
    assert_eq!(second, "主播_2026-11-22_10-00-00_2");
    // 释放后同名可以再次使用。
    drop(first_guard);
    let (third, _third_guard) = reserve_stem(dir, "主播_2026-11-22_10-00-00", true, ts);
    assert_eq!(third, first);
}

#[tokio::test]
async fn relative_output_dir_becomes_absolute() {
    let missing = PathBuf::from("no-such-ffmpeg-for-test.exe");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let probe: SpaceProbe = Arc::new(move |path: &Path| {
        sink.lock().unwrap().push(path.to_path_buf());
        Ok(u64::MAX)
    });
    let relative = PathBuf::from("target").join("recorder-relative-test");
    let handle = start_with_probe(
        RecordingRequest {
            output_dir: relative.clone(),
            ..request(
                &missing,
                "http://127.0.0.1:1/x.flv",
                Path::new(""),
                RecordOptions::default(),
            )
        },
        quiet_listener(),
        probe,
    )
    .await;
    let outcome = handle.wait().await;
    let _ = std::fs::remove_dir_all(&relative);
    let probed = seen.lock().unwrap().clone();
    assert_eq!(probed.len(), 1);
    assert!(probed[0].is_absolute(), "{probed:?}");
    assert!(outcome.output_dir.unwrap().is_absolute());
}

#[tokio::test]
async fn space_probe_failure_is_reported_as_warning() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::Live).await;
    let probe: SpaceProbe = Arc::new(|_: &Path| Err(io::Error::other("path too long")));
    let (listener, events) = recording_listener();
    let options = RecordOptions {
        segment_enabled: false,
        disk_check_interval_secs: 1,
        ..RecordOptions::default()
    };
    let handle = start_with_probe(
        request(&ffmpeg, &server.url, temp.path(), options),
        listener,
        probe,
    )
    .await;
    wait_for_duration(&handle, 3.0).await;
    let outcome = handle.stop_and_wait().await;
    assert_eq!(outcome.reason, EndReason::UserStopped);

    let events = events.lock().unwrap();
    assert!(matches!(events[0], RecorderEvent::Started { .. }));
    let warnings: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            RecorderEvent::Warning { message } => Some(message),
            _ => None,
        })
        .collect();
    // 启动前一次；录制中连续失败只报告一次。
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("path too long"));
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
/// 运行：`cargo test -p lubobao-core live_douyin -- --ignored --nocapture`。
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
            title: info.title.clone(),
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

// FLV 直接下载（`direct.rs`）。除最后一个外都不需要 ffmpeg：用合成的 FLV 字节流。

/// 合成一段 FLV：文件头加每 100 毫秒一个 2000 字节的视频 tag。内容不能解码，
/// 只用来检查原样保存。
fn synthetic_flv(dir: &Path, secs: u32) -> PathBuf {
    let mut bytes = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
    for index in 0..secs * 10 {
        let timestamp = index * 100;
        let payload: Vec<u8> = (0..2000u32).map(|i| (i + index) as u8).collect();
        let size = payload.len() as u32;
        bytes.push(9);
        bytes.extend_from_slice(&size.to_be_bytes()[1..]);
        bytes.extend_from_slice(&timestamp.to_be_bytes()[1..]);
        bytes.push((timestamp >> 24) as u8);
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&(11 + size).to_be_bytes());
    }
    let path = dir.join("synthetic.flv");
    std::fs::write(&path, bytes).unwrap();
    path
}

/// 服务端在 `CutAfter(secs)` 模式下发出的全部字节。
fn served_prefix(sample: &Path, secs: u32) -> Vec<u8> {
    let flv = parse_flv(&std::fs::read(sample).unwrap());
    let mut bytes = flv.header;
    for (timestamp, tag) in &flv.tags {
        if *timestamp >= secs * 1000 {
            break;
        }
        bytes.extend_from_slice(tag);
    }
    bytes
}

fn direct_options() -> RecordOptions {
    RecordOptions {
        direct_download: true,
        ..unsegmented()
    }
}

/// 目录里的全部文件（不含子目录）。
fn files_in(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn no_ffmpeg(temp: &Path) -> PathBuf {
    temp.join("no-ffmpeg.exe")
}

#[tokio::test]
async fn direct_download_saves_the_flv_body_until_the_server_closes() {
    let temp = tempfile::tempdir().unwrap();
    let sample = synthetic_flv(temp.path(), 10);
    let server = start_server(&sample, ServeMode::CutAfter(3)).await;
    let (listener, events) = recording_listener();
    // 分段开关打开也不分段。
    let options = RecordOptions {
        segment_enabled: true,
        segment_secs: 1,
        ..direct_options()
    };

    let handle = start(
        request(&no_ffmpeg(temp.path()), &server.url, temp.path(), options),
        listener,
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;

    assert_eq!(outcome.reason, EndReason::StreamEnded, "{outcome:?}");
    assert_eq!(outcome.exit_code, None);
    assert_eq!(outcome.files.len(), 1, "{outcome:?}");
    let file = &outcome.files[0];
    let expected_dir = temp
        .path()
        .join("录像 测试")
        .join("抖音")
        .join("测试主播_小明");
    assert_eq!(file.parent().unwrap(), expected_dir);
    let name = file.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("测试主播_小明_") && name.ends_with(".flv"),
        "{name}"
    );
    let expected = served_prefix(&sample, 3);
    assert_eq!(std::fs::read(file).unwrap(), expected);
    assert_eq!(outcome.bytes_written, expected.len() as u64);
    assert!(outcome.duration_secs.unwrap() > 0.5, "{outcome:?}");
    assert_eq!(files_in(&expected_dir), vec![file.clone()]);

    let events = events.lock().unwrap();
    match events.first() {
        Some(RecorderEvent::Started {
            output_path,
            source_format,
            segmented,
            pid,
            ..
        }) => {
            assert_eq!(output_path, file);
            assert_eq!(*source_format, SourceFormat::Flv);
            assert!(!segmented);
            assert_eq!(*pid, None);
        }
        other => panic!("{other:?}"),
    }
    let progress = events
        .iter()
        .find_map(|event| match event {
            RecorderEvent::Progress { progress } => Some(progress.clone()),
            _ => None,
        })
        .expect("progress event");
    assert!(progress.size_bytes.unwrap() > 0, "{progress:?}");
    assert!(progress.bitrate_kbps.unwrap() > 0.0, "{progress:?}");
    assert!(matches!(events.last(), Some(RecorderEvent::Finished { outcome: o }) if o == &outcome));

    // 请求头与 ffmpeg 录制相同。
    let request_text = server.requests.lock().unwrap()[0].to_ascii_lowercase();
    assert!(
        request_text.contains(&format!("user-agent: {}", TEST_USER_AGENT.to_ascii_lowercase())),
        "{request_text}"
    );
    assert!(
        request_text.contains(&format!("referer: {TEST_REFERER}")),
        "{request_text}"
    );
}

#[tokio::test]
async fn direct_download_follows_redirects_with_the_default_user_agent() {
    let temp = tempfile::tempdir().unwrap();
    let sample = synthetic_flv(temp.path(), 30);
    let server = start_server(&sample, ServeMode::Redirect).await;
    let handle = start(
        RecordingRequest {
            headers: BTreeMap::from([("Referer".to_string(), TEST_REFERER.to_string())]),
            ..request(
                &no_ffmpeg(temp.path()),
                &server.url,
                temp.path(),
                direct_options(),
            )
        },
        quiet_listener(),
    )
    .await;
    wait_for_duration(&handle, 1.0).await;
    let outcome = handle.stop_and_wait().await;
    assert_eq!(outcome.reason, EndReason::UserStopped, "{outcome:?}");
    assert_eq!(outcome.files.len(), 1);

    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "{requests:?}");
    let moved = requests[1].to_ascii_lowercase();
    assert!(moved.starts_with("get /live/moved.flv "), "{moved}");
    let user_agent = command::DEFAULT_USER_AGENT.to_ascii_lowercase();
    assert!(
        moved.contains(&format!("user-agent: {user_agent}")),
        "{moved}"
    );
    // 跳转后仍带原来的 Referer，不换成跳转前的地址。
    assert!(
        moved.contains(&format!("referer: {TEST_REFERER}")),
        "{moved}"
    );
}

#[tokio::test]
async fn direct_download_user_stop_closes_the_file_immediately() {
    let temp = tempfile::tempdir().unwrap();
    let sample = synthetic_flv(temp.path(), 30);
    let server = start_server(&sample, ServeMode::Live).await;
    let (listener, events) = recording_listener();
    let handle = start(
        request(
            &no_ffmpeg(temp.path()),
            &server.url,
            temp.path(),
            direct_options(),
        ),
        listener,
    )
    .await;
    wait_for_duration(&handle, 2.0).await;
    let stop_requested = Instant::now();
    let outcome = handle.stop_and_wait().await;
    let elapsed = stop_requested.elapsed();

    assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    assert_eq!(outcome.reason, EndReason::UserStopped, "{outcome:?}");
    assert_eq!(outcome.files.len(), 1);
    let saved = std::fs::read(&outcome.files[0]).unwrap();
    assert!(saved.starts_with(b"FLV"));
    assert_eq!(saved.len() as u64, outcome.bytes_written);
    assert!(outcome.duration_secs.unwrap() >= 2.0, "{outcome:?}");
    assert_eq!(handle.snapshot().state, RecorderState::Finished);
    let causes: Vec<StopCause> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            RecorderEvent::Stopping { cause } => Some(*cause),
            _ => None,
        })
        .collect();
    assert_eq!(causes, vec![StopCause::User]);
}

#[tokio::test]
async fn direct_download_http_errors_fail_without_leaving_a_file() {
    let temp = tempfile::tempdir().unwrap();
    let sample = synthetic_flv(temp.path(), 3);
    let server = start_server(&sample, ServeMode::NotFound).await;
    let handle = start(
        request(
            &no_ffmpeg(temp.path()),
            &server.url,
            temp.path(),
            direct_options(),
        ),
        quiet_listener(),
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;
    assert_eq!(outcome.reason, EndReason::NetworkError, "{outcome:?}");
    assert!(
        outcome.message.as_deref().unwrap().contains("404"),
        "{outcome:?}"
    );
    assert!(outcome.files.is_empty());
    assert!(files_in(&outcome.output_dir.unwrap()).is_empty());

    // 连不上：同样是网络错误。
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/live/test.flv");
    let handle = start(
        request(
            &no_ffmpeg(temp.path()),
            &url,
            temp.path(),
            direct_options(),
        ),
        quiet_listener(),
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;
    assert_eq!(outcome.reason, EndReason::NetworkError, "{outcome:?}");
    assert!(outcome.files.is_empty());
    assert!(files_in(&outcome.output_dir.unwrap()).is_empty());
}

#[tokio::test]
async fn direct_download_of_html_or_empty_responses_is_stream_end_without_a_file() {
    let temp = tempfile::tempdir().unwrap();
    let sample = synthetic_flv(temp.path(), 3);
    for mode in [ServeMode::Html, ServeMode::Empty] {
        let server = start_server(&sample, mode).await;
        let handle = start(
            request(
                &no_ffmpeg(temp.path()),
                &server.url,
                temp.path(),
                direct_options(),
            ),
            quiet_listener(),
        )
        .await;
        let outcome = wait_outcome(&handle, 30).await;
        assert_eq!(outcome.reason, EndReason::StreamEnded, "{outcome:?}");
        assert!(outcome.message.is_some());
        assert!(outcome.files.is_empty());
        assert!(files_in(&outcome.output_dir.unwrap()).is_empty());
    }
}

#[tokio::test]
async fn direct_download_watchdog_stops_a_stalled_stream() {
    let temp = tempfile::tempdir().unwrap();
    let sample = synthetic_flv(temp.path(), 30);
    let server = start_server(&sample, ServeMode::StallAfter(2)).await;
    let (listener, events) = recording_listener();
    let options = RecordOptions {
        stall_timeout_secs: 2,
        ..direct_options()
    };
    let begin = Instant::now();
    let handle = start_inner(
        request(&no_ffmpeg(temp.path()), &server.url, temp.path(), options),
        listener,
        default_space_probe(),
        1,
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;
    let elapsed = begin.elapsed();

    // 读超时要 15 秒；这么快结束说明是看门狗停的。
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    assert_eq!(outcome.reason, EndReason::NetworkError, "{outcome:?}");
    assert_eq!(
        outcome.message.as_deref(),
        Some("长时间没有收到新数据，已停止录制")
    );
    assert_eq!(outcome.files.len(), 1);
    let saved = std::fs::read(&outcome.files[0]).unwrap();
    assert!(saved.starts_with(b"FLV"));
    assert!(events.lock().unwrap().iter().any(|event| matches!(
        event,
        RecorderEvent::Stopping {
            cause: StopCause::Stalled
        }
    )));
}

#[tokio::test]
async fn direct_download_stops_when_disk_space_runs_low() {
    let temp = tempfile::tempdir().unwrap();
    let sample = synthetic_flv(temp.path(), 30);
    let server = start_server(&sample, ServeMode::Live).await;
    let disk_low = Arc::new(AtomicBool::new(false));
    let flag = disk_low.clone();
    let probe: SpaceProbe = Arc::new(move |_: &Path| {
        Ok(if flag.load(Ordering::SeqCst) {
            1 << 30
        } else {
            10 << 30
        })
    });
    let options = RecordOptions {
        disk_threshold_bytes: DEFAULT_DISK_THRESHOLD_BYTES,
        disk_check_interval_secs: 1,
        ..direct_options()
    };
    let handle = start_with_probe(
        request(&no_ffmpeg(temp.path()), &server.url, temp.path(), options),
        quiet_listener(),
        probe,
    )
    .await;
    wait_for_duration(&handle, 1.0).await;
    disk_low.store(true, Ordering::SeqCst);
    let outcome = wait_outcome(&handle, 30).await;
    assert_eq!(outcome.reason, EndReason::DiskFull, "{outcome:?}");
    assert_eq!(outcome.files.len(), 1);
}

#[tokio::test]
async fn direct_download_writes_the_time_subtitle_and_follows_naming_options() {
    let temp = tempfile::tempdir().unwrap();
    let sample = synthetic_flv(temp.path(), 30);
    let server = start_server(&sample, ServeMode::Live).await;
    let handle = start(
        RecordingRequest {
            title: Some("今晚唱歌".to_string()),
            ..request(
                &no_ffmpeg(temp.path()),
                &server.url,
                temp.path(),
                RecordOptions {
                    direct_download: true,
                    ..naming_options()
                },
            )
        },
        quiet_listener(),
    )
    .await;
    wait_for_duration(&handle, 2.5).await;
    let video = handle.snapshot().output_path.unwrap();
    let outcome = handle.stop_and_wait().await;

    assert_eq!(outcome.files, vec![video.clone()]);
    let started = Local
        .timestamp_millis_opt(outcome.started_at_ms)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(
        video.parent().unwrap(),
        temp.path()
            .join("录像 测试")
            .join("抖音")
            .join("测试主播_小明")
            .join(started)
            .join("今晚唱歌_测试主播_小明")
    );
    let name = video.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("测试主播_小明_今晚唱歌_") && name.ends_with(".flv"),
        "{name}"
    );
    let text = std::fs::read_to_string(video.with_extension("srt")).unwrap();
    assert!(
        text.starts_with("1\n00:00:00,000 --> 00:00:01,000\n20"),
        "{text}"
    );
    assert!(
        text.contains("\n2\n00:00:01,000 --> 00:00:02,000\n"),
        "{text}"
    );
}

#[tokio::test]
async fn direct_download_uses_socks_proxies() {
    let temp = tempfile::tempdir().unwrap();
    // 没有服务在监听的端口：代理连不上。
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let options = RecordOptions {
        proxy: Some(format!("socks5://127.0.0.1:{port}")),
        ..direct_options()
    };
    let handle = start(
        request(
            &no_ffmpeg(temp.path()),
            "http://127.0.0.1:1/x.flv",
            temp.path(),
            options,
        ),
        quiet_listener(),
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;
    // ffmpeg 录制会在启动前拒绝 SOCKS 代理（`socks_proxy_is_rejected_before_launch`）。
    assert_eq!(outcome.reason, EndReason::NetworkError, "{outcome:?}");
    assert!(outcome.files.is_empty());
}

#[tokio::test]
async fn hls_sources_are_not_downloaded_directly() {
    let temp = tempfile::tempdir().unwrap();
    let handle = start(
        RecordingRequest {
            source_format: SourceFormat::Hls,
            ..request(
                &no_ffmpeg(temp.path()),
                "http://127.0.0.1:1/live.m3u8",
                temp.path(),
                direct_options(),
            )
        },
        quiet_listener(),
    )
    .await;
    let outcome = handle.wait().await;
    // 交给了 ffmpeg（这里故意不存在）。
    assert_eq!(outcome.reason, EndReason::FfmpegFailed);
    assert!(outcome.message.unwrap().contains("无法启动 ffmpeg"));
}

#[tokio::test]
async fn direct_download_of_a_real_stream_is_playable() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sample = make_sample(&ffmpeg, temp.path());
    let server = start_server(&sample, ServeMode::CutAfter(5)).await;
    let handle = start(
        request(&ffmpeg, &server.url, temp.path(), direct_options()),
        quiet_listener(),
    )
    .await;
    let outcome = wait_outcome(&handle, 30).await;

    assert_eq!(outcome.reason, EndReason::StreamEnded, "{outcome:?}");
    assert_eq!(outcome.files.len(), 1);
    let file = &outcome.files[0];
    assert!(file.to_string_lossy().ends_with(".flv"));
    assert_eq!(std::fs::read(file).unwrap(), served_prefix(&sample, 5));
    assert_playable(&ffmpeg, file);
    let duration = media_duration(&ffmpeg, file);
    assert!((4.0..=5.5).contains(&duration), "duration {duration}");
}
