//! 命令行程序的端到端测试，不依赖真实直播间。
//!
//! 用 ffmpeg 生成一段 FLV 样本，起一个只监听 127.0.0.1 的 HTTP 服务按时间戳实时吐出数据，
//! 再用测试用平台适配器（feature `e2e`）把 `e2e+http://…` 当成直播间：
//!
//! 1. 启动 `lubobao-cli run`，`add` 后等待开始录制；
//! 2. `stop` 停止录制，文件可以完整解码；
//! 3. 关掉流模拟下播，再开播：自动续录；在录制中断流，录制随之结束，文件可以解码；
//! 4. 再开播、录制中发送 SIGTERM（Windows 上为 Ctrl+Break，发不出去时用 `shutdown`）：
//!    进程正常退出，最后的文件也能解码，没有 ffmpeg 残留（源站上的连接全部断开）。
//!
//! 需要环境变量 `LUBOBAO_TEST_FFMPEG`（ffmpeg 可执行文件），没设置时跳过。
//! `LUBOBAO_E2E_CLI` 可指定要测的 `lubobao-cli`（例如 Docker 镜像里的），缺省用本次构建的。
//! 运行：`cargo test -p lubobao-cli --features e2e --test e2e -- --nocapture`。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};

const SAMPLE_SECS: u32 = 120;
const STEP_TIMEOUT: Duration = Duration::from_secs(40);

fn test_ffmpeg() -> Option<PathBuf> {
    match std::env::var_os("LUBOBAO_TEST_FFMPEG") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => {
            eprintln!("跳过：未设置 LUBOBAO_TEST_FFMPEG（ffmpeg 可执行文件路径）");
            None
        }
    }
}

fn cli_path() -> PathBuf {
    match std::env::var_os("LUBOBAO_E2E_CLI") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(env!("CARGO_BIN_EXE_lubobao-cli")),
    }
}

