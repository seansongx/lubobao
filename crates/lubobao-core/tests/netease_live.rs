//! Live checks against real NetEase CC rooms. Run manually:
//! `cargo test -p lubobao-core --test netease_live -- --ignored --nocapture --test-threads=1`
//!
//! Set `NETEASE_LIVE_URL` to a room that is currently live to also check
//! stream selection and record about 30 seconds with FFmpeg. The recording
//! test reads FFmpeg from `LUBOBAO_TEST_FFMPEG` or `LUBOBAO_FFMPEG`, falling
//! back to the app's installed component under `%LOCALAPPDATA%`, and keeps
//! the file in `NETEASE_RECORD_DIR` (default: the system temp directory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, netease::NeteaseAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};

/// StreamCap README example. The account still exists (now an ordinary
/// user, not live), so it resolves to its anchor but is offline.
const README_LINK: &str = "https://cc.163.com/583946984";
const README_ROOM_ID: &str = "583946984";
const README_ANCHOR_ID: &str = "440395813";
/// The same id in the address the browser lands on now.
const GLIVE_LINK: &str = "https://ds.163.com/glive/?ccid=583946984";
/// A CC number with no account.
const MISSING_LINK: &str = "https://cc.163.com/1";

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

async fn report(
    adapter: &NeteaseAdapter,
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

#[tokio::test]
#[ignore = "requires network access to NetEase CC"]
async fn links_resolve_against_live_netease() {
    let adapter = NeteaseAdapter::new();
    let options = FetchOptions::default();
    let mut failures = Vec::new();
    for link in [README_LINK, GLIVE_LINK] {
        match report(&adapter, link, &options).await {
            Ok(info)
                if info.room_id == README_ROOM_ID
                    && info.anchor_id.as_deref() == Some(README_ANCHOR_ID)
                    && !info.anchor_name.is_empty()
                    && !info.is_live => {}
            Ok(info) => failures.push(format!(
                "{link}: room_id {} anchor_id {:?} live {}",
                info.room_id, info.anchor_id, info.is_live
            )),
            Err(kind) => failures.push(format!("{link}: {kind}")),
        }
    }
    match report(&adapter, MISSING_LINK, &options).await {
        Err(kind) if kind == "invalid_url" => {}
        other => failures.push(format!("missing id: {other:?}")),
    }
    if let Ok(live_url) = std::env::var("NETEASE_LIVE_URL") {
        for quality in Quality::ALL {
            let options = FetchOptions {
                quality,
                ..FetchOptions::default()
            };
            match report(&adapter, &live_url, &options).await {
                Ok(info) if info.selected.is_some() => {
                    if let Err(error) = check_flv_header(&info).await {
                        failures.push(format!("{quality}: {error}"));
                    }
                }
                other => failures.push(format!("{quality}: no stream ({other:?})")),
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
#[ignore = "requires a live NetEase CC room (NETEASE_LIVE_URL) and FFmpeg"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("NETEASE_LIVE_URL").expect("set NETEASE_LIVE_URL");
    let quality = Quality::parse_lenient(&std::env::var("NETEASE_QUALITY").unwrap_or_default());
    let adapter = NeteaseAdapter::new();
    let options = FetchOptions {
        quality,
        ..FetchOptions::default()
    };
    let info = report(&adapter, &live_url, &options)
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    // NetEase CC is not an FLV-preferred platform; record_url is the FLV stream.
    let (source_format, source_url) = selected.source_url(false, false);

    let output_dir = std::env::var("NETEASE_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-netease-live"));
    let ffmpeg = ffmpeg_path();
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "网易CC直播".to_string(),
        title: None,
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
        // CC's transcoded tiers (e.g. 高清 960x540) are 30 fps but declare a
        // 60 fps frame rate, and FLV timestamps are whole milliseconds. The
        // null muxer's default frame-rate time base then puts two frames in
        // one slot and warns about non-monotonic DTS although every frame
        // decodes. Keeping the source time base makes the check about
        // decoding errors only (as in the Bilibili test).
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
