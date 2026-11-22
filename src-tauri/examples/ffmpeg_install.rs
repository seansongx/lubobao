//! 真机验证 FFmpeg 组件安装。
//!
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --example ffmpeg_install -- <本地数据目录> [--offline]
//! ```
//!
//! `--offline` 把下载地址换成无效地址，用于确认已安装的版本会被直接复用。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lubobao_lib::ffmpeg::{FfmpegManager, FfmpegStatus, ManagerConfig, run_version};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let data_dir = PathBuf::from(
        args.next()
            .expect("usage: ffmpeg_install <data_dir> [--offline]"),
    );
    let offline = args.any(|arg| arg == "--offline");

    let root = data_dir.join("components").join("ffmpeg");
    let mut config = ManagerConfig::for_current_platform(root);
    if offline && let Ok(component) = &mut config.component {
        component.package.urls = vec![
            "http://127.0.0.1:9/offline.tgz".into(),
            "https://invalid.invalid/offline.tgz".into(),
        ];
    }
    match &config.component {
        Ok(component) => println!(
            "platform {} · ffmpeg {} · package {} · sources {:?}",
            component.platform,
            component.ffmpeg_version,
            component.package_version,
            component.package.urls
        ),
        Err(reason) => println!("unsupported: {reason}"),
    }

    let started = Instant::now();
    let sources: Arc<Mutex<Vec<String>>> = Arc::default();
    let last_print = Arc::new(Mutex::new(None::<Instant>));
    let listener_sources = Arc::clone(&sources);
    let manager = FfmpegManager::new(
        config,
        Box::new(move |status: &FfmpegStatus| {
            if let FfmpegStatus::Downloading {
                downloaded,
                total,
                source,
            } = status
            {
                {
                    let mut sources = listener_sources.lock().unwrap();
                    if sources.last() != Some(source) {
                        sources.push(source.clone());
                    }
                }
                let mut last = last_print.lock().unwrap();
                let now = Instant::now();
                if last.is_none_or(|at| now - at >= Duration::from_secs(5)) || downloaded == total {
                    *last = Some(now);
                    println!(
                        "[{:>6.1}s] downloading {:.1} / {:.1} MiB from {source}",
                        started.elapsed().as_secs_f64(),
                        *downloaded as f64 / 1048576.0,
                        *total as f64 / 1048576.0
                    );
                }
            } else {
                println!(
                    "[{:>6.1}s] {}",
                    started.elapsed().as_secs_f64(),
                    serde_json::to_string(status).unwrap()
                );
            }
        }),
    );

    let status = manager.ensure().await;
    println!("elapsed {:.1}s", started.elapsed().as_secs_f64());
    println!("sources used {:?}", sources.lock().unwrap());
    match status {
        FfmpegStatus::Ready { path, .. } => {
            println!("installed at {}", path.display());
            match run_version(&path) {
                Ok(line) => println!("-version: {line}"),
                Err(err) => println!("-version failed: {err}"),
            }
        }
        other => {
            println!("not ready: {other:?}");
            std::process::exit(1);
        }
    }
}
