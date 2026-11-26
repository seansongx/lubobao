//! Live checks against real YY rooms. Run manually:
//! `cargo test -p lubobao-core --test yy_live -- --ignored --nocapture --test-threads=1`
//!
//! Set `YY_LIVE_URL` to a room that is currently live to also check stream
//! selection and record about 30 seconds with FFmpeg; set `YY_OFFLINE_URL`
//! to a room that is not live to check the offline state. The recording test
//! reads FFmpeg from `LUBOBAO_TEST_FFMPEG` or `LUBOBAO_FFMPEG`, falling back
//! to the app's installed component under `%LOCALAPPDATA%`, and keeps the
//! file in `YY_RECORD_DIR` (default: the system temp directory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{FetchOptions, LiveInfo, PlatformAdapter, Quality, yy::YyAdapter};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};

/// StreamCap README example and the channel id it must resolve to.
const README_LINK: (&str, &str) = ("https://www.yy.com/22490906/22490906", "22490906");
/// A channel id that does not exist.
const MISSING_LINK: &str = "https://www.yy.com/1/1";

fn describe_tiers(info: &LiveInfo) -> String {
    info.variants
        .iter()
        .map(|variant| {
            let quality = variant.quality.map(|quality| quality.code()).unwrap_or("-");
            let formats = match (&variant.flv_url, &variant.hls_url) {
                (Some(_), Some(_)) => "flv+hls",
                (Some(_), None) => "flv",
                (None, Some(_)) => "hls",
                (None, None) => "none",
            };
            format!("{}={}({})", quality, variant.name, formats)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

async fn report(adapter: &YyAdapter, url: &str, options: &FetchOptions) -> Option<LiveInfo> {
    let started = Instant::now();
    let result = adapter.fetch(url, options).await;
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(info) => {
            println!("OK   {url} ({:?})", options.quality);
            println!(
                "     room_id={} anchor_id={:?} anchor={} title={:?} live={} tiers=[{}] {elapsed} ms",
                info.room_id,
                info.anchor_id,
                info.anchor_name,
                info.title,
                info.is_live,
                describe_tiers(&info)
            );
            if let Some(selected) = &info.selected {
                let (format, source) = selected.source_url(false, false);
                println!(
                    "     selected={} ({:?}) source={format:?} {source}",
                    selected.variant.name, selected.requested
                );
                println!("     headers={:?}", info.stream_headers);
            }
            Some(info)
        }
        Err(error) => {
            println!("ERR  {url}");
            println!("     {error} {elapsed} ms");
            None
        }
    }
}

/// `/{sid}` resolves to the same channel as the README `/{sid}/{sid}` link.
#[tokio::test]
#[ignore = "requires network access to YY"]
async fn channel_link_without_sub_channel_resolves() {
    let adapter = YyAdapter::new();
    let options = FetchOptions::default();
    let link = "https://www.yy.com/22490906";
    match report(&adapter, link, &options).await {
        Some(info) if info.room_id == README_LINK.1 => {}
        other => panic!("{link}: {:?}", other.map(|info| info.room_id)),
    }
}

#[tokio::test]
#[ignore = "requires network access to YY"]
async fn readme_link_resolves_against_live_yy() {
    let adapter = YyAdapter::new();
    let options = FetchOptions::default();
    let mut failures = Vec::new();
    let (link, room_id) = README_LINK;
    match report(&adapter, link, &options).await {
        Some(info) if info.room_id == room_id && !info.anchor_name.is_empty() => {}
        Some(info) => failures.push(format!("{link}: room_id {}", info.room_id)),
        None => failures.push(format!("{link}: fetch failed")),
    }
    match adapter.fetch(MISSING_LINK, &options).await {
        Err(error) if error.kind() == "invalid_url" => println!("OK   {MISSING_LINK}: {error}"),
        other => failures.push(format!("missing room: {other:?}")),
    }
    if let Ok(offline_url) = std::env::var("YY_OFFLINE_URL") {
        match report(&adapter, &offline_url, &options).await {
            Some(info) if !info.is_live && info.selected.is_none() => {}
            other => failures.push(format!("offline room: {other:?}")),
        }
    }
    if let Ok(live_url) = std::env::var("YY_LIVE_URL") {
        for quality in [Quality::Od, Quality::Hd, Quality::Ld] {
            let options = FetchOptions {
                quality,
                ..FetchOptions::default()
            };
            let info = report(&adapter, &live_url, &options).await;
            if info.and_then(|info| info.selected).is_none() {
                failures.push(format!("live room {quality}: no stream selected"));
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
#[ignore = "requires a live YY room (YY_LIVE_URL) and FFmpeg"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("YY_LIVE_URL").expect("set YY_LIVE_URL");
    let adapter = YyAdapter::new();
    let info = report(&adapter, &live_url, &FetchOptions::default())
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    let (source_format, source_url) = selected.source_url(false, false);

    let output_dir = std::env::var("YY_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-yy-live"));
    let ffmpeg = ffmpeg_path();
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "YY直播".to_string(),
        title: None,
        options: RecordOptions {
            segment_enabled: false,
            ..RecordOptions::default()
        },
    };
    let handle = recorder::start(request, Arc::new(|_| {})).await;
    tokio::time::sleep(Duration::from_secs(30)).await;
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
    // Some streams repeat a few timestamps in the source FLV itself; the null
    // muxer reports them but every frame decodes. Anything else is a real
    // error. Same rule as `huya_live.rs`.
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
