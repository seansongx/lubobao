//! FLV 直接下载：源是 HTTP(S) 的 FLV 时不启动 ffmpeg，用 reqwest 把响应体原样写进
//! `目录/文件名主体.flv`。对应 StreamCap 设置 `flv_use_direct_download`。
//!
//! 由 [`super::start`] 分流（见 [`applies`]），句柄、事件和结果与 ffmpeg 录制相同：
//!
//! - 开始：建目录、查磁盘、占文件名与 ffmpeg 录制共用；不分段，`pid` 为空。
//! - 进度：每秒一次，大小为已写入的字节数，时长为收到第一段数据以来经过的时间，码率由
//!   二者推算。
//! - 连接失败、HTTP 状态不是 2xx 记为网络错误；源站返回的内容不是 FLV（HTML 页面、空内容）
//!   记为直播结束，交给调度重新检测，与 ffmpeg 读到无效输入时相同。确认是 FLV 之后才创建
//!   文件，所以这些情况都不留下空文件。
//! - 源站正常结束响应体记为直播结束或断流，读取出错或超过 [`READ_TIMEOUT`] 没有数据记为
//!   网络错误；写文件失败记为需要处理的录制失败。
//! - 看门狗、录制中的磁盘检查、用户停止、[`super::stop_all`] 与 ffmpeg 录制相同，只是不必
//!   等进程收尾：立即停止读取，写完已收到的数据后关闭文件。
//! - 请求头与 ffmpeg 录制相同（没有 User-Agent 时用 [`command::DEFAULT_USER_AGENT`]），
//!   跟随重定向，不请求压缩（保存的是原始字节）。没有配置代理时直连，不用系统代理，
//!   与 ffmpeg 一致；配置的 HTTP、HTTPS、SOCKS 代理都能用（ffmpeg 只支持 HTTP 代理）。

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use chrono::Local;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, REFERER, USER_AGENT};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

use super::outcome::{self, ExitSummary};
use super::progress::{self, ErrorTail};
use super::{
    ACTIVE, EndReason, Progress, RecorderEvent, RecordingOutcome, RecordingRequest, RunState,
    SourceFormat, SpaceCheck, StopCause, Supervisor, check_space, collect_files, command, finish,
    lock,
};
use crate::platforms::{CONNECT_TIMEOUT, normalize_proxy};

/// 多久读不到数据就判定连接已断，与 ffmpeg 录制的 `-rw_timeout`（15 秒）相同。
pub const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// FLV 文件头的前三个字节。
const FLV_SIGNATURE: &[u8] = b"FLV";

/// 最多跟随的重定向次数。
const MAX_REDIRECTS: usize = 10;

