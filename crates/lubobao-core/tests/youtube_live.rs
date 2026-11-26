//! Live checks against real YouTube channels. Run manually:
//! `cargo test -p lubobao-core --test youtube_live -- --ignored --nocapture --test-threads=1`
//!
//! YouTube is not reachable from mainland China: set `LUBOBAO_TEST_PROXY` to
//! an HTTP proxy (for example `http://127.0.0.1:7890`). Every request then
//! goes through a local relay to that proxy, and the tests check from the
//! relay log that detection, the playlist and the media segments really used
//! it.
//!
//! YouTube asks many proxy addresses to sign in before it plays live streams.
//! Set `YOUTUBE_COOKIE` to the `Cookie` header of a logged-in youtube.com tab
//! (it is only used for the page request, like the app's platform cookie).
//!
//! Set `YOUTUBE_LIVE_URL` to a channel live page or live video to also check
//! stream selection and record about 30 seconds with FFmpeg (tier from
//! `YOUTUBE_RECORD_QUALITY`, default `SD`, so a slow proxy keeps up);
//! `YOUTUBE_OFFLINE_URL` overrides the offline channel (default `@mkbhd`).
//! The recording test reads FFmpeg from `LUBOBAO_TEST_FFMPEG` or
//! `LUBOBAO_FFMPEG`, falling back to the app's installed component under
//! `%LOCALAPPDATA%`, and keeps the file in `YOUTUBE_RECORD_DIR` (default: the
//! system temp directory).
//!
//! The stream comes from yt-dlp, which the adapter downloads with Deno on
//! first use into `YOUTUBE_TOOLS_DIR` (default: `lubobao-youtube-tools` in the
//! temp directory). `YOUTUBE_TOOLS_FRESH=1` deletes that directory first, so
//! `components_download_and_are_reused_offline` covers a download from
//! scratch. Stream URLs are printed as host only; the cookie is never printed.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, youtube::YoutubeAdapter,
};
use lubobao_core::recorder::{self, RecordOptions, RecordingRequest};
use lubobao_core::ytdlp::{self, ToolsManager};
use support::ProxyRelay;

fn tools_dir() -> PathBuf {
    std::env::var("YOUTUBE_TOOLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-youtube-tools"))
}

/// Fetch, and when the components are still downloading, wait for them and
/// fetch again.
async fn fetch_ready(
    adapter: &YoutubeAdapter,
    url: &str,
    options: &FetchOptions,
) -> Option<LiveInfo> {
    let manager = ytdlp::init(tools_dir());
    match adapter.fetch(url, options).await {
        Err(error) if error.kind() == "preparing" => {
            println!("WAIT {error}");
            if let Err(error) = manager.prepare_and_wait(options.proxy.as_deref()).await {
                println!("ERR  components: {error}");
                return None;
            }
            report(adapter, url, options).await
        }
        _ => report(adapter, url, options).await,
    }
}

/// A channel that rarely streams.
const OFFLINE_LINK: &str = "https://www.youtube.com/@mkbhd/live";
/// A handle that does not exist.
const MISSING_LINK: &str = "https://www.youtube.com/@nonexistenthandlezz9q/live";

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

async fn report(adapter: &YoutubeAdapter, url: &str, options: &FetchOptions) -> Option<LiveInfo> {
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
                    "     selected={} ({:?}) source={format:?} host={} header_keys={:?}",
                    selected.variant.name,
                    selected.requested,
                    support::url_host(source),
                    info.stream_headers.keys().collect::<Vec<_>>()
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
        cookies: std::env::var("YOUTUBE_COOKIE").ok(),
        proxy: relay.as_ref().map(|relay| relay.url.clone()),
        quality,
    }
}

