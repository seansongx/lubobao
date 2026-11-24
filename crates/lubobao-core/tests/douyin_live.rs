//! Live checks against real Douyin rooms. Run manually:
//! `cargo test -p lubobao-core --test douyin_live -- --ignored --nocapture`
//! Set `DOUYIN_LIVE_URL` to a room that is currently live to also check stream selection.

use std::time::Instant;

use lubobao_core::platforms::{
    FetchOptions, LiveInfo, PlatformAdapter, Quality, douyin::DouyinAdapter,
};

/// The five README link types, each with the room id (web_rid or 抖音号) it must resolve to.
const README_LINKS: [(&str, &str, &str); 8] = [
    (
        "web room from category page (live_web_rid)",
        "https://live.douyin.com/?anchor_id=2292900791589195&category_name=all&is_vs=0&live_web_rid=228052615835&page_type=main_category_page&vs_ep_group_id=&vs_episode_id=&vs_episode_stage=&vs_season_id=",
        "228052615835",
    ),
    (
        "web room",
        "https://live.douyin.com/745964462470",
        "745964462470",
    ),
    (
        "app room short link",
        "https://v.douyin.com/iQFeBnt/",
        "326500301367",
    ),
    ("douyin id", "https://live.douyin.com/yall1102", "yall1102"),
    (
        "app profile short link",
        "https://v.douyin.com/CeiU5cbX",
        "94014833191",
    ),
    (
        "web profile",
        "https://www.douyin.com/user/MS4wLjABAAAA3kr2yA4aRD-sjf9cx8xkOH8Di3RjktpKcAvqIetpsF0",
        "panmin76",
    ),
    (
        "main site room (root/live)",
        "https://www.douyin.com/root/live/745964462470",
        "745964462470",
    ),
    (
        "share landing profile (iesdouyin)",
        "https://www.iesdouyin.com/share/user/MS4wLjABAAAA3kr2yA4aRD-sjf9cx8xkOH8Di3RjktpKcAvqIetpsF0?from_ssr=1",
        "panmin76",
    ),
];

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

async fn report(adapter: &DouyinAdapter, url: &str, options: &FetchOptions) -> Option<LiveInfo> {
    let started = Instant::now();
    let result = adapter.fetch(url, options).await;
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(info) => {
            println!("OK   {url}");
            println!(
                "     room_id={} anchor_id={:?} anchor={} live={} title={:?} tiers=[{}] {elapsed} ms",
                info.room_id,
                info.anchor_id,
                info.anchor_name,
                info.is_live,
                info.title,
                describe_tiers(&info)
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
#[ignore = "requires network access to Douyin"]
async fn readme_links_resolve_against_live_douyin() {
    let adapter = DouyinAdapter::new();
    let options = FetchOptions::default();
    let mut failures = Vec::new();
    for (kind, url, expected_room_id) in README_LINKS {
        match report(&adapter, url, &options).await {
            Some(info) if info.room_id == expected_room_id && !info.anchor_name.is_empty() => {}
            Some(info) => failures.push(format!(
                "{kind}: room_id {} != {expected_room_id}",
                info.room_id
            )),
            None => failures.push(format!("{kind}: fetch failed")),
        }
    }
    if let Ok(live_url) = std::env::var("DOUYIN_LIVE_URL") {
        for quality in [Quality::Od, Quality::Hd, Quality::Ld] {
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
