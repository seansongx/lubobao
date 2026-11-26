//! Live checks against real Twitch channels. Run manually:
//! `cargo test -p lubobao-core --test twitch_live -- --ignored --nocapture --test-threads=1`
//!
//! Twitch is not reachable from mainland China: set `LUBOBAO_TEST_PROXY` to
//! an HTTP proxy (for example `http://127.0.0.1:7890`). Every request then
//! goes through a local relay to that proxy, and the tests check from the
//! relay log that detection, the playlist and the media segments really used
//! it.
//!
//! Set `TWITCH_LIVE_URL` to a channel that is live to also check stream
//! selection and record about 30 seconds with FFmpeg (tier from
//! `TWITCH_RECORD_QUALITY`, default `SD`, so a slow proxy keeps up);
//! `TWITCH_OFFLINE_URL` to a channel that is not live. The recording test
//! reads FFmpeg from `LUBOBAO_TEST_FFMPEG` or `LUBOBAO_FFMPEG`, falling back
//! to the app's installed component under `%LOCALAPPDATA%`, and keeps the
//! file in `TWITCH_RECORD_DIR` (default: the system temp directory).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, twitch::TwitchAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};
use support::ProxyRelay;

/// A channel name that does not exist.
const MISSING_LINK: &str = "https://www.twitch.tv/nonexistentchannelzz9q";

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

async fn report(adapter: &TwitchAdapter, url: &str, options: &FetchOptions) -> Option<LiveInfo> {
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
#[ignore = "requires network access to Twitch (LUBOBAO_TEST_PROXY in mainland China)"]
async fn links_resolve_against_live_twitch() {
    let relay = relay().await;
    let adapter = TwitchAdapter::new();
    let mut failures = Vec::new();
    match adapter
        .fetch(MISSING_LINK, &options(&relay, Quality::Od))
        .await
    {
        Err(error) if error.kind() == "invalid_url" => println!("OK   {MISSING_LINK}: {error}"),
        other => failures.push(format!("missing channel: {other:?}")),
    }
    if let Ok(offline_url) = std::env::var("TWITCH_OFFLINE_URL") {
        match report(&adapter, &offline_url, &options(&relay, Quality::Od)).await {
            Some(info) if !info.is_live && info.selected.is_none() => {}
            other => failures.push(format!("offline channel: {other:?}")),
        }
    }
    if let Ok(live_url) = std::env::var("TWITCH_LIVE_URL") {
        for quality in Quality::ALL {
            let info = report(&adapter, &live_url, &options(&relay, quality)).await;
            if info.and_then(|info| info.selected).is_none() {
                failures.push(format!("live channel {quality}: no stream selected"));
            }
        }
    }
    if let Some(relay) = &relay {
        println!("relayed: {:#?}", relay.requests());
        if !relay.saw_host_since(0, "gql.twitch.tv") {
            failures.push("detection did not go through the proxy".to_string());
        }
    }
    assert!(failures.is_empty(), "failures: {failures:#?}");
}

#[tokio::test]
#[ignore = "requires a live Twitch channel (TWITCH_LIVE_URL), FFmpeg and LUBOBAO_TEST_PROXY in mainland China"]
async fn live_channel_records_thirty_seconds() {
    let live_url = std::env::var("TWITCH_LIVE_URL").expect("set TWITCH_LIVE_URL");
    let quality = Quality::parse_lenient(
        &std::env::var("TWITCH_RECORD_QUALITY").unwrap_or_else(|_| "SD".to_string()),
    );
    let relay = relay().await;
    let adapter = TwitchAdapter::new();
    let info = report(&adapter, &live_url, &options(&relay, quality))
        .await
        .expect("fetch live channel");
    let selected = info.selected.clone().expect("channel must be live");
    let (source_format, source_url) = selected.source_url(false, false);
    let playlist_host = support::url_host(source_url);
    let segment_host = support::first_segment_host(
        source_url,
        &info.stream_headers,
        relay.as_ref().map(|relay| relay.url.as_str()),
    )
    .await;
    println!("playlist host {playlist_host}, first segment host {segment_host:?}");

    let output_dir = std::env::var("TWITCH_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-twitch-live"));
    let ffmpeg = support::ffmpeg_path();
    let relayed_before = relay.as_ref().map_or(0, ProxyRelay::count);
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "Twitch".to_string(),
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
