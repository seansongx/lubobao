//! Live checks against real Bilibili rooms. Run manually:
//! `cargo test -p lubobao-core --test bilibili_live -- --ignored --nocapture --test-threads=1`
//!
//! Without a logged-in cookie Bilibili serves at most
//! 超清 (qn 250), whatever quality is requested.
//!
//! Set `BILIBILI_LIVE_URL` to a room that is currently live to also check
//! stream selection and record about 30 seconds with FFmpeg. The recording
//! test reads FFmpeg from `LUBOBAO_FFMPEG`, falling back to the app's
//! installed component under `%LOCALAPPDATA%`, and keeps the file in
//! `BILIBILI_RECORD_DIR` (default: the system temp directory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, bilibili::BilibiliAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};

/// StreamCap README example (a short id) and the ids it must resolve to.
const README_LINK: &str = "https://live.bilibili.com/320";
const README_ROOM_ID: &str = "286893";
const README_ANCHOR_ID: &str = "13763303";
/// A room id that does not exist.
const MISSING_LINK: &str = "https://live.bilibili.com/999999999";

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

async fn report(adapter: &BilibiliAdapter, url: &str, options: &FetchOptions) -> Option<LiveInfo> {
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

#[tokio::test]
#[ignore = "requires network access to Bilibili"]
async fn readme_link_resolves_against_live_bilibili() {
    let adapter = BilibiliAdapter::new();
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
    match adapter.fetch(MISSING_LINK, &options).await {
        Err(error) if error.kind() == "invalid_url" => println!("OK   {MISSING_LINK}: {error}"),
        other => failures.push(format!("missing room: {other:?}")),
    }
    if let Ok(live_url) = std::env::var("BILIBILI_LIVE_URL") {
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

/// A `b23.tv` share link made with Bilibili's share API for room 21452505.
#[tokio::test]
#[ignore = "requires network access to Bilibili"]
async fn share_short_link_resolves_against_live_bilibili() {
    let adapter = BilibiliAdapter::new();
    let options = FetchOptions::default();
    let link = "https://b23.tv/oolIxoB";
    match report(&adapter, link, &options).await {
        Some(info) if info.room_id == "21452505" && !info.anchor_name.is_empty() => {}
        other => panic!("{link}: {:?}", other.map(|info| info.room_id)),
    }
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
#[ignore = "requires a live Bilibili room (BILIBILI_LIVE_URL) and FFmpeg"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("BILIBILI_LIVE_URL").expect("set BILIBILI_LIVE_URL");
    let adapter = BilibiliAdapter::new();
    let info = report(&adapter, &live_url, &FetchOptions::default())
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    let (source_format, source_url) = selected.source_url(false, false);

    let output_dir = std::env::var("BILIBILI_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-bilibili-live"));
    let ffmpeg = ffmpeg_path();
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "哔哩哔哩".to_string(),
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
        // Bilibili FLV timestamps are whole milliseconds, so a 30 fps stream
        // jitters by a millisecond. The null muxer's default 1/30 s time base
        // then puts two frames in one slot and warns about non-monotonic DTS
        // although every frame decodes. Keeping the source time base makes
        // the check about decoding errors only.
        .args([
            "-fps_mode",
            "passthrough",
            "-enc_time_base",
            "demux",
            "-f",
            "null",
            "-",
        ])
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
