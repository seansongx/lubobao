//! 录后脚本测试：用真实的系统命令行（Windows cmd，其他系统 sh）和真实时钟。

use std::time::Instant;

use super::*;

/// 按系统选命令。
fn shell_cmd(windows: &str, unix: &str) -> String {
    let command = if cfg!(windows) { windows } else { unix };
    command.to_string()
}

fn vars() -> ScriptVars {
    ScriptVars {
        anchor: "anchor".into(),
        platform: "douyin".into(),
        title: "title".into(),
        room_url: "https://live.douyin.com/1".into(),
    }
}

type Events = Arc<Mutex<Vec<ScriptEvent>>>;

fn collector() -> (ScriptListener, Events) {
    let events: Events = Arc::default();
    let sink = Arc::clone(&events);
    let listener: ScriptListener = Arc::new(move |event: &ScriptEvent| {
        sink.lock().unwrap().push(event.clone());
    });
    (listener, events)
}

fn job(command: String, files: Vec<PathBuf>, listener: &ScriptListener) -> ScriptJob {
    ScriptJob {
        command,
        files,
        vars: vars(),
        listener: Arc::clone(listener),
    }
}

async fn wait_for(limit: Duration, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const LIMIT: Duration = Duration::from_secs(30);

async fn wait_events(events: &Events, count: usize) -> Vec<ScriptEvent> {
    wait_for(LIMIT, "脚本结果", || {
        events.lock().unwrap().len() >= count
    })
    .await;
    events.lock().unwrap().clone()
}

/// 在临时目录里创建这些文件，返回路径。
fn touch(dir: &Path, names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| {
            let path = dir.join(name);
            std::fs::write(&path, b"x").unwrap();
            path
        })
        .collect()
}

