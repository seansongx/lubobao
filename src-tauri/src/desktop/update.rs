//! 应用内自动更新：检查 xiaofany.com 的 Tauri 升级接口，下载并校验签名，收尾录制后安装、重启。
//!
//! - 接口和公钥在 `tauri.conf.json` 的 `plugins.updater`。公钥还是占位符时（开发构建、没配
//!   公钥的构建）一律报「未配置」，不发请求。
//! - 请求带 `X-Client-Id`（本机随机生成，存在应用数据目录的 `client-id`），服务端据此灰度。
//! - 签名在下载完成时由插件校验（[`tauri_plugin_updater::Update::download`]），安装前再校验
//!   一次；不通过时拒绝安装。
//! - 安装前与退出相同地收尾（[`crate::scheduler::tauri_glue::finish_before_update`]）：停止调度和
//!   全部录制、等文件收尾。Windows 的 NSIS 安装器会结束应用，所以必须先收尾。是否有录制由
//!   前端先确认。
//! - Linux 只有 AppImage 能就地更新；deb 等其他安装方式只提示到官网下载。
//! - 调试构建可以用环境变量 `LUBOBAO_UPDATER_ENDPOINT` 换成本地测试服务器。

use std::fs;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{Emitter, Manager};
use tauri_plugin_updater::{Error as UpdaterError, Update, UpdaterExt};

use super::quit;