/// 这个源能否直接下载：FLV，并且是 `http`、`https` 地址。
pub fn is_direct_source(format: SourceFormat, url: &str) -> bool {
    format == SourceFormat::Flv
        && reqwest::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// 这次录制是否走直接下载：开启了 [`super::RecordOptions::direct_download`]，并且
/// [`is_direct_source`]。
pub fn applies(request: &RecordingRequest) -> bool {
    request.options.direct_download && is_direct_source(request.source_format, &request.source_url)
}

/// 直接下载用的代理，以及不能用时给用户的提示：HTTP、HTTPS、SOCKS 代理照常使用，
/// 其他协议改为直连（提示与 ffmpeg 录制相同）。
pub fn recording_proxy(address: Option<String>) -> (Option<String>, Option<String>) {
    let Some(address) = address else {
        return (None, None);
    };
    match normalize_proxy(&address) {
        Ok(proxy) => (proxy, None),
        Err(_) => {
            let scheme = address.split_once("://").map_or("", |(scheme, _)| scheme);
            (
                None,
                Some(format!(
                    "录制不支持 {} 代理，已直连录制（检测仍使用代理）",
                    scheme.to_ascii_uppercase()
                )),
            )
        }
    }
}

/// 取流请求头，规则与 ffmpeg 录制相同（[`command::split_headers`]）：去掉换行和无效的键，
/// 没有 User-Agent 时用 [`command::DEFAULT_USER_AGENT`]。
pub fn request_headers(headers: &BTreeMap<String, String>) -> HeaderMap {
    let split = command::split_headers(headers);
    let mut map = HeaderMap::new();
    let user_agent = split
        .user_agent
        .as_deref()
        .unwrap_or(command::DEFAULT_USER_AGENT);
    if let Ok(value) = HeaderValue::from_str(user_agent) {
        map.insert(USER_AGENT, value);
    }
    if let Some(value) = split
        .referer
        .as_deref()
        .and_then(|referer| HeaderValue::from_str(referer).ok())
    {
        map.insert(REFERER, value);
    }
    for line in split.extra.as_deref().unwrap_or_default().split("\r\n") {
        if let Some((key, value)) = line.split_once(": ")
            && let (Ok(key), Ok(value)) = (
                HeaderName::from_bytes(key.as_bytes()),
                HeaderValue::from_str(value),
            )
        {
            map.append(key, value);
        }
    }
    map
}

/// 准备好的下载：客户端、地址和请求头。
pub struct Download {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
}

impl Download {
    /// 建客户端。代理地址无效时返回给用户看的说明。
    pub(super) fn prepare(request: &RecordingRequest) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            // 跳转后保留平台要求的 Referer，不换成跳转前的地址。
            .referer(false)
            .no_gzip();
        match normalize_proxy(request.options.proxy.as_deref().unwrap_or_default()) {
            Ok(Some(proxy)) => {
                let proxy = reqwest::Proxy::all(&proxy)
                    .map_err(|error| format!("代理地址无效 {proxy}：{error}"))?;
                builder = builder.proxy(proxy);
            }
            Ok(None) => builder = builder.no_proxy(),
            Err(error) => return Err(error.message().to_string()),
        }
        let client = builder
            .build()
            .map_err(|error| format!("无法创建下载连接：{}", describe(error)))?;
        Ok(Download {
            client,
            url: request.source_url.clone(),
            headers: request_headers(&request.headers),
        })
    }

    /// 发请求并等到响应头。失败时给出结束原因。
    async fn connect(&self) -> Result<reqwest::Response, Failure> {
        let response = self
            .client
            .get(&self.url)
            .headers(self.headers.clone())
            .send()
            .await
            .map_err(|error| Failure::network(format!("无法连接直播源：{}", describe(error))))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Failure::network(format!("直播源返回 HTTP {status}")));
        }
        Ok(response)
    }
}

/// 不是由引擎停止造成的结束。
struct Failure {
    reason: EndReason,
    message: Option<String>,
}

impl Failure {
    fn network(message: String) -> Self {
        Failure {
            reason: EndReason::NetworkError,
            message: Some(message),
        }
    }

    fn not_flv() -> Self {
        Failure {
            reason: EndReason::StreamEnded,
            message: Some("源站没有返回可录制的直播流：响应不是 FLV 数据".to_string()),
        }
    }

    fn write(message: String) -> Self {
        Failure {
            reason: EndReason::FfmpegFailed,
            message: Some(message),
        }
    }
}