fn read_normalized(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

#[test]
fn reads_the_command_only_when_enabled_and_not_blank() {
    let settings = |enabled: bool, command: &str| {
        let mut map = crate::tasks::settings::defaults(Path::new("D:/录像"));
        map.insert("execute_custom_script".into(), json!(enabled));
        map.insert("custom_script_command".into(), json!(command));
        map
    };
    let defaults = crate::tasks::settings::defaults(Path::new("D:/录像"));
    assert_eq!(command(&defaults), None);
    assert_eq!(command(&settings(false, "echo hi")), None);
    assert_eq!(command(&settings(true, "  \t ")), None);
    assert_eq!(
        command(&settings(true, "  echo hi  ")).as_deref(),
        Some("echo hi")
    );
}

#[test]
fn final_files_use_the_converted_mp4_and_keep_originals_that_failed() {
    let files = vec![
        PathBuf::from("rec_000.ts"),
        PathBuf::from("rec_001.ts"),
        PathBuf::from("rec.flv"),
    ];
    // 第一个转换成功（原件即使保留也不再列出），第二个转换失败，FLV 不转换。
    let mut converted = HashMap::new();
    converted.insert(PathBuf::from("rec_000.ts"), PathBuf::from("rec_000.mp4"));
    assert_eq!(
        final_files(&files, &converted),
        vec![
            PathBuf::from("rec_000.mp4"),
            PathBuf::from("rec_001.ts"),
            PathBuf::from("rec.flv"),
        ]
    );
    assert_eq!(final_files(&files, &HashMap::new()), files);
}

#[test]
fn only_files_on_disk_are_listed() {
    let dir = tempfile::tempdir().unwrap();
    let present = touch(dir.path(), &["a.mp4", "b.ts"]);
    let mut files = present.clone();
    files.insert(1, dir.path().join("missing.ts"));
    assert_eq!(existing(files), present);
}

#[test]
fn environment_describes_the_recording() {
    let files = vec![PathBuf::from("/rec/a.mp4"), PathBuf::from("/rec/b.ts")];
    let vars = ScriptVars {
        anchor: "主播".into(),
        platform: "抖音".into(),
        title: String::new(),
        room_url: "https://live.douyin.com/1".into(),
    };
    let env: Vec<(&str, String)> = environment(&files, Path::new("/rec"), &vars)
        .into_iter()
        .map(|(name, value)| (name, value.to_string_lossy().into_owned()))
        .collect();
    let a = PathBuf::from("/rec/a.mp4").display().to_string();
    let b = PathBuf::from("/rec/b.ts").display().to_string();
    assert_eq!(
        env,
        vec![
            ("LUBOBAO_FILE", a.clone()),
            ("LUBOBAO_FILES", format!("{a}\n{b}")),
            ("LUBOBAO_DIR", Path::new("/rec").display().to_string()),
            ("LUBOBAO_ANCHOR", "主播".to_string()),
            ("LUBOBAO_PLATFORM", "抖音".to_string()),
            ("LUBOBAO_TITLE", String::new()),
            ("LUBOBAO_ROOM_URL", "https://live.douyin.com/1".to_string()),
        ]
    );
}

#[test]
fn vars_come_from_the_task_and_its_live_title() {
    let info = crate::scheduler::tests::live_info("https://live.douyin.com/1", true);
    let mut task = crate::scheduler::tests::snapshot_for(&info);
    assert_eq!(ScriptVars::of(&task).title, "");
    task.live = LiveState::Live {
        checked_at: 1,
        title: Some("标题".into()),
    };
    let vars = ScriptVars::of(&task);
    assert_eq!(vars.title, "标题");
    assert_eq!(vars.anchor, task.config.anchor_name);
    assert_eq!(vars.platform, "抖音");
    assert_eq!(vars.room_url, "https://live.douyin.com/1");
}

#[tokio::test]
async fn runs_in_the_recording_directory_with_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let present = touch(dir.path(), &["a.mp4", "b.ts"]);
    let mut files = present.clone();
    files.push(dir.path().join("missing.ts"));
    let command = shell_cmd(
        "set LUBOBAO_> env.txt",
        r#"printf 'LUBOBAO_FILE=%s\nLUBOBAO_FILES=%s\nLUBOBAO_DIR=%s\nLUBOBAO_ANCHOR=%s\nLUBOBAO_PLATFORM=%s\nLUBOBAO_TITLE=%s\nLUBOBAO_ROOM_URL=%s\n' "$LUBOBAO_FILE" "$LUBOBAO_FILES" "$LUBOBAO_DIR" "$LUBOBAO_ANCHOR" "$LUBOBAO_PLATFORM" "$LUBOBAO_TITLE" "$LUBOBAO_ROOM_URL" > env.txt"#,
    );
    let runner = ScriptRunner::default();
    let (listener, events) = collector();
    assert!(runner.enqueue(job(command, files, &listener)));
    let events = wait_events(&events, 1).await;
    let expected = ScriptEvent::Succeeded {
        files: present.clone(),
    };
    assert_eq!(events, vec![expected]);

    // 相对路径写到了录像目录里：工作目录就是录像所在目录。
    let output = read_normalized(&dir.path().join("env.txt"));
    let (a, b) = (present[0].display(), present[1].display());
    for line in [
        format!("LUBOBAO_FILE={a}\n"),
        format!("LUBOBAO_FILES={a}\n{b}\n"),
        format!("LUBOBAO_DIR={}\n", dir.path().display()),
        "LUBOBAO_ANCHOR=anchor\n".to_string(),
        "LUBOBAO_PLATFORM=douyin\n".to_string(),
        "LUBOBAO_TITLE=title\n".to_string(),
        "LUBOBAO_ROOM_URL=https://live.douyin.com/1\n".to_string(),
    ] {
        assert!(output.contains(&line), "缺少 {line:?}：{output}");
    }
    wait_for(LIMIT, "队列清空", || runner.pending() == 0).await;
}

#[tokio::test]
async fn a_nonzero_exit_is_a_failure_with_the_last_stderr_line() {
    let dir = tempfile::tempdir().unwrap();
    let files = touch(dir.path(), &["a.mp4"]);
    let command = shell_cmd(
        "echo first 1>&2 & echo boom 1>&2 & exit 3",
        "echo first >&2; echo boom >&2; exit 3",
    );
    let runner = ScriptRunner::default();
    let (listener, events) = collector();
    runner.enqueue(job(command, files.clone(), &listener));
    let events = wait_events(&events, 1).await;
    let expected = ScriptEvent::Failed {
        files: files.clone(),
        failure: ScriptFailure::Exit(Some(3)),
        stderr: Some("boom".into()),
    };
    assert_eq!(events, vec![expected]);
    let (kind, message, detail) = describe(&events[0]).unwrap();
    assert_eq!(kind, ActivityKind::Warning);
    assert_eq!(message, "录后脚本执行失败（退出码 3）：boom");
    assert_eq!(detail["kind"], "script");
    assert_eq!(detail["exitCode"], 3);
    assert_eq!(detail["stderr"], "boom");
}

