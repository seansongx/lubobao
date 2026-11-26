//! Live checks against real SOOP rooms. Run manually:
//! `cargo test -p lubobao-core --test sooplive_live -- --ignored --nocapture --test-threads=1`
//!
//! Requests go direct by default. In mainland China set `LUBOBAO_TEST_PROXY` to
//! an HTTP proxy (for example `http://127.0.0.1:7890`). Every request then
//! goes through a local relay to that proxy, and the tests check from the
//! relay log that detection, the playlist and the media segments really used
//! it.
//!
//! Set `SOOP_LIVE_URL` to a room that is live to also check stream
//! selection and record about 30 seconds with FFmpeg (tier from
//! `SOOP_RECORD_QUALITY`, default `SD`, so a slow proxy keeps up);
//! `SOOP_OFFLINE_URL` to a room that is not live. The recording test
//! reads FFmpeg from `LUBOBAO_TEST_FFMPEG` or `LUBOBAO_FFMPEG`, falling back
//! to the app's installed component under `%LOCALAPPDATA%`, and keeps the
//! file in `SOOP_RECORD_DIR` (default: the system temp directory).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, sooplive::SoopAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};
use support::ProxyRelay;

/// A room name that does not exist.
const MISSING_LINK: &str = "https://play.sooplive.com/zzzznotexist1";

fn describe_tiers(info: &LiveInfo) -> String {
    info.variants
        .iter()
        .map(|variant| {
            let quality = variant.quality.map(|quality| quality.code()).unwrap_or("-");
            format!("{}={}", quality, variant.name)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

async fn report(adapter: &SoopAdapter, url: &str, options: &FetchOptions) -> Option<LiveInfo> {
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
                    "     selected={} ({:?}) source={format:?} {}…",
                    selected.variant.name,
                    selected.requested,
                    &source[..source.len().min(90)]
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
#[ignore = "requires network access to SOOP"]
async fn links_resolve_against_live_sooplive() {
    let relay = relay().await;
    let adapter = SoopAdapter::new();
    let mut failures = Vec::new();
    match adapter
        .fetch(MISSING_LINK, &options(&relay, Quality::Od))
        .await
    {
        Err(error) if error.kind() == "invalid_url" => println!("OK   {MISSING_LINK}: {error}"),
        other => failures.push(format!("missing room: {other:?}")),
    }
    if let Ok(offline_url) = std::env::var("SOOP_OFFLINE_URL") {
        match report(&adapter, &offline_url, &options(&relay, Quality::Od)).await {
            Some(info) if !info.is_live && info.selected.is_none() => {}
            other => failures.push(format!("offline room: {other:?}")),
        }
    }
    if let Ok(live_url) = std::env::var("SOOP_LIVE_URL") {
        for quality in Quality::ALL {
            let info = report(&adapter, &live_url, &options(&relay, quality)).await;
            if info.and_then(|info| info.selected).is_none() {
                failures.push(format!("live room {quality}: no stream selected"));
            }
        }
    }
    if let Some(relay) = &relay {
        println!("relayed: {:#?}", relay.requests());
        if !relay.saw_host_since(0, "st.sooplive.com") {
            failures.push("detection did not go through the proxy".to_string());
        }
    }
    assert!(failures.is_empty(), "failures: {failures:#?}");
}

#[tokio::test]
#[ignore = "requires a live SOOP room (SOOP_LIVE_URL), FFmpeg"]
async fn live_room_records_thirty_seconds() {
    let live_url = std::env::var("SOOP_LIVE_URL").expect("set SOOP_LIVE_URL");
    let quality = Quality::parse_lenient(
        &std::env::var("SOOP_RECORD_QUALITY").unwrap_or_else(|_| "SD".to_string()),
    );
    let relay = relay().await;
    let adapter = SoopAdapter::new();
    let info = report(&adapter, &live_url, &options(&relay, quality))
        .await
        .expect("fetch live room");
    let selected = info.selected.clone().expect("room must be live");
    let (source_format, source_url) = selected.source_url(false, false);
    let playlist_host = support::url_host(source_url);
    let segment_host = support::first_segment_host(
        source_url,
        &info.stream_headers,
        relay.as_ref().map(|relay| relay.url.as_str()),
    )
    .await;
    println!("playlist host {playlist_host}, first segment host {segment_host:?}");

    let output_dir = std::env::var("SOOP_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-sooplive-live"));
    let ffmpeg = support::ffmpeg_path();
    let relayed_before = relay.as_ref().map_or(0, ProxyRelay::count);
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "SOOP".to_string(),
        title: None,
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
        println!("recording relayed {} connections: {hosts:?}", hosts.len());
        assert!(
            hosts.contains(&playlist_host),
            "playlist {playlist_host} did not go through the proxy: {hosts:?}"
        );
        if let Some(segment_host) = &segment_host {
            assert!(
                hosts.contains(segment_host),
                "segments from {segment_host} did not go through the proxy: {hosts:?}"
            );
        }
    }
    assert_eq!(outcome.files.len(), 1, "one recording file");
    assert!(outcome.bytes_written > 0);
    support::describe_file(&ffmpeg, &outcome.files[0]);
    support::assert_decodes(&ffmpeg, &outcome.files[0]);
}