/// 320×240、25 fps、每秒一个关键帧的 H.264，加 AAC 正弦波。
fn make_sample(ffmpeg: &Path, dir: &Path) -> PathBuf {
    let path = dir.join("sample.flv");
    let output = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=size=320x240:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100"])
        .args(["-t", &SAMPLE_SECS.to_string()])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-g", "25",
        ])
        .args(["-c:a", "aac", "-b:a", "64k", "-f", "flv"])
        .arg(&path)
        .output()
        .expect("run ffmpeg to create sample");
    assert!(
        output.status.success(),
        "sample generation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}

/// 分段录制时任务显示第一个分段 `…_000.ts`，展开成这次录制实际写出的所有分段。
fn expand(first: &Path) -> Vec<PathBuf> {
    let name = first.file_name().unwrap().to_string_lossy().into_owned();
    let Some((prefix, suffix)) = name.rsplit_once("_000") else {
        return vec![first.to_path_buf()];
    };
    let prefix = format!("{prefix}_");
    let dir = first.parent().unwrap();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    let name = path.file_name().unwrap().to_string_lossy();
                    // 只要序号是数字的分段：同一秒内再录的 `stem_2_000.ts` 不算。
                    name.strip_prefix(prefix.as_str())
                        .and_then(|rest| rest.strip_suffix(suffix))
                        .is_some_and(|number| {
                            !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                        })
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// 完整解码录像（展开成这次录制的所有分段后逐个解码），确认没有错误输出。
fn assert_recording_playable(ffmpeg: &Path, first: &Path) {
    let files = expand(first);
    assert!(!files.is_empty(), "no files for {}", first.display());
    for file in &files {
        assert_playable(ffmpeg, file);
    }
}

/// 完整解码，确认没有错误输出。
fn assert_playable(ffmpeg: &Path, file: &Path) {
    let output = std::process::Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg to decode");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.trim().is_empty(),
        "{} is not cleanly playable: {stderr}",
        file.display()
    );
}

struct Flv {
    header: Vec<u8>,
    tags: Vec<(u32, Vec<u8>)>,
}

fn parse_flv(bytes: &[u8]) -> Flv {
    assert_eq!(&bytes[..3], b"FLV");
    let header_len = u32::from_be_bytes(bytes[5..9].try_into().unwrap()) as usize + 4;
    let mut tags = Vec::new();
    let mut offset = header_len;
    while offset + 11 <= bytes.len() {
        let size = u32::from_be_bytes([0, bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
            as usize;
        let timestamp = u32::from_be_bytes([
            bytes[offset + 7],
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
        ]);
        let end = offset + 11 + size + 4;
        if end > bytes.len() {
            break;
        }
        tags.push((timestamp, bytes[offset..end].to_vec()));
        offset = end;
    }
    Flv {
        header: bytes[..header_len].to_vec(),
        tags,
    }
}

/// 模拟直播源。`live` 为假时 `HEAD`/`GET` 返回 404，正在发送的连接在下一个 tag 处断开。
struct Source {
    url: String,
    live: Arc<AtomicBool>,
    /// 正在发送数据的连接数，即正在拉流的 ffmpeg 数。
    active: Arc<AtomicUsize>,
}

struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn start_source(sample: &Path) -> Source {
    let flv = Arc::new(parse_flv(&std::fs::read(sample).unwrap()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/live/e2e.flv", listener.local_addr().unwrap());
    let live = Arc::new(AtomicBool::new(true));
    let active = Arc::new(AtomicUsize::new(0));
    let (live_flag, active_count) = (Arc::clone(&live), Arc::clone(&active));
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let flv = Arc::clone(&flv);
            let live = Arc::clone(&live_flag);
            let active = Arc::clone(&active_count);
            tokio::spawn(async move { serve(socket, &flv, &live, &active).await });
        }
    });
    Source { url, live, active }
}

async fn serve(
    mut socket: tokio::net::TcpStream,
    flv: &Flv,
    live: &AtomicBool,
    active: &Arc<AtomicUsize>,
) {
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
    let is_head = request.starts_with(b"HEAD ");
    if !live.load(Ordering::SeqCst) {
        let _ = socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        let _ = socket.shutdown().await;
        return;
    }
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nConnection: close\r\n\r\n";
    if socket.write_all(head).await.is_err() || is_head {
        let _ = socket.shutdown().await;
        return;
    }
    active.fetch_add(1, Ordering::SeqCst);
    let _guard = ActiveGuard(Arc::clone(active));
    if socket.write_all(&flv.header).await.is_err() {
        return;
    }
    let begin = Instant::now();
    for (timestamp, tag) in &flv.tags {
        if !live.load(Ordering::SeqCst) {
            let _ = socket.shutdown().await;
            return;
        }
        // 先给约 1 秒的缓存，之后按实时速率发送。
        let due = Duration::from_millis(u64::from(timestamp.saturating_sub(1000)));
        let elapsed = begin.elapsed();
        if due > elapsed {
            sleep(due - elapsed).await;
        }
        if socket.write_all(tag).await.is_err() {
            return;
        }
    }
    let _ = socket.shutdown().await;
}

/// 运行中的 `lubobao-cli run` 和测试目录。失败时打印 run 的日志；结束时确保进程已退出。
struct Harness {
    cli: PathBuf,
    ffmpeg: PathBuf,
    data_dir: PathBuf,
    log_path: PathBuf,
    child: Option<Child>,
    _temp: tempfile::TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
            eprintln!("---- lubobao-cli run 日志 ----\n{log}---- 日志结束 ----");
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

impl Harness {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.cli);
        command
            .env("LUBOBAO_DATA_DIR", &self.data_dir)
            .env("LUBOBAO_FFMPEG", &self.ffmpeg)
            .env_remove("LUBOBAO_SAVE_DIR")
            .stdin(Stdio::null());
        command
    }

    /// 运行一个子命令，要求成功，返回 stdout。
    async fn cli(&self, args: &[&str]) -> String {
        let output = self
            .command()
            .args(args)
            .output()
            .await
            .expect("run lubobao-cli");
        assert!(
            output.status.success(),
            "lubobao-cli {args:?} failed ({}): {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    async fn task(&self, id: i64) -> Value {
        let list: Value = serde_json::from_str(&self.cli(&["list", "--json"]).await).unwrap();
        list.as_array()
            .unwrap()
            .iter()
            .find(|task| task["id"].as_i64() == Some(id))
            .cloned()
            .unwrap_or_else(|| panic!("task {id} missing: {list}"))
    }

    /// 轮询任务直到满足条件。
    async fn wait_task(&self, id: i64, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            let task = self.task(id).await;
            if check(&task) {
                return task;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {task}"
            );
            sleep(Duration::from_millis(300)).await;
        }
    }

    async fn wait_recording(&self, id: i64) -> PathBuf {
        let task = self
            .wait_task(id, "recording with data", |task| {
                task["record"]["state"] == "recording"
                    && task["record"]["bytesWritten"].as_u64().unwrap_or(0) > 0
            })
            .await;
        PathBuf::from(task["record"]["filePath"].as_str().unwrap())
    }

    async fn wait_idle(&self, id: i64) {
        self.wait_task(id, "recording finished", |task| {
            task["record"]["state"] == "idle"
        })
        .await;
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

/// 与 `docker stop`、systemd 一样请求正常退出。
async fn request_exit(harness: &Harness, pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: 只向自己启动的子进程发信号。
        let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        assert_eq!(result, 0, "kill(SIGTERM) failed");
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
        // 子进程在自己的进程组里，Ctrl+Break 只发给它。测试进程没有控制台时发不出去，
        // 改用控制通道的 shutdown（与信号走同一条收尾路径）。
        // SAFETY: 参数为常量和子进程组 id。
        let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } != 0;
        if !sent {
            eprintln!("无法发送 Ctrl+Break，改用 lubobao-cli shutdown");
            harness.cli(&["shutdown"]).await;
        }
    }
    let _ = harness;
}

fn media_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            media_files(&path, out);
        } else if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("ts" | "mp4")
        ) {
            out.push(path);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_records_stops_and_exits_cleanly() {
    let Some(ffmpeg) = test_ffmpeg() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let sample = make_sample(&ffmpeg, temp.path());
    let source = start_source(&sample).await;
    let mut harness = Harness {
        cli: cli_path(),
        ffmpeg: ffmpeg.clone(),
        log_path: temp.path().join("run.log"),
        data_dir,
        child: None,
        _temp: temp,
    };

    // run 没有运行时直接改数据库，下次启动生效。
    harness
        .cli(&["config", "set", "force_https_recording", "false"])
        .await;
    harness
        .cli(&["config", "set", "loop_time_seconds", "2"])
        .await;
    let offline_stop = harness
        .command()
        .args(["stop", "all"])
        .output()
        .await
        .unwrap();
    assert_eq!(offline_stop.status.code(), Some(3), "stop needs run");

    let log = std::fs::File::create(&harness.log_path).unwrap();
    let mut run = harness.command();
    run.arg("run").stdout(Stdio::null()).stderr(log);
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        run.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let child = run.spawn().expect("start lubobao-cli run");
    let pid = child.id().unwrap();
    harness.child = Some(child);

    // 控制通道就绪后 status 报告运行中；第二个 run 被实例锁拒绝。
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        let status: Value =
            serde_json::from_str(&harness.cli(&["status", "--json"]).await).unwrap();
        if status["running"] == true {
            assert_eq!(status["pid"].as_u64(), Some(u64::from(pid)));
            assert_eq!(status["ffmpeg"]["state"], "external");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "run did not start: {}",
            harness.log()
        );
        sleep(Duration::from_millis(200)).await;
    }
    let second = harness.command().arg("run").output().await.unwrap();
    assert!(!second.status.success(), "second run must be rejected");

    // 1. 添加后立即检测，开始录制。
    let link = format!("e2e+{}", source.url);
    let added = harness.cli(&["add", &link]).await;
    assert!(added.contains("已添加"), "{added}");
    let list: Value = serde_json::from_str(&harness.cli(&["list", "--json"]).await).unwrap();
    let id = list[0]["id"].as_i64().unwrap();
    let first = harness.wait_recording(id).await;
    // 分段录制（默认开启）显示实际的第一个分段，不是 `%03d` 模板。
    assert!(
        first.to_string_lossy().ends_with("_000.ts"),
        "{}",
        first.display()
    );
    let listed = harness.cli(&["list"]).await;
    assert!(
        listed.contains("_000.ts") && !listed.contains('%'),
        "{listed}"
    );
    sleep(Duration::from_secs(4)).await;

    // 2. 手动停止：收尾后回到空闲，本次直播不再自动录制；日志写明是手动停止。
    harness.cli(&["stop", &id.to_string()]).await;
    harness.wait_idle(id).await;
    let task = harness.task(id).await;
    assert_eq!(task["recordHeld"], true, "{task}");
    assert_recording_playable(&ffmpeg, &first);
    let log = harness.log();
    assert!(log.contains("录制结束：手动停止"), "{log}");
    assert!(!log.contains("%03d"), "{log}");

    // 2b. start 清除手动停止的保持状态，立即检测，仍在播就再次录制。
    let started = harness.cli(&["start", &id.to_string()]).await;
    assert!(started.contains("已重新开始录制"), "{started}");
    let restarted = harness.wait_recording(id).await;
    assert_ne!(restarted, first);
    assert_eq!(harness.task(id).await["recordHeld"], false);
    sleep(Duration::from_secs(3)).await;
    harness.cli(&["stop", &id.to_string()]).await;
    harness.wait_idle(id).await;
    assert_recording_playable(&ffmpeg, &restarted);

    // 3. 下播 → 等待开播；再开播 → 自动续录；录制中断流 → 录制结束。
    source.live.store(false, Ordering::SeqCst);
    harness
        .wait_task(id, "offline", |task| task["display"] == "waiting")
        .await;
    source.live.store(true, Ordering::SeqCst);
    let second_file = harness.wait_recording(id).await;
    assert_ne!(second_file, first);
    sleep(Duration::from_secs(4)).await;
    source.live.store(false, Ordering::SeqCst);
    harness.wait_idle(id).await;
    assert_recording_playable(&ffmpeg, &second_file);

    // 4. 再开播，录制中请求退出：正常收尾，文件可解码，没有 ffmpeg 残留。
    source.live.store(true, Ordering::SeqCst);
    let third = harness.wait_recording(id).await;
    sleep(Duration::from_secs(3)).await;
    request_exit(&harness, pid).await;
    let child = harness.child.as_mut().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(45), child.wait())
        .await
        .expect("run did not exit within 45 s")
        .unwrap();
    harness.child = None;
    assert!(
        status.success(),
        "run exited with {status}: {}",
        harness.log()
    );
    let log = harness.log();
    assert!(log.contains("已退出"), "{log}");
    assert_recording_playable(&ffmpeg, &third);

    let deadline = Instant::now() + Duration::from_secs(5);
    while source.active.load(Ordering::SeqCst) > 0 {
        assert!(
            Instant::now() < deadline,
            "ffmpeg still pulling the stream after run exited"
        );
        sleep(Duration::from_millis(100)).await;
    }

    // 退出后控制通道已关闭，修改类命令回到直接操作数据库。
    let status: Value = serde_json::from_str(&harness.cli(&["status", "--json"]).await).unwrap();
    assert_eq!(status["running"], false);

    // 每个录像都能解码（含录后转出的 MP4）。
    let mut files = Vec::new();
    media_files(&harness.data_dir.join("recordings"), &mut files);
    assert!(files.len() >= 3, "{files:?}");
    for file in &files {
        assert_playable(&ffmpeg, file);
    }
    eprintln!(
        "---- lubobao-cli run 日志 ----
{}---- 日志结束 ----
录像：{files:#?}",
        harness.log()
    );
}