#[tokio::test]
async fn a_script_that_runs_too_long_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let files = touch(dir.path(), &["a.mp4"]);
    let command = shell_cmd("ping -n 30 127.0.0.1 > nul", "sleep 30");
    let limit = Duration::from_millis(500);
    let runner = ScriptRunner::new(limit);
    let (listener, events) = collector();
    let started = Instant::now();
    runner.enqueue(job(command, files.clone(), &listener));
    let events = wait_events(&events, 1).await;
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "超时后应结束进程，实际用了 {:?}",
        started.elapsed()
    );
    let expected = ScriptEvent::Failed {
        files,
        failure: ScriptFailure::TimedOut(limit),
        stderr: None,
    };
    assert_eq!(events, vec![expected]);
    let (kind, message, _) = describe(&events[0]).unwrap();
    assert_eq!(kind, ActivityKind::Warning);
    assert_eq!(
        failure_message(&ScriptFailure::TimedOut(TIMEOUT)),
        "录后脚本超过 10 分钟未结束，已结束进程"
    );
    assert!(message.starts_with("录后脚本超过"), "{message}");
}

#[tokio::test]
async fn a_command_that_cannot_start_is_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (_sender, mut cancel) = watch::channel(false);
    let run = run_command(
        "exit 0",
        &dir.path().join("missing"),
        &[],
        Duration::from_secs(5),
        &mut cancel,
    )
    .await;
    assert!(matches!(run, Run::SpawnFailed(_)));
    let event = ScriptEvent::Failed {
        files: vec![PathBuf::from("a.mp4")],
        failure: ScriptFailure::Spawn("找不到目录".into()),
        stderr: None,
    };
    let (kind, message, detail) = describe(&event).unwrap();
    assert_eq!(kind, ActivityKind::Warning);
    assert_eq!(message, "录后脚本无法启动：找不到目录");
    assert_eq!(detail["filePath"], "a.mp4");
    assert!(detail["exitCode"].is_null());
}

#[tokio::test]
async fn success_is_recorded_as_a_script_activity() {
    let dir = tempfile::tempdir().unwrap();
    let files = touch(dir.path(), &["a.mp4"]);
    let runner = ScriptRunner::default();
    let (listener, events) = collector();
    runner.enqueue(job("exit 0".into(), files.clone(), &listener));
    let events = wait_events(&events, 1).await;
    let (kind, message, detail) = describe(&events[0]).unwrap();
    assert_eq!(kind, ActivityKind::Script);
    assert_eq!(message, "录后脚本已执行");
    assert_eq!(detail["kind"], "script");
    assert_eq!(detail["files"].as_array().unwrap().len(), 1);
    // 文件都不在时不执行，也不记动态。
    std::fs::remove_file(&files[0]).unwrap();
    let (listener, events) = collector();
    runner.enqueue(job("exit 0".into(), files, &listener));
    let events = wait_events(&events, 1).await;
    assert_eq!(events, vec![ScriptEvent::Skipped]);
    assert_eq!(describe(&events[0]), None);
}

#[tokio::test]
async fn scripts_run_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let files = touch(dir.path(), &["a.mp4"]);
    let first = shell_cmd(
        "ping -n 2 127.0.0.1 > nul & echo x> one.txt",
        "sleep 1; echo x > one.txt",
    );
    // 第二个开始时第一个必须已经结束。
    let second = shell_cmd(
        "if exist one.txt (echo x> two.txt) else (exit 1)",
        "[ -f one.txt ] && echo x > two.txt",
    );
    let runner = ScriptRunner::default();
    let (listener, events) = collector();
    runner.enqueue(job(first, files.clone(), &listener));
    runner.enqueue(job(second, files.clone(), &listener));
    assert_eq!(runner.pending(), 2);
    let events = wait_events(&events, 2).await;
    let succeeded = |event: &ScriptEvent| matches!(event, ScriptEvent::Succeeded { .. });
    assert!(events.iter().all(succeeded), "{events:?}");
    assert!(dir.path().join("two.txt").is_file());
}

