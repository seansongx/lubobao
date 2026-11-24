//! Live checks against real TikTok rooms. Run manually:
//! `cargo test -p lubobao-core --test tiktok_live -- --ignored --nocapture --test-threads=1`
//!
//! TikTok is not reachable from mainland China: set `LUBOBAO_TEST_PROXY` to
//! an HTTP proxy (for example `http://127.0.0.1:7890`). Every request then
//! goes through a local relay to that proxy, and the tests check from the
//! relay log that detection and the recording really used it.
//!
//! Set `TIKTOK_LIVE_URL` to a user who is live to also check stream selection
//! and record about 30 seconds with FFmpeg; `TIKTOK_OFFLINE_URL` overrides the
//! offline user (default `@tiktok`). The recording test reads FFmpeg from
//! `LUBOBAO_TEST_FFMPEG` or `LUBOBAO_FFMPEG`, falling back to the app's
//! installed component under `%LOCALAPPDATA%`, and keeps the file in
//! `TIKTOK_RECORD_DIR` (default: the system temp directory).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, tiktok::TiktokAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};
use support::ProxyRelay;

/// TikTok's own account, normally not live.
const OFFLINE_LINK: &str = "https://www.tiktok.com/@tiktok/live";
/// A user name that does not exist.
const MISSING_LINK: &str = "https://www.tiktok.com/@nonexistentuserzz9q8/live";

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

async fn report(adapter: &TiktokAdapter, url: &str, options: &FetchOptions) -> Option<LiveInfo> {
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
                let (format, source) = selected.source_url(true, false);
                println!(
                    "     selected={} ({:?}) source={format:?} {source}",
                    selected.variant.name, selected.requested
                );
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

async fn relay() -> Option<ProxyRelay> {
    let relay = ProxyRelay::start(&support::upstream_proxy()?).await;
    println!(
        "proxy relay {} -> {:?}",
        relay.url,
        support::upstream_proxy()
    );
    Some(relay)
}

fn options(relay: &Option<ProxyRelay>, quality: Quality) -> FetchOptions {
    FetchOptions {
        proxy: relay.as_ref().map(|relay| relay.url.clone()),
        quality,
        ..FetchOptions::default()
    }
}

#[tokio::test]
#[ignore = "requires network access to TikTok (LUBOBAO_TEST_PROXY in mainland China)"]
async fn links_resolve_against_live_tiktok() {
    let relay = relay().await;
    let adapter = TiktokAdapter::new();
    let mut failures = Vec::new();
    let offline_url = std::env::var("TIKTOK_OFFLINE_URL").unwrap_or(OFFLINE_LINK.to_string());
    match report(&adapter, &offline_url, &options(&relay, Quality::Od)).await {
        Some(info) if !info.is_live && info.selected.is_none() => {}
        other => failures.push(format!("offline room: {other:?}")),
    }
    match adapter
        .fetch(MISSING_LINK, &options(&relay, Quality::Od))
        .await
    {
        Err(error) if error.kind() == "invalid_url" => println!("OK   {MISSING_LINK}: {error}"),
        other => failures.push(format!("missing user: {other:?}")),
    }
    if let Ok(live_url) = std::env::var("TIKTOK_LIVE_URL") {
        for quality in [Quality::Od, Quality::Hd, Quality::Ld] {
            let info = report(&adapter, &live_url, &options(&relay, quality)).await;
            if info.and_then(|info| info.selected).is_none() {
                failures.push(format!("live room {quality}: no stream selected"));
            }
        }
    }
    if let Some(relay) = &relay {
        println!("relayed: {:#?}", relay.requests());
        if !relay.saw_host_since(0, "www.tiktok.com") {
            failures.push("detection did not go through the proxy".to_string());
        }
    }
    assert!(failures.is_empty(), "failures: {failures:#?}");
}

#[tokio::test]
#[ignore = "requires a live TikTok user (TIKTOK_LIVE_URL), FFmpeg and LUBOBAO_TEST_PROXY in mainland China"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("TIKTOK_LIVE_URL").expect("set TIKTOK_LIVE_URL");
    let relay = relay().await;
    let adapter = TiktokAdapter::new();
    let info = report(&adapter, &live_url, &options(&relay, Quality::Od))
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    // Like the scheduler: TikTok is FLV-preferred (StreamCap
    // `is_flv_preferred_platform`).
    let (source_format, source_url) = selected.source_url(true, false);
    let source_host = support::url_host(source_url);

    let output_dir = std::env::var("TIKTOK_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-tiktok-live"));
    let ffmpeg = support::ffmpeg_path();
    let relayed_before = relay.as_ref().map_or(0, ProxyRelay::count);
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "TikTok".to_string(),
        options: RecordOptions {
            segment_enabled: false,
            proxy: relay.as_ref().map(|relay| relay.url.clone()),
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
    if let Some(relay) = &relay {
        let hosts = relay.hosts_since(relayed_before);
        println!("recording relayed: {hosts:?}");
        assert!(
            hosts.contains(&source_host),
            "recording of {source_host} did not go through the proxy: {hosts:?}"
        );
    }
    assert_eq!(outcome.files.len(), 1, "one recording file");
    assert!(outcome.bytes_written > 0);
    support::describe_file(&ffmpeg, &outcome.files[0]);
    support::assert_decodes(&ffmpeg, &outcome.files[0]);
}