#[tokio::test]
#[ignore = "requires network access to YouTube (LUBOBAO_TEST_PROXY in mainland China)"]
async fn links_resolve_against_live_youtube() {
    let relay = relay().await;
    let adapter = YoutubeAdapter::new();
    let mut failures = Vec::new();
    match adapter
        .fetch(MISSING_LINK, &options(&relay, Quality::Od))
        .await
    {
        Err(error) if error.kind() == "invalid_url" => println!("OK   {MISSING_LINK}: {error}"),
        other => failures.push(format!("missing channel: {other:?}")),
    }
    let offline_url = std::env::var("YOUTUBE_OFFLINE_URL").unwrap_or(OFFLINE_LINK.to_string());
    match report(&adapter, &offline_url, &options(&relay, Quality::Od)).await {
        Some(info) if !info.is_live && info.selected.is_none() => {}
        other => failures.push(format!("offline channel: {other:?}")),
    }
    if let Ok(live_url) = std::env::var("YOUTUBE_LIVE_URL") {
        for quality in Quality::ALL {
            let info = fetch_ready(&adapter, &live_url, &options(&relay, quality)).await;
            if info.and_then(|info| info.selected).is_none() {
                failures.push(format!("live channel {quality}: no stream selected"));
            }
        }
        // Without a cookie, a blocked proxy address gets the bot check.
        let anonymous = FetchOptions {
            cookies: None,
            ..options(&relay, Quality::Od)
        };
        match adapter.fetch(&live_url, &anonymous).await {
            Err(error) => println!("NO COOKIE {}: {error}", error.kind()),
            Ok(info) => println!(
                "NO COOKIE live={} (this address is not blocked)",
                info.is_live
            ),
        }
    }
    if let Some(relay) = &relay {
        println!("relayed: {:#?}", relay.requests());
        if !relay.saw_host_since(0, "www.youtube.com") {
            failures.push("detection did not go through the proxy".to_string());
        }
    }
    assert!(failures.is_empty(), "failures: {failures:#?}");
}

#[tokio::test]
#[ignore = "requires a live YouTube stream (YOUTUBE_LIVE_URL), FFmpeg and LUBOBAO_TEST_PROXY in mainland China"]
async fn live_stream_records_thirty_seconds() {
    let live_url = std::env::var("YOUTUBE_LIVE_URL").expect("set YOUTUBE_LIVE_URL");
    let quality = Quality::parse_lenient(
        &std::env::var("YOUTUBE_RECORD_QUALITY").unwrap_or_else(|_| "SD".to_string()),
    );
    let relay = relay().await;
    let adapter = YoutubeAdapter::new();
    let info = fetch_ready(&adapter, &live_url, &options(&relay, quality))
        .await
        .expect("fetch live stream");
    let selected = info.selected.clone().expect("stream must be live");
    let (source_format, source_url) = selected.source_url(false, false);
    let playlist_host = support::url_host(source_url);
    let segment_host = support::first_segment_host(
        source_url,
        &info.stream_headers,
        relay.as_ref().map(|relay| relay.url.as_str()),
    )
    .await;
    println!("playlist host {playlist_host}, first segment host {segment_host:?}");

    let output_dir = std::env::var("YOUTUBE_RECORD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("lubobao-youtube-live"));
    let ffmpeg = support::ffmpeg_path();
    let relayed_before = relay.as_ref().map_or(0, ProxyRelay::count);
    let request = RecordingRequest {
        ffmpeg_path: ffmpeg.clone(),
        source_url: source_url.to_string(),
        source_format,
        headers: info.stream_headers.clone(),
        output_dir,
        anchor_name: info.anchor_name.clone(),
        platform: "YouTube".to_string(),
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

#[tokio::test]
#[ignore = "downloads yt-dlp and Deno from GitHub (LUBOBAO_TEST_PROXY in mainland China)"]
async fn components_download_and_are_reused_offline() {
    let dir = tools_dir();
    if std::env::var("YOUTUBE_TOOLS_FRESH").as_deref() == Ok("1") {
        let _ = std::fs::remove_dir_all(&dir);
        println!("removed {}", dir.display());
    }
    let relay = relay().await;
    let proxy = relay.as_ref().map(|relay| relay.url.clone());
    let started = Instant::now();
    let manager = Arc::new(ToolsManager::new(dir.clone(), ytdlp::manifest::current()));
    let paths = manager
        .prepare_and_wait(proxy.as_deref())
        .await
        .expect("components install");
    println!(
        "ready in {:.1} s: {} {}",
        started.elapsed().as_secs_f64(),
        paths.ytdlp.display(),
        paths.deno.display()
    );
    if let Some(relay) = &relay {
        println!("relayed: {:?}", relay.hosts_since(0));
    }
    assert!(paths.ytdlp.is_file() && paths.deno.is_file());

    // A second manager on the same directory reuses the installed version
    // without any network access (the proxy here does not exist).
    let offline = Arc::new(ToolsManager::new(dir, ytdlp::manifest::current()));
    let started = Instant::now();
    let reused = offline
        .prepare(Some("http://127.0.0.1:9"))
        .expect("reuse without network");
    println!("reused in {} ms", started.elapsed().as_millis());
    assert_eq!(reused, paths);
}