/// reqwest 错误连同原因链，不带地址（地址里常有签名参数）。
fn describe(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        text.push('：');
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// 等到用户请求停止。所有句柄都已丢弃时永远等下去，录制继续到自然结束。
async fn stop_requested(stop_rx: &mut watch::Receiver<bool>) {
    if stop_rx.wait_for(|stop| *stop).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// 写入中的录像文件。确认响应是 FLV 之后才创建。
#[derive(Default)]
struct Output {
    file: Option<File>,
    /// 还不够判断是不是 FLV 的开头几个字节。
    pending: Vec<u8>,
    bytes: u64,
    /// 收到第一段 FLV 数据的时刻，时长从这里算起。
    first_data: Option<Instant>,
}

impl Output {
    async fn write(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Failure> {
        if self.file.is_some() {
            return self.append(bytes).await;
        }
        self.pending.extend_from_slice(bytes);
        if self.pending.len() < FLV_SIGNATURE.len() {
            return Ok(());
        }
        if !self.pending.starts_with(FLV_SIGNATURE) {
            return Err(Failure::not_flv());
        }
        // 名称已经由引擎占用；`create_new` 保证不覆盖期间出现的同名文件。
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
            .map_err(|error| {
                Failure::write(format!("无法创建录像文件 {}：{error}", path.display()))
            })?;
        self.file = Some(file);
        self.first_data = Some(Instant::now());
        let pending = std::mem::take(&mut self.pending);
        self.append(&pending).await
    }

    async fn append(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.write_all(bytes)
            .await
            .map_err(|error| Failure::write(format!("写入录像文件失败：{error}")))?;
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    /// 源站结束了响应体。
    fn end_of_stream(&self) -> Failure {
        if self.file.is_some() {
            return Failure {
                reason: EndReason::StreamEnded,
                message: None,
            };
        }
        if self.pending.is_empty() {
            return Failure {
                reason: EndReason::StreamEnded,
                message: Some("没有收到媒体数据，源站已结束推流".to_string()),
            };
        }
        Failure::not_flv()
    }

    fn duration_secs(&self) -> Option<f64> {
        let first = self.first_data?;
        (self.bytes > 0).then(|| (first.elapsed().as_secs_f64() * 100.0).round() / 100.0)
    }

    /// 关闭文件：等已交给系统的写入完成。没写进任何数据的文件删掉。
    async fn close(&mut self, path: &Path) {
        let Some(mut file) = self.file.take() else {
            return;
        };
        if let Err(error) = file.flush().await {
            log::warn!("关闭录像文件 {} 失败：{error}", path.display());
        }
        drop(file);
        if self.bytes == 0 {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

impl Supervisor {
    /// 直接下载的后台任务，结束时与 [`Supervisor::run`] 一样发出 `finished`。
    pub(super) async fn run_direct(mut self, download: Download) {
        let mut state = RunState {
            stop_cause: None,
            kill_deadline: None,
            last_progress: None,
            received_media: false,
            last_advance: Instant::now(),
            tail: ErrorTail::new(1),
            segment_bytes: 0,
            space_probe_failed: false,
        };
        let mut output = Output::default();
        let failure = self.transfer(&download, &mut state, &mut output).await;
        output.close(&self.output_path).await;

        let files = collect_files(&self.dir, &self.stem, &self.output_path, false).await;
        if let Some(writer) = self.subtitle.take() {
            writer.finish(!files.is_empty()).await;
        }
        let bytes_written = files.iter().map(|(_, size)| size).sum::<u64>();
        let (reason, message) = match failure {
            Some(failure) => (failure.reason, failure.message),
            None => outcome::classify(&ExitSummary {
                stop_cause: state.stop_cause,
                exit_code: None,
                received_media: bytes_written > 0,
                stderr_tail: &[],
            }),
        };
        let outcome = RecordingOutcome {
            reason,
            message,
            exit_code: None,
            files: files.into_iter().map(|(path, _)| path).collect(),
            bytes_written,
            duration_secs: output.duration_secs(),
            output_dir: Some(self.dir.clone()),
            started_at_ms: self.started_at_ms,
            ended_at_ms: Local::now().timestamp_millis(),
        };
        lock(&ACTIVE).remove(&self.id);
        finish(&self.snapshot_tx, &self.listener, outcome);
    }

    /// 连接并读取到结束。引擎停止（用户、看门狗、磁盘）时返回 `None`，停止原因在 `state` 里。
    async fn transfer(
        &mut self,
        download: &Download,
        state: &mut RunState,
        output: &mut Output,
    ) -> Option<Failure> {
        let mut stop_rx = self.stop_rx.clone();
        let connected = tokio::select! {
            connected = download.connect() => connected,
            () = stop_requested(&mut stop_rx) => {
                self.set_stop_cause(state, StopCause::User);
                return None;
            }
        };
        let mut response = match connected {
            Ok(response) => response,
            Err(failure) => return Some(failure),
        };

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_disk_check = Instant::now();
        loop {
            tokio::select! {
                chunk = response.chunk() => match chunk {
                    Ok(Some(bytes)) if bytes.is_empty() => {}
                    Ok(Some(bytes)) => {
                        state.last_advance = Instant::now();
                        if let Err(failure) = output.write(&self.output_path, &bytes).await {
                            return Some(failure);
                        }
                    }
                    Ok(None) => return Some(output.end_of_stream()),
                    Err(error) => {
                        let message = if error.is_timeout() {
                            format!("读取直播流超时：{}", describe(error))
                        } else {
                            format!("直播流读取中断：{}", describe(error))
                        };
                        return Some(Failure::network(message));
                    }
                },
                () = stop_requested(&mut stop_rx) => {
                    self.set_stop_cause(state, StopCause::User);
                    return None;
                }
                _ = tick.tick() => {
                    if let Some(writer) = self.subtitle.as_mut() {
                        writer.write_until(self.started_instant.elapsed().as_secs()).await;
                    }
                    self.report_progress(state, output);
                    let stalled = self
                        .stall_timeout
                        .is_some_and(|limit| state.last_advance.elapsed() >= limit);
                    if stalled {
                        self.set_stop_cause(state, StopCause::Stalled);
                        return None;
                    }
                    if last_disk_check.elapsed() >= self.disk_check_interval {
                        last_disk_check = Instant::now();
                        if self.direct_disk_low(state).await {
                            self.set_stop_cause(state, StopCause::DiskFull);
                            return None;
                        }
                    }
                }
            }
        }
    }

    fn report_progress(&self, state: &mut RunState, output: &Output) {
        let Some(duration_secs) = output.duration_secs() else {
            return;
        };
        let mut progress = Progress {
            size_bytes: Some(output.bytes),
            duration_secs: Some(duration_secs),
            bitrate_kbps: None,
            speed: None,
        };
        progress::fill_from_files(&mut progress, 0);
        state.received_media = true;
        state.last_progress = Some(progress.clone());
        self.snapshot_tx
            .send_modify(|snapshot| snapshot.progress = Some(progress.clone()));
        (self.listener)(&RecorderEvent::Progress { progress });
    }

    /// 录制中的磁盘检查，规则与 ffmpeg 录制相同：查询失败只在状态变化时提醒一次。
    async fn direct_disk_low(&self, state: &mut RunState) -> bool {
        match check_space(&self.probe, &self.root, self.threshold).await {
            SpaceCheck::Low(_) => true,
            SpaceCheck::Failed(message) => {
                if !state.space_probe_failed {
                    state.space_probe_failed = true;
                    (self.listener)(&RecorderEvent::Warning { message });
                }
                false
            }
            SpaceCheck::Enough | SpaceCheck::Skipped => {
                state.space_probe_failed = false;
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_flv_sources_are_downloaded_directly() {
        let flv = SourceFormat::Flv;
        assert!(is_direct_source(flv, "https://cdn/a.flv?x=1"));
        assert!(is_direct_source(flv, "HTTP://cdn/a.flv"));
        assert!(!is_direct_source(SourceFormat::Hls, "https://cdn/a.m3u8"));
        assert!(!is_direct_source(flv, "rtmp://cdn/live/a"));
        assert!(!is_direct_source(flv, "not a url"));
    }

    #[test]
    fn request_headers_match_the_ffmpeg_headers() {
        let headers = BTreeMap::from([
            ("user-agent".to_string(), " UA/1 ".to_string()),
            (
                "Referer".to_string(),
                "https://live.example.com/".to_string(),
            ),
            ("Cookie".to_string(), "a=1\r\nX-Evil: 1".to_string()),
            ("Bad:Key".to_string(), "x".to_string()),
            ("Origin".to_string(), String::new()),
        ]);
        let map = request_headers(&headers);
        assert_eq!(value(&map, "user-agent"), Some("UA/1"));
        assert_eq!(value(&map, "referer"), Some("https://live.example.com/"));
        assert_eq!(value(&map, "cookie"), Some("a=1X-Evil: 1"));
        assert_eq!(value(&map, "x-evil"), None);
        assert_eq!(value(&map, "origin"), None);
        assert_eq!(map.len(), 3);
    }

    fn value<'a>(map: &'a HeaderMap, key: &str) -> Option<&'a str> {
        map.get(key).and_then(|value| value.to_str().ok())
    }

    #[test]
    fn missing_user_agent_uses_the_ffmpeg_default() {
        let map = request_headers(&BTreeMap::new());
        assert_eq!(value(&map, "user-agent"), Some(command::DEFAULT_USER_AGENT));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn socks_and_https_proxies_are_used_as_configured() {
        assert_eq!(recording_proxy(None), (None, None));
        assert_eq!(
            recording_proxy(Some("socks5://127.0.0.1:1080".into())),
            (Some("socks5://127.0.0.1:1080".to_string()), None)
        );
        assert_eq!(
            recording_proxy(Some("https://proxy:443".into())),
            (Some("https://proxy:443".to_string()), None)
        );
        assert_eq!(
            recording_proxy(Some("127.0.0.1:7890".into())),
            (Some("http://127.0.0.1:7890".to_string()), None)
        );
        let (proxy, warning) = recording_proxy(Some("ftp://proxy:21".into()));
        assert_eq!(proxy, None);
        assert_eq!(
            warning.as_deref(),
            Some("录制不支持 FTP 代理，已直连录制（检测仍使用代理）")
        );
    }
}
