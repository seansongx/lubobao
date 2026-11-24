//! Live checks against real Kugou rooms. Run manually:
//! `cargo test -p lubobao-core --test kugou_live -- --ignored --nocapture --test-threads=1`
//!
//! Set `KUGOU_LIVE_URL` to a room that is currently live to also check
//! stream selection and record about 30 seconds with FFmpeg. The recording
//! test reads FFmpeg from `LUBOBAO_TEST_FFMPEG` or `LUBOBAO_FFMPEG`, falling
//! back to the app's installed component under `%LOCALAPPDATA%`, and keeps
//! the file in `KUGOU_RECORD_DIR` (default: the system temp directory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, kugou::KugouAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};

/// StreamCap README example (usually offline).
const README_LINK: &str = "https://fanxing2.kugou.com/50428671?refer=2177&sourceFrom=";
const README_ROOM_ID: &str = "50428671";
/// A room id with no anchor.
const MISSING_LINK: &str = "https://fanxing.kugou.com/1";

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
    adapter: &KugouAdapter,
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
#[ignore = "requires network access to Kugou"]
async fn links_resolve_against_live_kugou() {
    let adapter = KugouAdapter::new();
    let options = FetchOptions::default();
    let mut failures = Vec::new();
    // The README anchor may happen to be live; only identity is checked.
    match report(&adapter, README_LINK, &options).await {
        Ok(info)
            if info.room_id == README_ROOM_ID
                && info.anchor_id.is_some()
                && !info.anchor_name.is_empty()
                && info.is_live == info.selected.is_some() => {}
        Ok(info) => failures.push(format!(
            "readme: room_id {} anchor_id {:?} live {}",
            info.room_id, info.anchor_id, info.is_live
        )),
        Err(kind) => failures.push(format!("readme: {kind}")),
    }
    match report(&adapter, MISSING_LINK, &options).await {
        Err(kind) if kind == "invalid_url" => {}
        other => failures.push(format!("missing room: {other:?}")),
    }
    if let Ok(live_url) = std::env::var("KUGOU_LIVE_URL") {
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
#[ignore = "requires a live Kugou room (KUGOU_LIVE_URL) and FFmpeg"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("KUGOU_LIVE_URL").expect("set KUGOU_LIVE_URL");
    let quality = Quality::parse_lenient(&std::env::var("KUGOU_QUALITY").unwrap_or_default());
    let adapter = KugouAdapter::new();
    let options = FetchOptions {
        quality,
        ..FetchOptions::default()
    };
    let info = report(&adapter, &live_url, &options)
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    // Kugou only offers FLV; record_url is the FLV stream.
    let (source_format, source_url) = selected.source_url(false, false);

    let output_dir = std::env::var("KUGOU_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-kugou-live"));
    let ffmpeg = ffmpeg_path();
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "酷狗直播".to_string(),
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

    let file = &outcome.files[0];
    let check = std::process::Command::new(&ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg check");
    let stderr = String::from_utf8_lossy(&check.stderr);
    // Timestamps repeated in the source FLV itself are reported by the null
    // muxer although every frame decodes (as on Huya).
    const SOURCE_DTS: &str = "non monotonically increasing dts";
    let repeated = stderr
        .lines()
        .filter(|line| line.contains(SOURCE_DTS))
        .count();
    // Kugou puts an SEI after the slice of every frame. In MPEG-TS that SEI
    // travels with the next frame, so the last frame's SEI ends up alone as
    // the file's final packet (remuxing a clean FLV to TS does the same).
    // The decoder reports that picture-less packet once; every frame still
    // decodes. These lines are accepted only when the last video packet is
    // such a small lone packet and each appears at most once.
    const LONE_SEI: [&str; 3] = [
        "missing picture in access unit",
        "no frame!",
        "Decoding error: Invalid data found when processing input",
    ];
    let packets = video_packet_sizes(&ffmpeg, file, "copy");
    let frames = video_packet_sizes(&ffmpeg, file, "decode").len();
    let lone_tail = packets.last().is_some_and(|size| *size < 1024);
    let lone_lines = stderr
        .lines()
        .filter(|line| LONE_SEI.iter().any(|known| line.contains(known)))
        .count();
    let errors: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.contains(SOURCE_DTS))
        .filter(|line| !(lone_tail && LONE_SEI.iter().any(|known| line.contains(known))))
        .collect();
    println!(
        "check exit={:?} stderr_lines={} repeated_source_dts={repeated} video_packets={} decoded_frames={frames} last_packet={:?} lone_sei_lines={lone_lines}",
        check.status.code(),
        stderr.lines().count(),
        packets.len(),
        packets.last()
    );
    assert!(check.status.success(), "ffmpeg check failed: {stderr}");
    assert!(errors.is_empty(), "ffmpeg reported errors: {errors:#?}");
    assert!(
        lone_lines <= LONE_SEI.len(),
        "lone SEI reported more than once"
    );
    // Every packet but a lone trailing SEI decodes to a frame.
    let expected = packets.len() - usize::from(lone_tail);
    assert!(frames >= expected, "decoded {frames} of {expected} frames");
}

/// Sizes of the video packets (`copy`) or decoded frames (`decode`) from
/// FFmpeg's `framecrc` output.
fn video_packet_sizes(ffmpeg: &PathBuf, file: &std::path::Path, mode: &str) -> Vec<u64> {
    let mut command = std::process::Command::new(ffmpeg);
    command
        .args(["-v", "quiet", "-i"])
        .arg(file)
        .args(["-map", "0:v"]);
    if mode == "copy" {
        command.args(["-c", "copy"]);
    } else {
        // Keep frames whose repeated source timestamps the muxer would drop.
        command.args(["-fps_mode", "passthrough", "-enc_time_base", "demux"]);
    }
    let output = command
        .args(["-f", "framecrc", "-"])
        .output()
        .expect("run ffmpeg framecrc");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split(',').nth(4)?.trim().parse().ok())
        .collect()
}
