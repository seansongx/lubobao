//! YouTube 用的外部组件：yt-dlp（取直播流）和 Deno（yt-dlp 解 YouTube 的 JS 挑战要用）。
//!
//! 和 FFmpeg 一样按需下载、校验后原子安装、按绝对路径启动，但只在第一次需要从 YouTube
//! 取直播流时才下载，其他平台用不到。安装位置：
//! `<root>/<yt-dlp|deno>/<版本>/<可执行文件>`，旁边的 `verified.json` 记录安装时的大小、
//! 修改时间和 SHA-256，下次启动直接复用（离线也可用）；`<root>/.tmp/` 放下载中的文件、
//! 解压暂存和临时 Cookie 文件，安装期间持有 `.tmp/lock` 的系统文件锁。
//!
//! **更新办法**：版本、下载地址和 SHA-256 固定在 [`manifest`] 里，随录播宝发布更新。
//! 不用 `yt-dlp -U`：自更新换掉的文件不在我们的校验清单里，也会让不同用户跑不同的版本，
//! 出问题时难以复现。YouTube 改版导致 yt-dlp 失效时，检测会报「YouTube 组件可能需要更新」，
//! 发布新版录播宝时更新清单；旧版本目录在新版本装好后保留，不影响正在运行的进程。
//!
//! 本模块不依赖 Tauri；桌面端和命令行在启动时调用 [`init`] 指定组件目录。

pub mod manifest;

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ffmpeg::InstallError;
use crate::ffmpeg::download::{self, DownloadOptions};
use crate::ffmpeg::install::sha256_file;
use crate::platforms::{PlatformError, normalize_proxy};
pub use manifest::{Archive, Tool, Toolset};

const VERIFIED_RECORD: &str = "verified.json";
const LOCK_FILE: &str = "lock";
/// 安装失败后多久才再次尝试，避免每次检测都重新下载。
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// 试运行 `--version` 的时限。yt-dlp 单文件版第一次运行要解压自身，可能要十几秒。
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

/// 组件的状态，用于检测失败时给出说明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ToolsStatus {
    /// 还没检查或还没开始下载。
    Idle,
    Checking,
    Downloading {
        tool: String,
        downloaded: u64,
        total: u64,
    },
    Verifying {
        tool: String,
    },
    Ready,
    Failed {
        reason: String,
    },
}

impl ToolsStatus {
    /// 给用户看的一句话。
    pub fn describe(&self) -> String {
        match self {
            ToolsStatus::Idle | ToolsStatus::Checking => {
                "正在准备 YouTube 组件（yt-dlp 和 Deno）".to_string()
            }
            ToolsStatus::Downloading {
                tool,
                downloaded,
                total,
            } => {
                let percent = if *total == 0 {
                    0
                } else {
                    downloaded.saturating_mul(100) / total
                };
                format!(
                    "正在准备 YouTube 组件：下载 {tool} {percent}%（{:.1} / {:.1} MB）",
                    *downloaded as f64 / 1_048_576.0,
                    *total as f64 / 1_048_576.0
                )
            }
            ToolsStatus::Verifying { tool } => format!("正在准备 YouTube 组件：校验 {tool}"),
            ToolsStatus::Ready => "YouTube 组件已就绪".to_string(),
            ToolsStatus::Failed { reason } => {
                format!("YouTube 组件下载失败，稍后自动重试：{reason}")
            }
        }
    }
}

/// 装好的组件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPaths {
    pub ytdlp: PathBuf,
    pub deno: PathBuf,
    /// yt-dlp 的缓存目录（解题脚本和播放器信息），放在组件目录里。
    pub cache_dir: PathBuf,
    /// 临时文件目录（Cookie 文件）。
    pub temp_dir: PathBuf,
}

/// 组件管理器。同一进程只有一个，见 [`init`]。
pub struct ToolsManager {
    root: PathBuf,
    toolset: Result<Toolset, String>,
    download: DownloadOptions,
    status: Mutex<ToolsStatus>,
    ready: Mutex<Option<ToolPaths>>,
    running: AtomicBool,
    failed_at: Mutex<Option<Instant>>,
}

