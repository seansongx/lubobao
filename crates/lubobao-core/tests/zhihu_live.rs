//! Live checks against real Zhihu rooms. Run manually:
//! `cargo test -p lubobao-core --test zhihu_live -- --ignored --nocapture --test-threads=1`
//!
//! Set `ZHIHU_LIVE_URL` to a room that is currently live to also check stream
//! selection and record about 30 seconds with FFmpeg; set `ZHIHU_OFFLINE_URL`
//! to a room that is not live to check the offline answer. The recording test
//! reads FFmpeg from `LUBOBAO_TEST_FFMPEG` or `LUBOBAO_FFMPEG`, falling back
//! to the app's installed component under `%LOCALAPPDATA%`, and keeps the
//! file in `ZHIHU_RECORD_DIR` (default: the system temp directory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, zhihu::ZhihuAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};

/// Upstream example link.
const README_LINK: &str = "https://www.zhihu.com/theater/10687?drama_id=2037464603865642996";
const README_ROOM_ID: &str = "10687";
/// A room that does not exist.
const MISSING_LINK: &str = "https://www.zhihu.com/theater/99999999";

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
    adapter: &ZhihuAdapter,
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

#[tokio::test]
#[ignore = "requires network access to Zhihu"]
async fn links_resolve_against_live_zhihu() {
    let adapter = ZhihuAdapter::new();
    let options = FetchOptions::default();
    let mut failures = Vec::new();
    // The upstream example may or may not be live; only identity is checked.
    match report(&adapter, README_LINK, &options).await {
        Ok(info) if info.room_id == README_ROOM_ID && info.is_live == info.selected.is_some() => {}
        Ok(info) => failures.push(format!(
            "readme: room_id {} live {}",
            info.room_id, info.is_live
        )),
        Err(kind) => failures.push(format!("readme: {kind}")),
    }
    match report(&adapter, MISSING_LINK, &options).await {
        Err(kind) if kind == "invalid_url" => {}
        other => failures.push(format!("missing id: {other:?}")),
    }
    if let Ok(live_url) = std::env::var("ZHIHU_LIVE_URL") {
        for quality in Quality::ALL {
            let options = FetchOptions {
                quality,
                ..FetchOptions::default()
            };
            match report(&adapter, &live_url, &options).await {
                Ok(info) if info.selected.is_some() => {}
                other => failures.push(format!("{quality}: no stream ({other:?})")),
            }
        }
    }
    if let Ok(offline_url) = std::env::var("ZHIHU_OFFLINE_URL") {
        match report(&adapter, &offline_url, &options).await {
            Ok(info) if !info.is_live && info.selected.is_none() => {}
            other => failures.push(format!("offline: expected not live ({other:?})")),
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
#[ignore = "requires a live Zhihu room (ZHIHU_LIVE_URL) and FFmpeg"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("ZHIHU_LIVE_URL").expect("set ZHIHU_LIVE_URL");
    let quality = Quality::parse_lenient(&std::env::var("ZHIHU_QUALITY").unwrap_or_default());
    let adapter = ZhihuAdapter::new();
    let options = FetchOptions {
        quality,
        ..FetchOptions::default()
    };
    let info = report(&adapter, &live_url, &options)
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    let (source_format, source_url) = selected.source_url(false, false);

    let output_dir = std::env::var("ZHIHU_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-zhihu-live"));
    let ffmpeg = ffmpeg_path();
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "知乎直播".to_string(),
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
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg check");
    let stderr = String::from_utf8_lossy(&check.stderr);
    // Timestamps repeated in the source stream itself are reported by the
    // null muxer although every frame decodes (as on Huya). Anything else is
    // a real error.
    const SOURCE_DTS: &str = "non monotonically increasing dts";
    // Zhihu's stream starts with an SEI (buffering period) packet that comes
    // before the first SPS, so the decoder reports it once at the very start;
    // every frame still decodes.
    const SOURCE_SEI: &str = "non-existing SPS 0 referenced in buffering period";
    let repeated = stderr
        .lines()
        .filter(|line| line.contains(SOURCE_DTS))
        .count();
    let errors: Vec<&str> = stderr
        .lines()
        .filter(|line| {
            !line.contains(SOURCE_DTS)
                && !line.contains(SOURCE_SEI)
                && !line.contains("Last message repeated")
        })
        .collect();
    println!(
        "check exit={:?} stderr_lines={} repeated_source_dts={repeated}",
        check.status.code(),
        stderr.lines().count()
    );
    assert!(check.status.success(), "ffmpeg check failed: {stderr}");
    assert!(errors.is_empty(), "ffmpeg reported errors: {errors:#?}");
}
