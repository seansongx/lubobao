//! Live checks against real Douyin rooms. Run manually:
//! `cargo test --manifest-path src-tauri/Cargo.toml --test douyin_live -- --ignored --nocapture`
//! Set `DOUYIN_LIVE_URL` to a room that is currently live to also check stream selection.

use std::time::Instant;

use lubobao_lib::platforms::{FetchOptions, PlatformAdapter, Quality, douyin::DouyinAdapter};

const README_LINKS: [&str; 5] = [
    "https://live.douyin.com/745964462470",
    "https://v.douyin.com/iQFeBnt/",
    "https://live.douyin.com/yall1102",
    "https://v.douyin.com/CeiU5cbX",
    "https://www.douyin.com/user/MS4wLjABAAAA3kr2yA4aRD-sjf9cx8xkOH8Di3RjktpKcAvqIetpsF0",
];

async fn report(adapter: &DouyinAdapter, url: &str, options: &FetchOptions) -> bool {
    let started = Instant::now();
    let result = adapter.fetch(url, options).await;
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(info) => {
            let tiers: Vec<String> = info
                .variants
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
                .collect();
            println!(
                "OK   {url}\n     room_id={} anchor={} live={} title={:?} tiers=[{}] {elapsed} ms",
                info.room_id,
                info.anchor_name,
                info.is_live,
                info.title,
                tiers.join(", ")
            );
            if let Some(selected) = &info.selected {
                let (format, source) = selected.source_url(true, false);
                println!(
                    "     selected={} ({:?}) source={format:?} {source}",
                    selected.variant.name, selected.requested
                );
                println!(
                    "     hls={}",
                    selected.variant.hls_url.as_deref().unwrap_or("-")
                );
                println!("     headers={:?}", info.stream_headers);
            }
            true
        }
        Err(error) => {
            println!("ERR  {url}\n     {error} {elapsed} ms");
            false
        }
    }
}

#[tokio::test]
#[ignore = "requires network access to Douyin"]
async fn readme_links_resolve_against_live_douyin() {
    let adapter = DouyinAdapter::new();
    let options = FetchOptions::default();
    let mut ok = 0;
    for url in README_LINKS {
        if report(&adapter, url, &options).await {
            ok += 1;
        }
    }
    if let Ok(live_url) = std::env::var("DOUYIN_LIVE_URL") {
        for quality in [Quality::Od, Quality::Hd, Quality::Ld] {
            let options = FetchOptions {
                quality,
                ..FetchOptions::default()
            };
            assert!(
                report(&adapter, &live_url, &options).await,
                "live room failed"
            );
        }
    }
    assert!(ok >= 1, "no README link resolved");
}
