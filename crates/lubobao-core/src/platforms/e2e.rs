//! 端到端测试用的平台适配器，只在 feature `e2e` 下编译，发布版二进制里没有。
//!
//! 链接写成 `e2e+http://127.0.0.1:<端口>/<路径>`：对去掉 `e2e+` 前缀的 HTTP 地址发一次
//! `HEAD` 请求，返回 2xx 视为正在直播，流地址就是这个 HTTP 地址（按 FLV 录制）；返回其他
//! 状态或连接不上视为未开播。样本流由测试用 ffmpeg 生成，再由测试里的本地 HTTP 服务提供。

use std::collections::BTreeMap;
use std::time::Duration;

use super::{
    BoxFuture, FetchOptions, LiveInfo, Platform, PlatformAdapter, PlatformError, Quality,
    StreamVariant, selected_stream,
};

/// 测试链接的前缀。
pub const SCHEME_PREFIX: &str = "e2e+";

pub struct E2eAdapter;

fn http_url(url: &str) -> Option<&str> {
    let rest = url.trim().strip_prefix(SCHEME_PREFIX)?;
    rest.starts_with("http://").then_some(rest)
}

impl PlatformAdapter for E2eAdapter {
    fn platform(&self) -> Platform {
        Platform::E2e
    }

    fn matches(&self, url: &str) -> bool {
        http_url(url).is_some()
    }

    fn normalize_url(&self, url: &str) -> Option<String> {
        http_url(url).map(str::to_string)
    }

    fn fetch<'a>(
        &'a self,
        url: &'a str,
        options: &'a FetchOptions,
    ) -> BoxFuture<'a, Result<LiveInfo, PlatformError>> {
        Box::pin(async move {
            let stream = http_url(url)
                .ok_or_else(|| PlatformError::InvalidUrl(format!("不是测试链接：{url}")))?;
            // 只连本机，不走系统代理。
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|error| PlatformError::Network(error.to_string()))?;
            let is_live = match client.head(stream).send().await {
                Ok(response) => response.status().is_success(),
                Err(error) if error.is_connect() => false,
                Err(error) => return Err(PlatformError::Network(error.to_string())),
            };
            let room_id = stream
                .trim_start_matches("http://")
                .split_once('/')
                .map(|(_, path)| path.to_string())
                .unwrap_or_default();
            let variant = StreamVariant {
                name: "ORIGIN".into(),
                quality: Some(Quality::Od),
                flv_url: Some(stream.to_string()),
                hls_url: None,
            };
            let selected = if is_live {
                selected_stream(options.quality, &variant)
            } else {
                None
            };
            Ok(LiveInfo {
                platform: Platform::E2e,
                room_id: room_id.clone(),
                anchor_id: Some(format!("e2e:{room_id}")),
                live_url: None,
                anchor_name: "测试主播".into(),
                title: is_live.then(|| "端到端测试直播".to_string()),
                is_live,
                variants: if is_live { vec![variant] } else { Vec::new() },
                selected,
                stream_headers: BTreeMap::new(),
            })
        })
    }
}