static GLOBAL: OnceLock<Arc<ToolsManager>> = OnceLock::new();

/// 指定组件目录（例如 `<数据目录>/components/youtube`）。只有第一次调用生效。
pub fn init(root: PathBuf) -> Arc<ToolsManager> {
    GLOBAL
        .get_or_init(|| Arc::new(ToolsManager::new(root, manifest::current())))
        .clone()
}

/// [`init`] 设置的管理器。
pub fn global() -> Option<Arc<ToolsManager>> {
    GLOBAL.get().cloned()
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    version: String,
    sha256: String,
    size: u64,
    modified: u64,
}

fn modified_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

impl ToolsManager {
    pub fn new(root: PathBuf, toolset: Result<Toolset, String>) -> Self {
        Self {
            root,
            toolset,
            download: DownloadOptions::default(),
            status: Mutex::new(ToolsStatus::Idle),
            ready: Mutex::new(None),
            running: AtomicBool::new(false),
            failed_at: Mutex::new(None),
        }
    }

    pub fn status(&self) -> ToolsStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set(&self, status: ToolsStatus) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }

    fn temp_dir(&self) -> PathBuf {
        self.root.join(".tmp")
    }

    pub fn binary_path(&self, tool: &Tool) -> PathBuf {
        self.root
            .join(tool.dir)
            .join(tool.version)
            .join(tool.binary)
    }

    fn paths(&self, toolset: &Toolset) -> ToolPaths {
        ToolPaths {
            ytdlp: self.binary_path(&toolset.ytdlp),
            deno: self.binary_path(&toolset.deno),
            cache_dir: self.root.join("cache"),
            temp_dir: self.temp_dir(),
        }
    }

    /// 已装好的组件；没有时在后台开始下载（每个进程同时只有一个），返回「正在准备」。
    /// 下载失败后 [`RETRY_AFTER`] 内不重试，期间返回失败原因。
    pub fn prepare(self: &Arc<Self>, proxy: Option<&str>) -> Result<ToolPaths, PlatformError> {
        if let Some(paths) = self.ready.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(paths);
        }
        let toolset = self.toolset.clone().map_err(PlatformError::Config)?;
        // 本地已有校验过的版本：直接用，不必等后台任务。
        if toolset.tools().iter().all(|tool| self.quick_check(tool)) {
            let paths = self.paths(&toolset);
            *self.ready.lock().unwrap_or_else(|e| e.into_inner()) = Some(paths.clone());
            self.set(ToolsStatus::Ready);
            return Ok(paths);
        }
        let recently_failed = self
            .failed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|at| at.elapsed() < RETRY_AFTER);
        if !recently_failed && !self.running.swap(true, Ordering::SeqCst) {
            let manager = Arc::clone(self);
            let proxy = proxy.map(str::to_string);
            self.set(ToolsStatus::Checking);
            tokio::spawn(async move {
                let result = manager.install_all(&toolset, proxy.as_deref()).await;
                match result {
                    Ok(()) => {
                        *manager.ready.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(manager.paths(&toolset));
                        *manager.failed_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        manager.set(ToolsStatus::Ready);
                    }
                    Err(error) => {
                        *manager.failed_at.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(Instant::now());
                        manager.set(ToolsStatus::Failed {
                            reason: error.to_string(),
                        });
                    }
                }
                manager.running.store(false, Ordering::SeqCst);
            });
        }
        Err(PlatformError::Preparing(self.status().describe()))
    }

    /// 等后台安装结束（测试和命令行用）。
    pub async fn prepare_and_wait(
        self: &Arc<Self>,
        proxy: Option<&str>,
    ) -> Result<ToolPaths, PlatformError> {
        loop {
            match self.prepare(proxy) {
                Ok(paths) => return Ok(paths),
                Err(error) => {
                    if let ToolsStatus::Failed { .. } = self.status()
                        && !self.running.load(Ordering::SeqCst)
                    {
                        return Err(error);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// 只比较大小和修改时间与安装记录是否一致，不计算哈希。
    fn quick_check(&self, tool: &Tool) -> bool {
        let binary = self.binary_path(tool);
        let Some(record) = read_record(&binary) else {
            return false;
        };
        let Ok(meta) = fs::metadata(&binary) else {
            return false;
        };
        record.version == tool.version
            && meta.is_file()
            && meta.len() == record.size
            && modified_secs(&meta) == record.modified
    }

    /// 已安装的文件和记录的哈希一致（大小或修改时间变了时重新计算哈希）。
    fn full_check(&self, tool: &Tool) -> bool {
        if self.quick_check(tool) {
            return true;
        }
        let binary = self.binary_path(tool);
        let Some(record) = read_record(&binary) else {
            return false;
        };
        let matches = record.version == tool.version
            && sha256_file(&binary).is_ok_and(|hash| hash.eq_ignore_ascii_case(&record.sha256));
        if matches {
            // 文件没变，只是修改时间变了：更新记录，下次不必再算。
            let _ = write_record(&binary, tool.version);
        }
        matches
    }

    async fn install_all(
        &self,
        toolset: &Toolset,
        proxy: Option<&str>,
    ) -> Result<(), InstallError> {
        let temp = self.temp_dir();
        tokio::fs::create_dir_all(&temp)
            .await
            .map_err(|err| InstallError::io("创建临时目录", err))?;
        let lock_path = temp.join(LOCK_FILE);
        let _lock = blocking(move || lock_file(&lock_path)).await??;
        let client = download_client(&self.download, proxy)?;
        for tool in toolset.tools() {
            if self.full_check(tool) {
                continue;
            }
            self.install(&client, tool).await?;
        }
        Ok(())
    }

    async fn install(&self, client: &reqwest::Client, tool: &Tool) -> Result<(), InstallError> {
        let temp = self.temp_dir();
        let part = temp.join(format!("{}.part", tool.asset()));
        let name = tool.name.to_string();
        let mut on_progress = |downloaded: u64, total: u64, _source: &str| {
            self.set(ToolsStatus::Downloading {
                tool: name.clone(),
                downloaded,
                total,
            });
        };
        download::download(
            client,
            &self.download,
            std::slice::from_ref(&tool.url),
            &part,
            tool.size,
            &mut on_progress,
        )
        .await?;

        self.set(ToolsStatus::Verifying {
            tool: tool.name.to_string(),
        });
        let stage = temp.join(format!("stage-{}", unique_suffix()));
        let target = self.binary_path(tool);
        let result = {
            let tool = tool.clone();
            let part = part.clone();
            let stage = stage.clone();
            let target = target.clone();
            blocking(move || stage_and_install(&tool, &part, &stage, &target)).await?
        };
        let _ = tokio::fs::remove_dir_all(&stage).await;
        if !matches!(result, Err(InstallError::Io { .. })) {
            download::discard_part(&part).await;
        }
        result
    }
}

/// 校验下载文件，取出可执行文件，试运行通过后改名到安装位置并写记录。阻塞。
fn stage_and_install(
    tool: &Tool,
    part: &Path,
    stage: &Path,
    target: &Path,
) -> Result<(), InstallError> {
    let size = fs::metadata(part)
        .map_err(|err| InstallError::io("读取下载文件", err))?
        .len();
    let hash = sha256_file(part).map_err(|err| InstallError::io("读取下载文件", err))?;
    if size != tool.size || !hash.eq_ignore_ascii_case(tool.sha256) {
        return Err(InstallError::TarballMismatch);
    }
    fs::create_dir_all(stage).map_err(|err| InstallError::io("创建临时目录", err))?;
    let staged = stage.join(tool.binary);
    match tool.archive {
        Archive::Raw => {
            fs::copy(part, &staged).map_err(|err| InstallError::io("复制下载文件", err))?;
        }
        Archive::Zip { entry } => {
            let file = File::open(part).map_err(|err| InstallError::io("打开安装包", err))?;
            let mut archive =
                zip::ZipArchive::new(file).map_err(|err| InstallError::io("解压安装包", err))?;
            let mut source = archive
                .by_name(entry)
                .map_err(|_| InstallError::BinaryMissing(entry.to_string()))?;
            let mut out =
                File::create(&staged).map_err(|err| InstallError::io("写入临时文件", err))?;
            io::copy(&mut source, &mut out).map_err(|err| InstallError::io("解压安装包", err))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
            .map_err(|err| InstallError::io("设置可执行权限", err))?;
    }
    probe(&staged).map_err(|detail| {
        InstallError::ProbeFailed(format!("{} 无法正常运行：{detail}", tool.name))
    })?;
    let dir = target.parent().expect("binary has a directory");
    fs::create_dir_all(dir).map_err(|err| InstallError::io("创建安装目录", err))?;
    fs::rename(&staged, target).map_err(|err| InstallError::io("安装组件", err))?;
    write_record(target, tool.version).map_err(|err| InstallError::io("写入校验记录", err))?;
    Ok(())
}

fn record_path(binary: &Path) -> PathBuf {
    binary.with_file_name(VERIFIED_RECORD)
}

fn read_record(binary: &Path) -> Option<Record> {
    let text = fs::read_to_string(record_path(binary)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_record(binary: &Path, version: &str) -> io::Result<()> {
    let meta = fs::metadata(binary)?;
    let record = Record {
        version: version.to_string(),
        sha256: sha256_file(binary)?,
        size: meta.len(),
        modified: modified_secs(&meta),
    };
    let text = serde_json::to_string_pretty(&record).map_err(io::Error::other)?;
    fs::write(record_path(binary), text)
}

/// 运行 `--version`，成功且有输出才算可用。阻塞。
fn probe(binary: &Path) -> Result<String, String> {
    let mut command = std::process::Command::new(binary);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    no_window(&mut command);
    let mut child = command.spawn().map_err(|err| format!("无法启动：{err}"))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = io::Read::read_to_string(&mut stdout, &mut text);
        text
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("运行超时".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => return Err(format!("无法等待进程：{err}")),
        }
    };
    let output = reader.join().unwrap_or_default();
    let first = output.lines().next().unwrap_or("").trim().to_string();
    if !status.success() || first.is_empty() {
        return Err(format!("退出状态 {status}"));
    }
    Ok(first)
}

/// Windows 上不弹出控制台窗口。
pub fn no_window(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn lock_file(path: &Path) -> Result<File, InstallError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|err| InstallError::io("打开安装锁", err))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => file
            .lock()
            .map(|()| file)
            .map_err(|err| InstallError::io("等待安装锁", err)),
        Err(TryLockError::Error(err)) => Err(InstallError::io("锁定安装目录", err)),
    }
}

/// 下载用的 client：和 FFmpeg 一样关闭自动解压；GitHub 在中国大陆通常要走代理，
/// 所以带上 YouTube 的代理设置。
fn download_client(
    options: &DownloadOptions,
    proxy: Option<&str>,
) -> Result<reqwest::Client, InstallError> {
    let mut builder = reqwest::Client::builder()
        .no_gzip()
        .connect_timeout(options.connect_timeout)
        .user_agent(concat!("lubobao/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy) = normalize_proxy(proxy.unwrap_or_default())
        .map_err(|error| InstallError::io("代理地址", error.message()))?
    {
        let proxy = reqwest::Proxy::all(&proxy).map_err(|err| InstallError::io("代理地址", err))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|err| InstallError::io("创建下载连接", err))
}

async fn blocking<T, F>(f: F) -> Result<T, InstallError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| InstallError::io("后台任务", err))
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// 把 `Cookie` 请求头的值写成 yt-dlp 能读的 Netscape cookies.txt，限定 `.youtube.com`。
pub fn netscape_cookies(header: &str) -> String {
    let mut text = String::from("# Netscape HTTP Cookie File\n");
    for pair in header.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty()
            || name.contains(['\t', '\r', '\n'])
            || value.contains(['\t', '\r', '\n'])
        {
            continue;
        }
        let secure = if name.starts_with("__Secure-") || name.starts_with("__Host-") {
            "TRUE"
        } else {
            "FALSE"
        };
        text.push_str(&format!(
            ".youtube.com\tTRUE\t/\t{secure}\t0\t{name}\t{}\n",
            value.trim()
        ));
    }
    text
}

/// 只在调用期间存在的临时文件，离开作用域时删除。
pub struct TempFile(PathBuf);

impl TempFile {
    /// 在 `dir` 里新建一个只有自己能读的文件并写入内容。
    pub fn create(dir: &Path, prefix: &str, content: &str) -> io::Result<TempFile> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{prefix}-{}.txt", unique_suffix()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = TempFile(path);
        let mut handle = options.open(&file.0)?;
        io::Write::write_all(&mut handle, content.as_bytes())?;
        Ok(file)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_header_becomes_a_netscape_file_for_youtube() {
        let text = netscape_cookies("SID=abc; __Secure-3PAPISID=x/y=z ;bad; =v; HSID=1");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "# Netscape HTTP Cookie File");
        assert_eq!(lines[1], ".youtube.com\tTRUE\t/\tFALSE\t0\tSID\tabc");
        assert_eq!(
            lines[2],
            ".youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-3PAPISID\tx/y=z"
        );
        assert_eq!(lines[3], ".youtube.com\tTRUE\t/\tFALSE\t0\tHSID\t1");
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn temp_files_are_removed_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let file = TempFile::create(dir.path(), "cookies", "secret").unwrap();
            assert_eq!(fs::read_to_string(file.path()).unwrap(), "secret");
            file.path().to_path_buf()
        };
        assert!(!path.exists());
    }

    #[test]
    fn status_describes_progress() {
        let status = ToolsStatus::Downloading {
            tool: "Deno".into(),
            downloaded: 21 * 1_048_576,
            total: 42 * 1_048_576,
        };
        assert_eq!(
            status.describe(),
            "正在准备 YouTube 组件：下载 Deno 50%（21.0 / 42.0 MB）"
        );
    }

    #[test]
    fn installed_tools_are_reused_from_their_record() {
        let dir = tempfile::tempdir().unwrap();
        let set = manifest::pinned("linux", "x86_64").unwrap();
        let manager = ToolsManager::new(dir.path().to_path_buf(), Ok(set.clone()));
        assert!(!manager.quick_check(&set.ytdlp));
        let binary = manager.binary_path(&set.ytdlp);
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, b"fake").unwrap();
        assert!(!manager.full_check(&set.ytdlp), "no record yet");
        write_record(&binary, set.ytdlp.version).unwrap();
        assert!(manager.quick_check(&set.ytdlp));
        // Changed content no longer matches the recorded hash.
        fs::write(&binary, b"other").unwrap();
        assert!(!manager.full_check(&set.ytdlp));
    }

    #[tokio::test]
    async fn unsupported_platforms_are_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(ToolsManager::new(
            dir.path().to_path_buf(),
            Err("不支持".into()),
        ));
        assert_eq!(manager.prepare(None).unwrap_err().kind(), "config");
    }

    #[tokio::test]
    async fn a_bad_download_is_rejected_and_reported() {
        // A local server that serves the wrong bytes: the hash check must fail.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf).await;
                let body = b"not the real file";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body).await;
            }
        });
        let tool = Tool {
            name: "yt-dlp",
            dir: "yt-dlp",
            version: "1",
            url: format!("http://{addr}/yt-dlp"),
            size: 17,
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            archive: Archive::Raw,
            binary: "yt-dlp",
        };
        let set = Toolset {
            ytdlp: tool.clone(),
            deno: Tool {
                dir: "deno",
                name: "Deno",
                ..tool
            },
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(ToolsManager::new(dir.path().to_path_buf(), Ok(set)));
        let first = manager.prepare(None).unwrap_err();
        assert_eq!(first.kind(), "preparing", "{first}");
        let error = manager.prepare_and_wait(None).await.unwrap_err();
        assert_eq!(error.kind(), "preparing", "{error}");
        assert!(error.message().contains("下载失败"), "{error}");
        assert!(
            !manager
                .binary_path(&manager.toolset.clone().unwrap().ytdlp)
                .exists()
        );
    }
}