/// `tauri.conf.json` 里 `plugins.updater.pubkey` 的占位符。发布前由 release.yml 检查已换成真实公钥。
pub const PUBKEY_PLACEHOLDER: &str = "LUBOBAO_UPDATER_PUBKEY_NOT_SET";
/// 下载进度事件，载荷见 [`Progress`]。
pub const PROGRESS_EVENT: &str = "update-progress";
/// 调试构建中覆盖升级接口的环境变量。
const ENDPOINT_ENV: &str = "LUBOBAO_UPDATER_ENDPOINT";
const CLIENT_ID_FILE: &str = "client-id";
const CLIENT_ID_HEADER: &str = "X-Client-Id";
/// 检查更新的总时限。下载不设总时限（安装包可能很大），只靠 [`READ_TIMEOUT`]。
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// 连接建立后多久收不到数据就放弃（检查和下载都适用），免得一直停在「正在检查」。
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateErrorKind {
    /// 这个构建没有配置更新公钥。
    NotConfigured,
    /// 连不上更新服务器、下载中断、服务器返回的数据无效。
    Network,
    /// 签名校验失败，已拒绝安装。
    Signature,
    /// 当前系统、架构或安装方式不支持应用内更新。
    Unsupported,
    /// 正在下载、安装或退出。
    Busy,
    /// 还没有检查或下载。
    NotReady,
    /// 安装失败。
    Install,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateError {
    pub kind: UpdateErrorKind,
    pub message: String,
}

impl UpdateError {
    fn new(kind: UpdateErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// 插件错误归类：签名相关的一律算签名失败，网络和服务器数据问题算网络错误。
fn classify(error: &UpdaterError) -> UpdateErrorKind {
    match error {
        UpdaterError::Minisign(_)
        | UpdaterError::Base64(_)
        | UpdaterError::SignatureUtf8(_)
        | UpdaterError::SignedVersionMismatch { .. }
        | UpdaterError::MissingSignedVersion => UpdateErrorKind::Signature,
        UpdaterError::Reqwest(_)
        | UpdaterError::Network(_)
        | UpdaterError::ReleaseNotFound
        | UpdaterError::Serialization(_)
        | UpdaterError::Semver(_)
        | UpdaterError::UrlParse(_)
        | UpdaterError::InsecureTransportProtocol
        | UpdaterError::EmptyEndpoints => UpdateErrorKind::Network,
        UpdaterError::UnsupportedOs
        | UpdaterError::UnsupportedArch
        | UpdaterError::TargetNotFound(_)
        | UpdaterError::TargetsNotFound(_)
        | UpdaterError::InvalidUpdaterFormat => UpdateErrorKind::Unsupported,
        _ => UpdateErrorKind::Install,
    }
}

fn describe(kind: UpdateErrorKind, error: &UpdaterError) -> String {
    match kind {
        UpdateErrorKind::Signature => format!("更新包签名校验失败，已拒绝安装：{error}"),
        UpdateErrorKind::Network => match error {
            UpdaterError::ReleaseNotFound => "更新服务器没有返回有效的版本信息，请稍后再试".into(),
            _ => format!("无法连接更新服务器：{error}"),
        },
        UpdateErrorKind::Unsupported => format!("当前系统不支持应用内更新：{error}"),
        _ => format!("安装更新失败：{error}"),
    }
}

impl From<UpdaterError> for UpdateError {
    fn from(error: UpdaterError) -> Self {
        let kind = classify(&error);
        Self::new(kind, describe(kind, &error))
    }
}

/// 发现的新版本。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    /// 更新说明（纯文本或简单的 Markdown）。
    pub notes: Option<String>,
    /// RFC 3339。
    pub pub_date: Option<String>,
    /// 服务端标记的强制更新（最新版要求强制升级，或当前版本低于最低可用版本）。只用来醒目提示。
    pub mandatory: bool,
}

/// 安装方式：`auto` 可以应用内下载安装；`manual` 只能到官网下载（Linux 的 deb 等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallMode {
    Auto,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum UpdateCheck {
    #[serde(rename_all = "camelCase")]
    UpToDate { current_version: String },
    #[serde(rename_all = "camelCase")]
    Available {
        update: UpdateInfo,
        install: InstallMode,
    },
}

/// 下载进度。`total` 为 `None` 时服务器没给长度。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// 检查到的更新和下载好的安装包（已校验签名）。
#[derive(Default)]
pub struct UpdateState {
    pending: Mutex<Option<Update>>,
    downloaded: Mutex<Option<(String, Vec<u8>)>>,
    busy: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 同一时间只做一件事（检查、下载或安装），结束时自动释放。
struct BusyGuard<'a>(&'a AtomicBool);

impl<'a> BusyGuard<'a> {
    fn take(flag: &'a AtomicBool) -> Result<Self, UpdateError> {
        if flag.swap(true, Ordering::SeqCst) {
            return Err(UpdateError::new(
                UpdateErrorKind::Busy,
                "正在检查或下载更新，请稍候",
            ));
        }
        Ok(Self(flag))
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 在 `setup` 中调用。
pub fn init(app: &tauri::AppHandle) {
    app.manage(UpdateState::default());
}

/// 公钥是否已配置：非空，且不是占位符。
pub fn pubkey_configured(pubkey: Option<&str>) -> bool {
    pubkey
        .map(str::trim)
        .is_some_and(|key| !key.is_empty() && key != PUBKEY_PLACEHOLDER)
}

fn configured_pubkey(app: &tauri::AppHandle) -> Option<String> {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|config| config.get("pubkey"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// 这种安装方式能否就地更新。Linux 上只有 AppImage 可以：xiaofany.com 对 linux 只返回 AppImage，
/// deb、rpm 用户要到官网下载。
fn install_mode() -> InstallMode {
    if cfg!(target_os = "linux") {
        let appimage = tauri::utils::platform::bundle_type()
            == Some(tauri::utils::config::BundleType::AppImage)
            && std::env::var_os("APPIMAGE").is_some();
        if !appimage {
            return InstallMode::Manual;
        }
    }
    InstallMode::Auto
}

/// 客户端 ID：16 字节随机数的十六进制，至少 8 位才算有效（服务端的要求）。
fn valid_client_id(text: &str) -> bool {
    text.len() >= 8 && text.len() <= 64 && text.chars().all(|c| c.is_ascii_hexdigit())
}

fn new_client_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 读取或生成本机的客户端 ID，只用于灰度分桶。读写失败时返回 `None`（不带这个头，按全量）。
fn client_id(app: &tauri::AppHandle) -> Option<String> {
    let dir = app.path().app_local_data_dir().ok()?;
    let path = dir.join(CLIENT_ID_FILE);
    if let Ok(text) = fs::read_to_string(&path) {
        let text = text.trim();
        if valid_client_id(text) {
            return Some(text.to_string());
        }
    }
    let id = new_client_id()?;
    fs::create_dir_all(&dir).ok()?;
    fs::write(&path, &id).ok()?;
    Some(id)
}

fn build_updater(app: &tauri::AppHandle) -> Result<tauri_plugin_updater::Updater, UpdateError> {
    if !pubkey_configured(configured_pubkey(app).as_deref()) {
        return Err(UpdateError::new(
            UpdateErrorKind::NotConfigured,
            "这个版本没有配置更新签名公钥，无法应用内更新，请到官网下载新版本",
        ));
    }
    let mut builder = app
        .updater_builder()
        .timeout(CHECK_TIMEOUT)
        .configure_client(|client| {
            client
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
        });
    if let Some(id) = client_id(app) {
        builder = builder.header(CLIENT_ID_HEADER, id)?;
    }
    if cfg!(debug_assertions)
        && let Ok(endpoint) = std::env::var(ENDPOINT_ENV)
    {
        let url = tauri::Url::parse(&endpoint).map_err(|error| {
            UpdateError::new(
                UpdateErrorKind::Network,
                format!("{ENDPOINT_ENV} 不是有效的地址：{error}"),
            )
        })?;
        builder = builder.endpoints(vec![url])?;
    }
    // Windows 上安装器启动后插件直接结束进程，不经过 RunEvent::Exit：先保存窗口大小和位置，
    // 再清掉托盘图标，免得任务栏留下残影。
    #[cfg(windows)]
    {
        let handle = app.clone();
        builder = builder.on_before_exit(move || {
            use tauri_plugin_window_state::AppHandleExt;
            if let Err(error) = handle.save_window_state(super::window::SAVED_STATE) {
                eprintln!("录播宝无法保存窗口位置：{error}");
            }
            handle.cleanup_before_exit();
        });
    }
    Ok(builder.build()?)
}

fn info_of(update: &Update) -> UpdateInfo {
    UpdateInfo {
        version: update.version.clone(),
        current_version: update.current_version.clone(),
        notes: update
            .body
            .as_deref()
            .map(str::trim)
            .filter(|notes| !notes.is_empty())
            .map(str::to_string),
        // 服务端原样给出的 RFC 3339 字符串，不必再格式化插件解析出的时间。
        pub_date: update
            .raw_json
            .get("pub_date")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        mandatory: update
            .raw_json
            .get("mandatory")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    }
}

fn state(app: &tauri::AppHandle) -> Result<tauri::State<'_, UpdateState>, UpdateError> {
    app.try_state::<UpdateState>()
        .ok_or_else(|| UpdateError::new(UpdateErrorKind::NotReady, "更新模块还没有初始化"))
}

/// 下载地址必须是 https（调试构建为了本地测试服务器也接受 http）。
fn download_url_allowed(url: &tauri::Url, debug: bool) -> bool {
    url.scheme() == "https" || (debug && url.scheme() == "http")
}

/// 整理检查得到的更新，准备下载：地址必须是 https；去掉总时限（只保留读超时）；不把客户端
/// ID 发给下载服务器（dl.xiaofany.cn），它只用于升级接口的灰度。
fn prepare_download(mut update: Update) -> Result<Update, UpdateError> {
    if !download_url_allowed(&update.download_url, cfg!(debug_assertions)) {
        return Err(UpdateError::new(
            UpdateErrorKind::Network,
            format!(
                "更新服务器给出的下载地址不是 https，已拒绝：{}://{}",
                update.download_url.scheme(),
                update.download_url.host_str().unwrap_or_default()
            ),
        ));
    }
    update.timeout = None;
    update.headers.remove(CLIENT_ID_HEADER);
    Ok(update)
}

/// 检查更新。发现新版本时记下，供 [`download_update`] 使用。
#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> Result<UpdateCheck, UpdateError> {
    let state = state(&app)?;
    let _busy = BusyGuard::take(&state.busy)?;
    let updater = build_updater(&app)?;
    let current_version = app.package_info().version.to_string();
    match updater.check().await? {
        Some(update) => {
            let update = prepare_download(update)?;
            let info = info_of(&update);
            {
                let mut downloaded = lock(&state.downloaded);
                if downloaded
                    .as_ref()
                    .is_some_and(|(version, _)| *version != update.version)
                {
                    *downloaded = None;
                }
            }
            *lock(&state.pending) = Some(update);
            Ok(UpdateCheck::Available {
                update: info,
                install: install_mode(),
            })
        }
        None => {
            *lock(&state.pending) = None;
            *lock(&state.downloaded) = None;
            Ok(UpdateCheck::UpToDate { current_version })
        }
    }
}

/// 至少间隔这么久才发一次进度事件。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

/// 下载 [`check_update`] 发现的新版本并校验签名。进度经 [`PROGRESS_EVENT`] 推送。签名不符时
/// 丢弃安装包，返回 `signature` 错误。
#[tauri::command]
pub async fn download_update(app: tauri::AppHandle) -> Result<(), UpdateError> {
    if install_mode() == InstallMode::Manual {
        return Err(UpdateError::new(
            UpdateErrorKind::Unsupported,
            "当前安装方式不支持应用内更新，请到官网下载新版本",
        ));
    }
    let state = state(&app)?;
    let _busy = BusyGuard::take(&state.busy)?;
    let update = lock(&state.pending)
        .clone()
        .ok_or_else(|| UpdateError::new(UpdateErrorKind::NotReady, "请先检查更新"))?;
    if lock(&state.downloaded)
        .as_ref()
        .is_some_and(|(version, _)| *version == update.version)
    {
        return Ok(());
    }
    let mut downloaded = 0u64;
    let mut last = Instant::now() - PROGRESS_INTERVAL;
    let handle = app.clone();
    let bytes = update
        .download(
            |chunk, total| {
                downloaded += chunk as u64;
                let done = total.is_some_and(|total| downloaded >= total);
                if done || last.elapsed() >= PROGRESS_INTERVAL {
                    last = Instant::now();
                    let _ = handle.emit(PROGRESS_EVENT, Progress { downloaded, total });
                }
            },
            || {},
        )
        .await?;
    *lock(&state.downloaded) = Some((update.version.clone(), bytes));
    Ok(())
}

/// 安装下载好的更新：与退出相同地停止调度和全部录制、等文件收尾，再安装并重启。Windows 上
/// 安装器启动后进程随即结束，安装器装完再打开新版本。
///
/// 前端在有录制时先确认。正在退出时拒绝。安装失败时录制已经停止，返回 `install` 错误，由前端
/// 提示重启。
#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<(), UpdateError> {
    let state = state(&app)?;
    let _busy = BusyGuard::take(&state.busy)?;
    let update = lock(&state.pending)
        .clone()
        .ok_or_else(|| UpdateError::new(UpdateErrorKind::NotReady, "请先检查更新"))?;
    let bytes = match lock(&state.downloaded).take() {
        Some((version, bytes)) if version == update.version => bytes,
        _ => {
            return Err(UpdateError::new(UpdateErrorKind::NotReady, "请先下载更新"));
        }
    };
    // 安装前再校验一次（install 内部也会校验）：签名不符时还没停止录制。
    if let Err(error) = update.verify(&bytes) {
        return Err(error.into());
    }
    // 在主线程上判断并进入收尾，与退出入口（也都在主线程）不会交错。
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let idle = quit::phase(&handle) == quit::QuitPhase::Idle;
        if idle {
            quit::begin_update(&handle);
        }
        let _ = sender.send(idle);
    });
    if !receiver.await.unwrap_or(false) {
        *lock(&state.downloaded) = Some((update.version.clone(), bytes));
        return Err(UpdateError::new(
            UpdateErrorKind::Busy,
            "正在退出录播宝，暂时不能安装更新",
        ));
    }
    if let Some(code) = crate::scheduler::tauri_glue::finish_before_update(&app).await {
        // 收尾期间有退出请求（注销、关机等）：放弃安装，照常退出。
        app.exit(code);
        return Err(UpdateError::new(
            UpdateErrorKind::Busy,
            "录播宝正在退出，已取消安装更新",
        ));
    }
    // Windows：启动安装器后插件调用 `std::process::exit`，不会返回。
    let installed = tauri::async_runtime::spawn_blocking(move || update.install(&bytes)).await;
    let reason = match installed {
        Ok(Ok(())) => {
            app.request_restart();
            return Ok(());
        }
        Ok(Err(error)) => format!("安装失败：{error}"),
        Err(error) => format!("安装失败：{error}"),
    };
    // 调度已经停止、无法恢复：显示窗口，前端只给「重启录播宝」。
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        quit::end_update(&handle);
        super::show_main_window(&handle);
    });
    // 前端的标题已经说明「已停止录制，重启后恢复监控」，这里只给原因。
    Err(UpdateError::new(UpdateErrorKind::Install, reason))
}