#[tokio::test]
async fn shutdown_drops_queued_scripts_and_kills_the_running_one() {
    let dir = tempfile::tempdir().unwrap();
    let files = touch(dir.path(), &["a.mp4"]);
    let long = shell_cmd(
        "echo x> started.txt & ping -n 30 127.0.0.1 > nul",
        "echo x > started.txt; sleep 30",
    );
    let queued = shell_cmd("echo x> queued.txt", "echo x > queued.txt");
    let runner = ScriptRunner::default();
    let (listener, events) = collector();
    runner.enqueue(job(long, files.clone(), &listener));
    runner.enqueue(job(queued, files.clone(), &listener));
    let started_marker = dir.path().join("started.txt");
    wait_for(LIMIT, "第一个脚本开始", || started_marker.is_file()).await;

    let started = Instant::now();
    assert!(runner.shutdown(SHUTDOWN_TIMEOUT).await);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "退出不应等脚本跑完：{:?}",
        started.elapsed()
    );
    let events = events.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events.contains(&ScriptEvent::Cancelled { started: false }));
    assert!(events.contains(&ScriptEvent::Cancelled { started: true }));
    let cancelled = ScriptEvent::Cancelled { started: true };
    let (kind, message, _) = describe(&cancelled).unwrap();
    assert_eq!(kind, ActivityKind::Warning);
    assert_eq!(message, "应用退出，录后脚本已中止");
    assert_eq!(describe(&ScriptEvent::Cancelled { started: false }), None);

    assert_eq!(runner.pending(), 0);
    assert!(!runner.enqueue(job("exit 0".into(), files, &listener)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!dir.path().join("queued.txt").exists());
}

// ---- 与转换衔接 ----

fn tracker(runner: &ScriptRunner, files: Vec<PathBuf>) -> (AfterRecording, Events) {
    let (listener, events) = collector();
    let after = AfterRecording::new(runner, "exit 0".into(), files, vars(), listener);
    (after.unwrap(), events)
}

#[tokio::test]
async fn waits_for_every_conversion_before_running() {
    let dir = tempfile::tempdir().unwrap();
    // a.ts 转出了 a.mp4（原件保留），b.ts 转换失败，c.flv 不转换。
    let files = touch(dir.path(), &["a.ts", "a.mp4", "b.ts", "c.flv"]);
    let [a_ts, a_mp4, b_ts, c_flv] = <[PathBuf; 4]>::try_from(files).unwrap();
    let runner = ScriptRunner::default();
    let (after, events) = tracker(&runner, vec![a_ts.clone(), b_ts.clone(), c_flv.clone()]);
    after.converting(&[a_ts.clone(), b_ts.clone()]);
    after.release();
    after.observe(&ConvertEvent::Started {
        source: a_ts.clone(),
        target: a_mp4.clone(),
    });
    after.observe(&ConvertEvent::Succeeded {
        source: a_ts.clone(),
        target: a_mp4.clone(),
        duration_secs: 2.0,
        original_deleted: false,
        delete_error: None,
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(runner.pending(), 0, "还有转换没结束，不应执行");
    assert!(events.lock().unwrap().is_empty());

    after.observe(&ConvertEvent::Failed {
        source: b_ts.clone(),
        message: "出错".into(),
        original_kept: true,
    });
    let events = wait_events(&events, 1).await;
    let expected = ScriptEvent::Succeeded {
        files: vec![a_mp4, b_ts, c_flv],
    };
    assert_eq!(events, vec![expected]);
}

#[tokio::test]
async fn runs_once_right_away_when_nothing_is_converted() {
    let dir = tempfile::tempdir().unwrap();
    let files = touch(dir.path(), &["a.ts"]);
    let runner = ScriptRunner::default();
    let (after, log) = tracker(&runner, files.clone());
    after.release();
    after.release();
    let events = wait_events(&log, 1).await;
    assert_eq!(events, vec![ScriptEvent::Succeeded { files }]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(runner.pending(), 0);
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_conversion_cancelled_by_shutdown_skips_the_script() {
    let dir = tempfile::tempdir().unwrap();
    let files = touch(dir.path(), &["a.ts"]);
    let runner = ScriptRunner::default();
    let (after, events) = tracker(&runner, files.clone());
    after.converting(&files);
    after.release();
    after.observe(&ConvertEvent::Cancelled {
        source: files[0].clone(),
        started: true,
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(runner.pending(), 0);
    assert!(events.lock().unwrap().is_empty());
}
