//! Live checks against real Kuaishou rooms. Run manually:
//! `cargo test --manifest-path src-tauri/Cargo.toml --test kuaishou_live -- --ignored --nocapture --test-threads=1`
//!
//! Set `KUAISHOU_LIVE_URL` to a room that is currently live to also check
//! stream selection and record about 30 seconds with FFmpeg. The recording
//! test reads FFmpeg from `LUBOBAO_FFMPEG`, falling back to the app's
//! installed component under `%LOCALAPPDATA%`, and keeps the file in
//! `KUAISHOU_RECORD_DIR` (default: the system temp directory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_lib::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, kuaishou::KuaishouAdapter,
};
use lubobao_lib::recorder::{self, RecordOptions, RecordingRequest};

/// StreamCap README example and the ids it must resolve to.
const README_LINK: &str = "https://live.kuaishou.com/u/yall1102";
const README_ROOM_ID: &str = "yall1102";
const README_ANCHOR_ID: &str = "2141922601";

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

async fn report(adapter: &KuaishouAdapter, url: &str, options: &FetchOptions) -> Option<LiveInfo> {
    let started = Instant::now();
    let result = adapter.fetch(url, options).await;
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(info) => {
            println!("OK   {url} ({:?})", options.quality);
            println!(
                "     room_id={} anchor_id={:?} anchor={} live={} tiers=[{}] {elapsed} ms",
                info.room_id,
                info.anchor_id,
                info.anchor_name,
                info.is_live,
                describe_tiers(&info)
            );
            if let Some(selected) = &info.selected {
                let (format, source) = selected.source_url(true, false);
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

#[tokio::test]
#[ignore = "requires network access to Kuaishou"]
async fn readme_link_resolves_against_live_kuaishou() {
    let adapter = KuaishouAdapter::new();
    let options = FetchOptions::default();
    let mut failures = Vec::new();
    match report(&adapter, README_LINK, &options).await {
        Some(info)
            if info.room_id == README_ROOM_ID
                && info.anchor_id.as_deref() == Some(README_ANCHOR_ID)
                && !info.anchor_name.is_empty() => {}
        Some(info) => failures.push(format!(
            "readme link: room_id {} anchor_id {:?}",
            info.room_id, info.anchor_id
        )),
        None => failures.push("readme link: fetch failed".to_string()),
    }
    if let Ok(live_url) = std::env::var("KUAISHOU_LIVE_URL") {
        for quality in [Quality::Od, Quality::Uhd, Quality::Hd, Quality::Ld] {
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
#[ignore = "requires a live Kuaishou room (KUAISHOU_LIVE_URL) and FFmpeg"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("KUAISHOU_LIVE_URL").expect("set KUAISHOU_LIVE_URL");
    let adapter = KuaishouAdapter::new();
    let info = report(&adapter, &live_url, &FetchOptions::default())
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    let (source_format, source_url) = selected.source_url(true, false);

    let output_dir = std::env::var("KUAISHOU_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-kuaishou-live"));
    let ffmpeg = ffmpeg_path();
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "快手直播".to_string(),
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
    let errors = String::from_utf8_lossy(&check.stderr);
    println!(
        "check exit={:?} stderr_lines={}",
        check.status.code(),
        errors.lines().count()
    );
    assert!(check.status.success(), "ffmpeg check failed: {errors}");
    assert!(errors.trim().is_empty(), "ffmpeg reported errors: {errors}");
}
