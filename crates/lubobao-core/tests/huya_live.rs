//! Live checks against real Huya rooms. Run manually:
//! `cargo test -p lubobao-core --test huya_live -- --ignored --nocapture --test-threads=1`
//!
//! Set `HUYA_LIVE_URL` to a room that is currently live to also check stream
//! selection and record about 30 seconds with FFmpeg (`HUYA_LOL_URL` adds a
//! live 英雄联盟 room, which goes through the web page and the anti-leech
//! parameters). The recording test reads FFmpeg from `LUBOBAO_TEST_FFMPEG`
//! or `LUBOBAO_FFMPEG`, falling back to the app's installed component under
//! `%LOCALAPPDATA%`, and keeps the file in `HUYA_RECORD_DIR` (default: the
//! system temp directory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, huya::HuyaAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};

/// StreamCap README example and the ids it must resolve to.
const README_LINK: &str = "https://www.huya.com/52333";
const README_ROOM_ID: &str = "52333";
const README_ANCHOR_ID: &str = "1356299405";
/// A letter id and the room it resolves to.
const ALIAS_LINK: &str = "https://www.huya.com/lpl";
const ALIAS_ROOM_ID: &str = "660000";
/// A room number that does not exist.
const MISSING_LINK: &str = "https://www.huya.com/999999999";

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
    adapter: &HuyaAdapter,
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
#[ignore = "requires network access to Huya"]
async fn links_resolve_against_live_huya() {
    let adapter = HuyaAdapter::new();
    let options = FetchOptions::default();
    let mut failures = Vec::new();
    match report(&adapter, README_LINK, &options).await {
        Ok(info)
            if info.room_id == README_ROOM_ID
                && info.anchor_id.as_deref() == Some(README_ANCHOR_ID)
                && !info.anchor_name.is_empty() => {}
        Ok(info) => failures.push(format!(
            "readme link: room_id {} anchor_id {:?}",
            info.room_id, info.anchor_id
        )),
        Err(kind) => failures.push(format!("readme link: {kind}")),
    }
    match report(&adapter, ALIAS_LINK, &options).await {
        Ok(info) if info.room_id == ALIAS_ROOM_ID => {}
        Ok(info) => failures.push(format!("alias link: room_id {}", info.room_id)),
        Err(kind) => failures.push(format!("alias link: {kind}")),
    }
    match report(&adapter, MISSING_LINK, &options).await {
        Err(kind) if kind == "invalid_url" => {}
        other => failures.push(format!("missing room: {other:?}")),
    }
    for (var, live_url) in ["HUYA_LIVE_URL", "HUYA_LOL_URL"]
        .into_iter()
        .filter_map(|var| std::env::var(var).ok().map(|url| (var, url)))
    {
        for quality in Quality::ALL {
            let options = FetchOptions {
                quality,
                ..FetchOptions::default()
            };
            match report(&adapter, &live_url, &options).await {
                Ok(info) if info.selected.is_some() => {
                    if let Err(error) = check_flv_header(&info).await {
                        failures.push(format!("{var} {quality}: {error}"));
                    }
                }
                other => failures.push(format!("{var} {quality}: no stream ({other:?})")),
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
#[ignore = "requires a live Huya room (HUYA_LIVE_URL) and FFmpeg"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("HUYA_LIVE_URL").expect("set HUYA_LIVE_URL");
    let quality = Quality::parse_lenient(&std::env::var("HUYA_QUALITY").unwrap_or_default());
    let adapter = HuyaAdapter::new();
    let options = FetchOptions {
        quality,
        ..FetchOptions::default()
    };
    let info = report(&adapter, &live_url, &options)
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    // Huya is not an FLV-preferred platform; record_url is the FLV stream.
    let (source_format, source_url) = selected.source_url(false, false);

    let output_dir = std::env::var("HUYA_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-huya-live"));
    let ffmpeg = ffmpeg_path();
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "虎牙直播".to_string(),
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
    // Huya's transcoded tiers (`ratio=…`) repeat a few audio timestamps in
    // the source FLV itself; the null muxer reports them but every frame
    // decodes. Anything else is a real error.
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