/// 安装更新失败后重启录播宝。只在录制已经收尾（调度已停止）后允许：重启不经过退出确认和
/// 收尾，不能拿来绕过它们。
#[tauri::command]
pub fn restart_after_failed_update(app: tauri::AppHandle) -> Result<(), UpdateError> {
    if !crate::scheduler::tauri_glue::finished() {
        return Err(UpdateError::new(
            UpdateErrorKind::NotReady,
            "录制还没有收尾，不能直接重启",
        ));
    }
    app.request_restart();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_or_empty_pubkey_is_not_configured() {
        assert!(!pubkey_configured(None));
        assert!(!pubkey_configured(Some("")));
        assert!(!pubkey_configured(Some("  ")));
        assert!(!pubkey_configured(Some(PUBKEY_PLACEHOLDER)));
        assert!(pubkey_configured(Some(
            "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWdu"
        )));
    }

    #[test]
    fn download_urls_must_use_https_outside_debug_builds() {
        let https = tauri::Url::parse("https://dl.xiaofany.cn/a.exe").unwrap();
        let http = tauri::Url::parse("http://dl.xiaofany.cn/a.exe").unwrap();
        let file = tauri::Url::parse("file:///C:/a.exe").unwrap();
        assert!(download_url_allowed(&https, false));
        assert!(!download_url_allowed(&http, false));
        assert!(download_url_allowed(&http, true));
        assert!(!download_url_allowed(&file, true));
    }

    #[test]
    fn client_ids_are_random_hex_of_at_least_eight_characters() {
        let a = new_client_id().unwrap();
        let b = new_client_id().unwrap();
        assert_eq!(a.len(), 32);
        assert!(valid_client_id(&a));
        assert_ne!(a, b);
        assert!(!valid_client_id("abc"));
        assert!(!valid_client_id("not-hex-at-all"));
        assert!(!valid_client_id(""));
    }

    #[test]
    fn signature_problems_are_reported_as_signature_errors() {
        let error = UpdaterError::SignedVersionMismatch {
            signed: "0.1.0".into(),
            announced: "0.1.1".into(),
        };
        assert_eq!(classify(&error), UpdateErrorKind::Signature);
        assert_eq!(
            classify(&UpdaterError::MissingSignedVersion),
            UpdateErrorKind::Signature
        );
        let converted = UpdateError::from(UpdaterError::SignatureUtf8("x".into()));
        assert_eq!(converted.kind, UpdateErrorKind::Signature);
        assert!(converted.message.contains("拒绝安装"));
    }

    #[test]
    fn server_and_network_problems_are_reported_as_network_errors() {
        assert_eq!(
            classify(&UpdaterError::ReleaseNotFound),
            UpdateErrorKind::Network
        );
        assert_eq!(
            classify(&UpdaterError::Network("status 500".into())),
            UpdateErrorKind::Network
        );
    }

    #[test]
    fn check_results_serialize_for_the_frontend() {
        let check = UpdateCheck::Available {
            update: UpdateInfo {
                version: "0.1.1".into(),
                current_version: "0.1.0".into(),
                notes: Some("- 修复".into()),
                pub_date: None,
                mandatory: true,
            },
            install: InstallMode::Manual,
        };
        let json = serde_json::to_value(&check).unwrap();
        assert_eq!(json["status"], "available");
        assert_eq!(json["install"], "manual");
        assert_eq!(json["update"]["currentVersion"], "0.1.0");
        assert_eq!(json["update"]["mandatory"], true);
        let json = serde_json::to_value(UpdateCheck::UpToDate {
            current_version: "0.1.0".into(),
        })
        .unwrap();
        assert_eq!(json["status"], "upToDate");
        assert_eq!(json["currentVersion"], "0.1.0");
        let error =
            serde_json::to_value(UpdateError::new(UpdateErrorKind::NotConfigured, "x")).unwrap();
        assert_eq!(error["kind"], "not_configured");
    }
}
