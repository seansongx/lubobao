//! Live checks against real Xiaohongshu rooms. Run manually:
//! `cargo test -p lubobao-core --test xiaohongshu_live -- --ignored --nocapture --test-threads=1`
//!
//! Xiaohongshu rooms are per broadcast, so there is no fixed live link: set
//! `XIAOHONGSHU_LIVE_URL` to a room that is currently live (for example from
//! `www.xiaohongshu.com/livelist`) to check stream selection and record about
//! 30 seconds with FFmpeg, and `XIAOHONGSHU_ENDED_URL` to a room whose
//! broadcast has ended. The recording test reads FFmpeg from
//! `LUBOBAO_TEST_FFMPEG` or `LUBOBAO_FFMPEG`, falling back to the app's
//! installed component under `%LOCALAPPDATA%`, and keeps the file in
//! `XIAOHONGSHU_RECORD_DIR` (default: the system temp directory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, xiaohongshu::XiaohongshuAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};

/// StreamCap README example: a share link from a broadcast long over.
const README_LINK: &str = "http://xhslink.com/xpJpfM";
/// A room id that does not exist.
const MISSING_LINK: &str = "https://www.xiaohongshu.com/livestream/570000000000000001";

async fn report(
    adapter: &XiaohongshuAdapter,
    url: &str,
    options: &FetchOptions,
) -> Result<LiveInfo, String> {
    let started = Instant::now();
    let result = adapter.fetch(url, options).await;
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(info) => {
            println!("OK   {url} ({:?})", options.quality);
            println!(
                "     room_id={} anchor_id={:?} anchor={} title={:?} live={} variants={:?} {elapsed} ms",
                info.room_id,
                info.anchor_id,
                info.anchor_name,
                info.title,
                info.is_live,
                info.variants
                    .iter()
                    .map(|variant| variant.name.as_str())
                    .collect::<Vec<_>>()
            );
            if let Some(selected) = &info.selected {
                let (format, source) = selected.source_url(false, false);
                println!(
                    "     selected={} ({:?}) source={format:?} {source}",
                    selected.variant.name, selected.requested
                );
            }
            Ok(info)
        }
        Err(error) => {
            println!("ERR  {url}");
            println!("     {error} {elapsed} ms");
            Err(error.kind().to_string())
        }
    }
}

/// Pull the first bytes of a stream and check the FLV signature.
async fn check_flv_header(info: &LiveInfo) -> Result<(), String> {
    let selected = info.selected.as_ref().ok_or("no stream")?;
    let url = selected.variant.flv_url.as_deref().ok_or("no flv")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?;
    let mut request = client.get(url);
    for (key, value) in &info.stream_headers {
        request = request.header(key, value);
    }
    let mut response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let chunk = response
        .chunk()
        .await
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    println!(
        "     flv status={status} first={:?}",
        &chunk[..chunk.len().min(3)]
    );
    if status.is_success() && chunk.starts_with(b"FLV") {
        Ok(())
    } else {
        Err(format!("status {status}, not an FLV stream"))
    }
}

/// A `rednote.com` room link is read like the `xiaohongshu.com` one.
#[tokio::test]
#[ignore = "requires network access to Xiaohongshu"]
async fn rednote_room_link_behaves_like_xiaohongshu() {
    let adapter = XiaohongshuAdapter::new();
    let options = FetchOptions::default();
    let link = "https://www.rednote.com/livestream/570000000000000001";
    let started = std::time::Instant::now();
    match adapter.fetch(link, &options).await {
        Err(error) if error.kind() == "invalid_url" => {
            println!("OK   {link}: {error} {} ms", started.elapsed().as_millis())
        }
        other => panic!("{link}: {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires network access to Xiaohongshu"]
async fn links_resolve_against_live_xiaohongshu() {
    let adapter = XiaohongshuAdapter::new();
    let options = FetchOptions::default();
    let mut failures = Vec::new();
    match report(&adapter, README_LINK, &options).await {
        Err(kind) if kind == "invalid_url" => {}
        other => failures.push(format!("expired share link: {other:?}")),
    }
    match report(&adapter, MISSING_LINK, &options).await {
        Err(kind) if kind == "invalid_url" => {}
        other => failures.push(format!("missing room: {other:?}")),
    }
    if let Ok(ended_url) = std::env::var("XIAOHONGSHU_ENDED_URL") {
        match report(&adapter, &ended_url, &options).await {
            Ok(info) if !info.is_live && !info.anchor_name.is_empty() => {}
            other => failures.push(format!("ended room: {other:?}")),
        }
    }
    if let Ok(live_url) = std::env::var("XIAOHONGSHU_LIVE_URL") {
        for quality in Quality::ALL {
            let options = FetchOptions {
                quality,
                ..FetchOptions::default()
            };
            match report(&adapter, &live_url, &options).await {
                Ok(info) if info.selected.is_some() => {
                    if let Err(error) = check_flv_header(&info).await {
                        failures.push(format!("live {quality}: {error}"));
                    }
                }
                other => failures.push(format!("live {quality}: no stream ({other:?})")),
            }
        }
    }
    assert!(failures.is_empty(), "failures: {failures:#?}");
}

fn ffmpeg_path() -> PathBuf {
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

#[tokio::test]
#[ignore = "requires a live Xiaohongshu room (XIAOHONGSHU_LIVE_URL) and FFmpeg"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("XIAOHONGSHU_LIVE_URL").expect("set XIAOHONGSHU_LIVE_URL");
    let adapter = XiaohongshuAdapter::new();
    let info = report(&adapter, &live_url, &FetchOptions::default())
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    // Xiaohongshu is not an FLV-preferred platform; record_url is the FLV stream.
    let (source_format, source_url) = selected.source_url(false, false);

    let output_dir = std::env::var("XIAOHONGSHU_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-xiaohongshu-live"));
    let ffmpeg = ffmpeg_path();
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "小红书".to_string(),
        options: RecordOptions {
            segment_enabled: false,
            ..RecordOptions::default()
        },
    };
    let handle = recorder::start(request, Arc::new(|_| {})).await;
    tokio::time::sleep(Duration::from_secs(35)).await;
    let outcome = handle.stop_and_wait().await;
    println!(
        "outcome={:?} message={:?} bytes={} duration={:?} files={:?}",
        outcome.reason,
        outcome.message,
        outcome.bytes_written,
        outcome.duration_secs,
        outcome.files
    );
    assert_eq!(outcome.files.len(), 1, "one recording file");
    assert!(outcome.bytes_written > 0);

    let check = std::process::Command::new(&ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&outcome.files[0])
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg check");
    let stderr = String::from_utf8_lossy(&check.stderr);
    // FLV millisecond timestamps can round onto the same tick in the null
    // muxer; that is a property of the source, not a decoding error.
    const SOURCE_DTS: &str = "non monotonically increasing dts";
    let repeated = stderr
        .lines()
        .filter(|line| line.contains(SOURCE_DTS))
        .count();
    let errors: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.contains(SOURCE_DTS))
        .collect();
    println!(
        "check exit={:?} stderr_lines={} repeated_source_dts={repeated}",
        check.status.code(),
        stderr.lines().count()
    );
    assert!(check.status.success(), "ffmpeg check failed: {stderr}");
    assert!(errors.is_empty(), "ffmpeg reported errors: {errors:#?}");
}
