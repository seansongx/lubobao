//! 调度测试：平台请求和录制引擎都换成假实现，时间用 Tokio 暂停时钟，不依赖网络和 ffmpeg。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;
use tokio::time::{Instant, sleep};

use super::*;
use crate::platforms::{Platform, Quality, SelectedStream, StreamVariant};
use crate::recorder::{EndReason, Progress, StopCause};
use crate::tasks::model::HELD_REASON;
use crate::tasks::store::Store;
use crate::tasks::{AddTaskInput, DisplayStatus, TaskMutation};

use super::SHUTDOWN_TIMEOUT;

// ---- 假平台 ----

#[derive(Clone)]
enum Reply {
    Live,
    /// 已开播，但没有流地址。
    LiveNoStream,
    Offline,
    Fail(PlatformError),
    /// 适配器内部 panic。
    Panic,
}

#[derive(Default)]
struct FakeSource {
    replies: Mutex<HashMap<String, Reply>>,
    calls: Mutex<Vec<(String, Instant)>>,
    /// 每次请求收到的选项。
    options: Mutex<Vec<FetchOptions>>,
    /// 设置后每次请求先取一个许可，用来让请求停在半路。
    gate: Mutex<Option<Arc<Semaphore>>>,
    current: AtomicUsize,
    max: AtomicUsize,
}

impl FakeSource {
    fn set(&self, url: &str, reply: Reply) {
        self.replies.lock().unwrap().insert(url.to_string(), reply);
    }

    fn calls(&self, url: &str) -> Vec<Instant> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(called, _)| called == url)
            .map(|(_, at)| *at)
            .collect()
    }

    fn total_calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

pub(super) fn live_info(url: &str, is_live: bool) -> LiveInfo {
    let room_id = url.rsplit('/').next().unwrap_or_default().to_string();
    let variant = StreamVariant {
        name: "ORIGIN".into(),
        quality: Some(Quality::Od),
        flv_url: Some(format!("http://cdn.example/{room_id}.flv")),
        hls_url: Some(format!("http://cdn.example/{room_id}.m3u8")),
    };
    LiveInfo {
        platform: Platform::Douyin,
        room_id: room_id.clone(),
        anchor_id: Some(format!("sec-{room_id}")),
        live_url: Some(url.to_string()),
        anchor_name: format!("主播{room_id}"),
        title: is_live.then(|| "标题".to_string()),
        is_live,
        variants: if is_live {
            vec![variant.clone()]
        } else {
            Vec::new()
        },
        selected: is_live.then(|| SelectedStream {
            requested: Quality::Od,
            record_url: variant.hls_url.clone().unwrap(),
            variant,
        }),
        stream_headers: BTreeMap::from([(
            "Referer".to_string(),
            "https://live.douyin.com/".to_string(),
        )]),
    }
}

/// 由检测结果构造一个监控中、空闲的任务快照（policy 测试用）。
pub(super) fn snapshot_for(info: &LiveInfo) -> TaskSnapshot {
    crate::tasks::Task {
        id: 1,
        config: crate::tasks::TaskConfig {
            url: info.live_url.clone().unwrap_or_default(),
            platform: info.platform,
            room_id: info.room_id.clone(),
            anchor_id: info.anchor_id.clone(),
            anchor_name: info.anchor_name.clone(),
            live_url: info.live_url.clone(),
            quality: Quality::Od,
            notify_only: false,
            note: String::new(),
            created_at: 0,
        },
        monitoring: Monitoring::Enabled,
        live: LiveState::Unknown,
        record: RecordState::Idle,
        held: false,
    }
    .snapshot()
}

impl LiveSource for Arc<FakeSource> {
    fn fetch(
        &self,
        url: String,
        options: FetchOptions,
    ) -> BoxFuture<'static, Result<LiveInfo, PlatformError>> {
        let source = Arc::clone(self);
        Box::pin(async move {
            source.options.lock().unwrap().push(options);
            source
                .calls
                .lock()
                .unwrap()
                .push((url.clone(), Instant::now()));
            let now = source.current.fetch_add(1, Ordering::SeqCst) + 1;
            source.max.fetch_max(now, Ordering::SeqCst);
            let gate = source.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            source.current.fetch_sub(1, Ordering::SeqCst);
            let reply = source
                .replies
                .lock()
                .unwrap()
                .get(&url)
                .cloned()
                .unwrap_or(Reply::Offline);
            match reply {
                Reply::Live => Ok(live_info(&url, true)),
                Reply::LiveNoStream => Ok(LiveInfo {
                    selected: None,
                    variants: Vec::new(),
                    ..live_info(&url, true)
                }),
                Reply::Offline => Ok(live_info(&url, false)),
                Reply::Fail(error) => Err(error),
                Reply::Panic => panic!("假适配器 panic"),
            }
        })
    }
}

// ---- 假录制引擎 ----
//
// 与真实引擎一致：
// - `start` 返回前可能要等一会（建目录、查磁盘、启动 ffmpeg），测试用 `start_gate` 控制；
// - `stop` 只发请求，事件在另一个任务里送回；
// - `stop_all` 只停 `start` 已经返回的录制（真实引擎此时才登记到全局列表）；
// - `wait` 在 `finished` 送出之后才返回；引擎内部崩溃时不发 `finished`，`wait` 返回失败。

struct Shared {
    listener: EventListener,
    stopped: AtomicBool,
    finished: AtomicBool,
    /// `start` 是否已经返回。
    returned: AtomicBool,
    outcome: tokio::sync::watch::Sender<Option<RecordingOutcome>>,
    /// 结束结果里的文件。
    files: Vec<PathBuf>,
}

impl Shared {
    fn finish(&self, reason: EndReason) {
        if !self.finished.swap(true, Ordering::SeqCst) {
            let mut outcome = outcome(reason);
            outcome.files = self.files.clone();
            (self.listener)(&RecorderEvent::Finished {
                outcome: outcome.clone(),
            });
            self.outcome.send_replace(Some(outcome));
        }
    }

    /// 引擎内部任务崩溃：不发 `finished`，`wait` 得到失败结果。
    fn crash(&self) {
        if !self.finished.swap(true, Ordering::SeqCst) {
            self.outcome
                .send_replace(Some(failed_outcome("录制任务意外结束".into())));
        }
    }
}

struct FakeRecording {
    request: RecordingRequest,
    shared: Arc<Shared>,
}

struct FakeControl(Arc<Shared>);

fn outcome(reason: EndReason) -> RecordingOutcome {
    RecordingOutcome {
        reason,
        message: None,
        exit_code: Some(0),
        files: vec![PathBuf::from("rec_000.ts")],
        bytes_written: 1000,
        duration_secs: Some(10.0),
        output_dir: None,
        started_at_ms: 0,
        ended_at_ms: 1,
    }
}

impl RecordingControl for FakeControl {
    fn stop(&self) {
        if self.0.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared = Arc::clone(&self.0);
        tokio::spawn(async move {
            if shared.finished.load(Ordering::SeqCst) {
                return;
            }
            (shared.listener)(&RecorderEvent::Stopping {
                cause: StopCause::User,
            });
            shared.finish(EndReason::UserStopped);
        });
    }

    fn wait(&self) -> BoxFuture<'static, RecordingOutcome> {
        let mut rx = self.0.outcome.subscribe();
        Box::pin(async move {
            rx.wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|outcome| outcome.clone())
                .unwrap_or_else(|| failed_outcome("录制任务意外结束".into()))
        })
    }
}

type Hook = Box<dyn Fn() + Send + Sync>;

struct FakeEngine {
    ffmpeg: Mutex<Option<PathBuf>>,
    /// 为 false 时启动后不发 `started`。
    emit_started: AtomicBool,
    /// 设置后 `start` 要取到许可才返回，模拟 ffmpeg 还在启动。
    start_gate: Mutex<Option<Arc<Semaphore>>>,
    panic_on_start: AtomicBool,
    /// `stop_all` 永不返回，模拟收尾卡住。
    stop_all_hangs: AtomicBool,
    /// 查询 ffmpeg 路径时执行，用来在「检测到开播之后、占位之前」插入用户操作。
    on_ffmpeg_path: Mutex<Option<Hook>>,
    recordings: Mutex<Vec<FakeRecording>>,
    stop_all_calls: AtomicUsize,
    /// 录制结果里的文件，缺省为不存在的 `rec_000.ts`（不会触发转 MP4）。
    files: Mutex<Option<Vec<PathBuf>>>,
}

impl Default for FakeEngine {
    fn default() -> Self {
        FakeEngine {
            ffmpeg: Mutex::new(Some(PathBuf::from("ffmpeg.exe"))),
            emit_started: AtomicBool::new(true),
            start_gate: Mutex::default(),
            panic_on_start: AtomicBool::new(false),
            stop_all_hangs: AtomicBool::new(false),
            on_ffmpeg_path: Mutex::default(),
            recordings: Mutex::default(),
            stop_all_calls: AtomicUsize::new(0),
            files: Mutex::default(),
        }
    }
}

impl FakeEngine {
    fn starts(&self) -> usize {
        self.recordings.lock().unwrap().len()
    }

    fn request(&self, index: usize) -> RecordingRequest {
        self.recordings.lock().unwrap()[index].request.clone()
    }

    fn shared(&self, index: usize) -> Arc<Shared> {
        Arc::clone(&self.recordings.lock().unwrap()[index].shared)
    }

    fn stopped(&self, index: usize) -> bool {
        self.shared(index).stopped.load(Ordering::SeqCst)
    }

    fn emit(&self, index: usize, event: RecorderEvent) {
        (self.shared(index).listener)(&event);
    }

    /// 引擎自己结束录制（断流、磁盘满等）。
    fn end(&self, index: usize, reason: EndReason) {
        self.shared(index).finish(reason);
    }

    fn gate_starts(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *self.start_gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }
}

fn started_event() -> RecorderEvent {
    RecorderEvent::Started {
        output_dir: PathBuf::from("out"),
        output_path: PathBuf::from("out/rec_%03d.ts"),
        source_format: crate::platforms::SourceFormat::Flv,
        segmented: true,
        pid: Some(1),
        started_at_ms: now_ms(),
    }
}

impl RecordEngine for Arc<FakeEngine> {
    fn ffmpeg_path(&self) -> Option<PathBuf> {
        if let Some(hook) = self.on_ffmpeg_path.lock().unwrap().take() {
            hook();
        }
        self.ffmpeg.lock().unwrap().clone()
    }

    fn start(
        &self,
        request: RecordingRequest,
        listener: EventListener,
    ) -> BoxFuture<'static, Arc<dyn RecordingControl>> {
        let engine = Arc::clone(self);
        Box::pin(async move {
            let shared = Arc::new(Shared {
                listener,
                stopped: AtomicBool::new(false),
                finished: AtomicBool::new(false),
                returned: AtomicBool::new(false),
                outcome: tokio::sync::watch::channel(None).0,
                files: engine
                    .files
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| vec![PathBuf::from("rec_000.ts")]),
            });
            engine.recordings.lock().unwrap().push(FakeRecording {
                request,
                shared: Arc::clone(&shared),
            });
            if engine.panic_on_start.load(Ordering::SeqCst) {
                panic!("假录制引擎 panic");
            }
            let gate = engine.start_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            if engine.emit_started.load(Ordering::SeqCst) {
                (shared.listener)(&started_event());
            }
            shared.returned.store(true, Ordering::SeqCst);
            Arc::new(FakeControl(shared)) as Arc<dyn RecordingControl>
        })
    }

    fn stop_all(&self) -> BoxFuture<'static, ()> {
        self.stop_all_calls.fetch_add(1, Ordering::SeqCst);
        let engine = Arc::clone(self);
        Box::pin(async move {
            if engine.stop_all_hangs.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            let active: Vec<Arc<Shared>> = engine
                .recordings
                .lock()
                .unwrap()
                .iter()
                .filter(|recording| recording.shared.returned.load(Ordering::SeqCst))
                .map(|recording| Arc::clone(&recording.shared))
                .collect();
            for shared in active {
                shared.stopped.store(true, Ordering::SeqCst);
                shared.finish(EndReason::UserStopped);
            }
        })
    }
}

// ---- 测试装置 ----

struct Harness {
    _dir: tempfile::TempDir,
    tasks: Arc<TaskManager>,
    source: Arc<FakeSource>,
    engine: Arc<FakeEngine>,
    scheduler: Arc<Scheduler>,
    statuses: Arc<Mutex<Vec<SchedulerStatus>>>,
    /// 每次推送任务快照时的（任务、录制状态、当时调度占用的录制位置数）。
    record_writes: Arc<Mutex<Vec<(TaskId, RecordState, usize)>>>,
}

const INTERVAL: Duration = Duration::from_secs(180);

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("lubobao.db")).unwrap();
    let record_writes: Arc<Mutex<Vec<(TaskId, RecordState, usize)>>> = Arc::default();
    let scheduler_cell: Arc<std::sync::OnceLock<std::sync::Weak<Scheduler>>> = Arc::default();
    let (writes, cell) = (Arc::clone(&record_writes), Arc::clone(&scheduler_cell));
    let listener: crate::tasks::Listener = Box::new(move |event| {
        if let crate::tasks::TaskEvent::Updated(task) = event {
            let slots = cell
                .get()
                .and_then(std::sync::Weak::upgrade)
                .map_or(0, |scheduler| scheduler.status().recording);
            writes
                .lock()
                .unwrap()
                .push((task.id, task.record.clone(), slots));
        }
    });
    let tasks = Arc::new(TaskManager::open(store, &dir.path().join("videos"), listener).unwrap());
    let source = Arc::new(FakeSource::default());
    let engine = Arc::new(FakeEngine::default());
    let statuses: Arc<Mutex<Vec<SchedulerStatus>>> = Arc::default();
    let sink = Arc::clone(&statuses);
    let scheduler = Scheduler::new(
        Arc::clone(&tasks),
        Arc::new(Arc::clone(&source)),
        Arc::new(Arc::clone(&engine)),
        Box::new(move |status: &SchedulerStatus| sink.lock().unwrap().push(status.clone())),
    );
    let _ = scheduler_cell.set(Arc::downgrade(&scheduler));
    Harness {
        _dir: dir,
        tasks,
        source,
        engine,
        scheduler,
        statuses,
        record_writes,
    }
}

impl Harness {
    fn add(&self, room: &str, reply: Reply, notify_only: bool) -> (TaskId, String) {
        let url = format!("https://live.douyin.com/{room}");
        let is_live = matches!(reply, Reply::Live | Reply::LiveNoStream);
        self.source.set(&url, reply);
        let input = AddTaskInput {
            url: url.clone(),
            notify_only: Some(notify_only),
            ..AddTaskInput::default()
        };
        match self
            .tasks
            .add_resolved(input, &live_info(&url, is_live))
            .unwrap()
        {
            TaskMutation::Added { task } => (task.id, url),
            other => panic!("expected added, got {other:?}"),
        }
    }

    fn start(&self) {
        tokio::spawn(self.scheduler.run());
    }

    fn task(&self, id: TaskId) -> TaskSnapshot {
        self.tasks.get(id).unwrap()
    }

    fn update_settings(&self, value: Value) {
        let Value::Object(map) = value else { panic!() };
        let map: Map<String, Value> = map;
        self.tasks.update_settings(map).unwrap();
    }
}

/// 在虚拟时间里等条件成立，最多等 `limit`。
async fn until(limit: Duration, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        sleep(Duration::from_millis(10)).await;
    }
}

const SHORT: Duration = Duration::from_secs(5);

// ---- 测试 ----

#[tokio::test(start_paused = true)]
async fn waits_while_offline_and_polls_at_the_interval() {
    let h = harness();
    let (id, url) = h.add("1001", Reply::Offline, false);
    let t0 = Instant::now();
    h.start();
    until(SHORT, "首次检测", || h.source.calls(&url).len() == 1).await;
    // 新任务立即检测，不等满一个间隔。
    assert!(h.source.calls(&url)[0] - t0 < Duration::from_secs(1));
    until(SHORT, "检测完成", || {
        matches!(h.task(id).live, LiveState::Offline { .. })
    })
    .await;
    assert_eq!(h.task(id).display, DisplayStatus::Waiting);

    sleep(INTERVAL - Duration::from_secs(10)).await;
    assert_eq!(h.source.calls(&url).len(), 1, "间隔未到不应再检测");
    until(Duration::from_secs(20), "第二次检测", || {
        h.source.calls(&url).len() == 2
    })
    .await;
    let calls = h.source.calls(&url);
    assert_eq!((calls[1] - calls[0]).as_secs(), INTERVAL.as_secs());
    assert_eq!(h.engine.starts(), 0);
}

#[tokio::test(start_paused = true)]
async fn starts_recording_when_live_with_settings_and_tracks_progress() {
    let h = harness();
    h.update_settings(json!({
        "live_save_path": "D:/录像",
        "video_segment_time": 600,
        "proxy_address": "127.0.0.1:7890",
        "default_platform_with_proxy": "douyin",
    }));
    let (id, url) = h.add("1002", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    let request = h.engine.request(0);
    assert_eq!(request.ffmpeg_path, PathBuf::from("ffmpeg.exe"));
    assert_eq!(request.output_dir, PathBuf::from("D:/录像"));
    // 抖音默认录 FLV，强制 HTTPS。
    assert_eq!(request.source_url, "https://cdn.example/1002.flv");
    assert_eq!(request.platform, "抖音");
    assert_eq!(request.anchor_name, "主播1002");
    assert_eq!(request.options.segment_secs, 600);
    assert_eq!(
        request.options.proxy.as_deref(),
        Some("http://127.0.0.1:7890")
    );
    assert_eq!(request.headers["Referer"], "https://live.douyin.com/");

    let task = h.task(id);
    assert!(matches!(task.record, RecordState::Recording { .. }));
    assert_eq!(task.display, DisplayStatus::Recording);

    h.engine.emit(
        0,
        RecorderEvent::Progress {
            progress: Progress {
                size_bytes: Some(4096),
                duration_secs: Some(2.0),
                bitrate_kbps: Some(16.0),
                speed: Some(1.0),
            },
        },
    );
    let RecordState::Recording {
        bytes_written,
        bitrate_kbps,
        ..
    } = h.task(id).record
    else {
        panic!("应在录制中");
    };
    assert_eq!((bytes_written, bitrate_kbps), (4096, 16.0));

    // 录制期间不再检测。
    sleep(INTERVAL * 3).await;
    assert_eq!(h.source.calls(&url).len(), 1);
    assert_eq!(h.engine.starts(), 1);
}

#[tokio::test(start_paused = true)]
async fn two_live_detections_start_only_one_recording() {
    let h = harness();
    // 不发 started：录制状态还是空闲，只靠调度的占位挡住第二次启动。
    h.engine.emit_started.store(false, Ordering::SeqCst);
    let (id, _) = h.add("1003", Reply::Live, false);
    let task = h.task(id);
    let core = Arc::clone(&h.scheduler.core);
    tokio::join!(
        Arc::clone(&core).check(task.clone()),
        Arc::clone(&core).check(task.clone())
    );
    Arc::clone(&core).check(task.clone()).await;
    assert_eq!(h.engine.starts(), 1);
    assert_eq!(h.scheduler.status().recording, 1);

    // 录制开始后再检测到开播，也不会再启动。
    h.engine.emit(
        0,
        RecorderEvent::Started {
            output_dir: PathBuf::from("out"),
            output_path: PathBuf::from("out/a.ts"),
            source_format: crate::platforms::SourceFormat::Flv,
            segmented: false,
            pid: None,
            started_at_ms: now_ms(),
        },
    );
    Arc::clone(&core).check(h.task(id)).await;
    assert_eq!(h.engine.starts(), 1);
}

#[tokio::test(start_paused = true)]
async fn rechecks_after_stream_end_and_resumes_recording() {
    let h = harness();
    let (id, url) = h.add("1004", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    sleep(Duration::from_secs(120)).await;

    // 录了两分钟后断流：立即重新检测，仍在播就续录。
    h.engine.end(0, EndReason::StreamEnded);
    until(SHORT, "续录", || h.engine.starts() == 2).await;
    assert_eq!(h.source.calls(&url).len(), 2);
    let calls = h.source.calls(&url);
    assert!(calls[1] - calls[0] >= Duration::from_secs(120));
    assert!(matches!(h.task(id).record, RecordState::Recording { .. }));

    // 刚开始就断：先退避 5 秒再检测。
    h.engine.end(1, EndReason::NetworkError);
    sleep(Duration::from_secs(4)).await;
    assert_eq!(h.source.calls(&url).len(), 2, "秒断后应先退避");
    until(SHORT, "退避后续录", || h.engine.starts() == 3).await;

    // 断流后已下播：回到等待开播，按间隔检测。
    h.source.set(&url, Reply::Offline);
    sleep(Duration::from_secs(120)).await;
    h.engine.end(2, EndReason::StreamEnded);
    until(SHORT, "下播", || {
        matches!(h.task(id).live, LiveState::Offline { .. })
    })
    .await;
    assert_eq!(h.task(id).record, RecordState::Idle);
    assert_eq!(h.task(id).display, DisplayStatus::Waiting);
    assert_eq!(h.engine.starts(), 3);
}

#[tokio::test(start_paused = true)]
async fn user_stop_stays_stopped_until_the_stream_goes_offline() {
    let h = harness();
    let (id, url) = h.add("1005", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;

    let snapshots = h.tasks.stop_recording(&[id]).unwrap();
    assert!(snapshots[0].record_held);
    assert!(h.engine.stopped(0));
    until(SHORT, "收尾", || h.task(id).record == RecordState::Idle).await;
    let task = h.task(id);
    assert_eq!(task.display, DisplayStatus::Notify);
    assert_eq!(task.display_reason.as_deref(), Some(HELD_REASON));

    // 仍在播：照常检测，但不再录制。
    sleep(INTERVAL * 2 + SHORT).await;
    assert_eq!(h.source.calls(&url).len(), 3);
    assert_eq!(h.engine.starts(), 1);
    assert!(h.task(id).record_held);

    // 下播后保持状态结束，再开播照常录制。
    h.source.set(&url, Reply::Offline);
    until(INTERVAL + SHORT, "下播", || {
        matches!(h.task(id).live, LiveState::Offline { .. })
    })
    .await;
    assert!(!h.task(id).record_held);
    h.source.set(&url, Reply::Live);
    until(INTERVAL + SHORT, "再开播", || h.engine.starts() == 2).await;
}

#[tokio::test(start_paused = true)]
async fn reenabling_monitoring_clears_the_user_stop() {
    let h = harness();
    let (id, _) = h.add("1006", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    h.tasks.stop_recording(&[id]).unwrap();
    until(SHORT, "收尾", || h.task(id).record == RecordState::Idle).await;

    h.tasks.set_monitoring(&[id], false).unwrap();
    assert_eq!(h.task(id).display, DisplayStatus::Paused);
    h.tasks.set_monitoring(&[id], true).unwrap();
    assert!(!h.task(id).record_held);
    // 重新启用后立即检测，不等间隔。
    until(SHORT, "重新录制", || h.engine.starts() == 2).await;
}

#[tokio::test(start_paused = true)]
async fn pausing_monitoring_stops_recording_and_polling() {
    let h = harness();
    let (id, url) = h.add("1007", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    h.tasks.set_monitoring(&[id], false).unwrap();
    assert!(h.engine.stopped(0));
    until(SHORT, "收尾", || h.task(id).record == RecordState::Idle).await;
    assert_eq!(h.task(id).display, DisplayStatus::Paused);
    sleep(INTERVAL * 2).await;
    assert_eq!(h.source.calls(&url).len(), 1);
    assert_eq!(h.engine.starts(), 1);
}

#[tokio::test(start_paused = true)]
async fn notify_only_tasks_stay_live_without_recording() {
    let h = harness();
    let (id, url) = h.add("1008", Reply::Live, true);
    h.start();
    until(SHORT, "首次检测", || h.source.calls(&url).len() == 1).await;
    until(SHORT, "检测完成", || {
        matches!(h.task(id).live, LiveState::Live { .. })
    })
    .await;
    assert_eq!(h.task(id).display, DisplayStatus::Notify);
    until(INTERVAL + SHORT, "第二次检测", || {
        h.source.calls(&url).len() == 2
    })
    .await;
    assert_eq!(h.engine.starts(), 0);
    assert_eq!(h.task(id).record, RecordState::Idle);
}

#[tokio::test(start_paused = true)]
async fn retryable_failures_back_off_and_record_the_error() {
    let h = harness();
    let (id, url) = h.add(
        "1009",
        Reply::Fail(PlatformError::Network("超时".into())),
        false,
    );
    h.start();
    until(Duration::from_secs(400), "五次检测", || {
        h.source.calls(&url).len() == 5
    })
    .await;
    let calls = h.source.calls(&url);
    let gaps: Vec<u64> = calls.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
    assert_eq!(gaps, vec![10, 20, 40, 80]);
    until(SHORT, "第五次失败", || {
        matches!(h.task(id).live, LiveState::CheckFailed { attempt: 5, .. })
    })
    .await;
    let task = h.task(id);
    let LiveState::CheckFailed {
        kind,
        message,
        failed_at,
        next_retry_at,
        ..
    } = &task.live
    else {
        unreachable!()
    };
    assert_eq!(*kind, PlatformErrorKind::Network);
    assert_eq!(message, "超时");
    assert_eq!(next_retry_at.unwrap() - failed_at, 160_000);
    assert_eq!(task.display, DisplayStatus::Retrying);

    // 封顶为检测间隔。
    until(Duration::from_secs(400), "第七次检测", || {
        h.source.calls(&url).len() == 7
    })
    .await;
    let calls = h.source.calls(&url);
    assert_eq!((calls[6] - calls[5]).as_secs(), 180);

    // 恢复后计数清零，回到正常间隔。
    h.source.set(&url, Reply::Offline);
    until(INTERVAL + SHORT, "恢复", || {
        matches!(h.task(id).live, LiveState::Offline { .. })
    })
    .await;
    h.source
        .set(&url, Reply::Fail(PlatformError::Network("超时".into())));
    until(INTERVAL + SHORT, "再次失败", || {
        matches!(h.task(id).live, LiveState::CheckFailed { attempt: 1, .. })
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn failures_needing_the_user_retry_slowly_and_after_settings_change() {
    let h = harness();
    let (id, url) = h.add(
        "1010",
        Reply::Fail(PlatformError::AuthRequired("需要登录".into())),
        false,
    );
    h.start();
    until(SHORT, "首次检测", || h.source.calls(&url).len() == 1).await;
    until(SHORT, "失败", || {
        matches!(h.task(id).live, LiveState::CheckFailed { .. })
    })
    .await;
    assert_eq!(h.task(id).display, DisplayStatus::NeedsAction);
    sleep(Duration::from_secs(590)).await;
    assert_eq!(h.source.calls(&url).len(), 1, "需处理的失败不应高频重试");
    until(Duration::from_secs(20), "十分钟后重试", || {
        h.source.calls(&url).len() == 2
    })
    .await;

    // 用户改了设置：立即重试。
    h.update_settings(json!({ "proxy_address": "127.0.0.1:7890" }));
    until(SHORT, "改设置后重试", || {
        h.source.calls(&url).len() == 3
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn respects_the_per_platform_concurrency_limit() {
    let h = harness();
    h.update_settings(json!({ "platform_max_concurrent_requests": 2 }));
    let gate = Arc::new(Semaphore::new(0));
    *h.source.gate.lock().unwrap() = Some(Arc::clone(&gate));
    for room in 0..5 {
        h.add(&format!("20{room}"), Reply::Offline, false);
    }
    h.start();
    until(SHORT, "两个请求", || h.source.total_calls() == 2).await;
    sleep(Duration::from_secs(30)).await;
    assert_eq!(h.source.total_calls(), 2, "超出并发上限的任务应排队");
    assert_eq!(h.scheduler.status().checking, 2);

    gate.add_permits(100);
    until(SHORT, "全部检测", || h.source.total_calls() == 5).await;
    assert_eq!(h.source.max.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn interval_changes_take_effect_from_the_next_round() {
    let h = harness();
    let (_, url) = h.add("1011", Reply::Offline, false);
    h.start();
    until(SHORT, "首次检测", || h.source.calls(&url).len() == 1).await;
    sleep(Duration::from_secs(10)).await;
    h.update_settings(json!({ "loop_time_seconds": 30 }));
    until(Duration::from_secs(300), "第三次检测", || {
        h.source.calls(&url).len() == 3
    })
    .await;
    let calls = h.source.calls(&url);
    assert_eq!((calls[1] - calls[0]).as_secs(), 180);
    assert_eq!((calls[2] - calls[1]).as_secs(), 30);
}

#[tokio::test(start_paused = true)]
async fn recording_failures_need_attention_and_do_not_restart() {
    let h = harness();
    let (id, url) = h.add("1012", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    h.engine.end(0, EndReason::DiskFull);
    until(SHORT, "失败", || {
        matches!(h.task(id).record, RecordState::Failed { .. })
    })
    .await;
    let task = h.task(id);
    assert_eq!(task.display, DisplayStatus::NeedsAction);
    assert_eq!(task.display_reason.as_deref(), Some("磁盘空间不足"));
    until(INTERVAL + SHORT, "照常检测", || {
        h.source.calls(&url).len() == 2
    })
    .await;
    sleep(SHORT).await;
    assert_eq!(h.engine.starts(), 1);

    // 用户处理后重新启用监控，恢复录制。
    h.tasks.set_monitoring(&[id], false).unwrap();
    h.tasks.set_monitoring(&[id], true).unwrap();
    until(SHORT, "恢复录制", || h.engine.starts() == 2).await;
}

#[tokio::test(start_paused = true)]
async fn waits_for_ffmpeg_before_recording() {
    let h = harness();
    *h.engine.ffmpeg.lock().unwrap() = None;
    let (id, url) = h.add("1013", Reply::Live, false);
    h.start();
    until(SHORT, "首次检测", || h.source.calls(&url).len() == 1).await;
    sleep(SHORT).await;
    assert_eq!(h.engine.starts(), 0);
    assert_eq!(h.task(id).display, DisplayStatus::Checking);
    *h.engine.ffmpeg.lock().unwrap() = Some(PathBuf::from("ffmpeg.exe"));
    until(Duration::from_secs(20), "装好后录制", || {
        h.engine.starts() == 1
    })
    .await;
    let calls = h.source.calls(&url);
    assert_eq!((calls[1] - calls[0]).as_secs(), 15);
}

#[tokio::test(start_paused = true)]
async fn shutdown_finishes_recordings_and_stops_polling() {
    let h = harness();
    let (first, first_url) = h.add("1014", Reply::Live, false);
    let (second, _) = h.add("1015", Reply::Live, false);
    let (_, offline_url) = h.add("1016", Reply::Offline, false);
    h.start();
    until(SHORT, "两个录制", || h.engine.starts() == 2).await;
    assert_eq!(h.scheduler.status().recording, 2);

    h.scheduler.shutdown().await;
    assert_eq!(h.engine.stop_all_calls.load(Ordering::SeqCst), 1);
    // shutdown 返回时结束状态已经写回。
    assert_eq!(h.task(first).record, RecordState::Idle);
    assert_eq!(h.task(second).record, RecordState::Idle);
    assert_eq!(h.scheduler.status().recording, 0);

    let calls = h.source.total_calls();
    sleep(INTERVAL * 3).await;
    assert_eq!(h.source.total_calls(), calls, "退出后不再检测");
    assert_eq!(h.engine.starts(), 2);
    assert_eq!(h.source.calls(&first_url).len(), 1);
    assert_eq!(h.source.calls(&offline_url).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn reports_the_next_check_time() {
    let h = harness();
    assert_eq!(h.scheduler.status().next_check_at, None);
    let (_, url) = h.add("1018", Reply::Offline, false);
    h.start();
    until(SHORT, "首次检测", || h.source.calls(&url).len() == 1).await;
    until(SHORT, "排期", || {
        h.scheduler.status().next_check_at.is_some()
    })
    .await;
    let remaining = h.scheduler.status().next_check_at.unwrap() - now_ms();
    assert!((170_000..=181_000).contains(&remaining), "{remaining}");
    let statuses = h.statuses.lock().unwrap().clone();
    assert!(statuses.iter().any(|status| status.next_check_at.is_some()));
    assert!(statuses.iter().any(|status| status.checking == 1));
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_a_recording_that_is_still_starting() {
    let h = harness();
    let gate = h.engine.gate_starts();
    let (id, _) = h.add("1017", Reply::Live, false);
    h.start();
    until(SHORT, "启动中", || h.engine.starts() == 1).await;
    // 退出时 ffmpeg 还在启动：stop_all 看不到它，要靠调度在启动返回后停止。
    let scheduler = Arc::clone(&h.scheduler);
    let shutdown = tokio::spawn(async move { scheduler.shutdown().await });
    sleep(Duration::from_secs(1)).await;
    assert!(!shutdown.is_finished(), "录制还没结束，shutdown 不应返回");
    gate.add_permits(1);
    shutdown.await.unwrap();
    assert!(h.engine.stopped(0));
    assert_eq!(h.scheduler.status().recording, 0);
    assert_eq!(h.task(id).record, RecordState::Idle);
}

#[tokio::test(start_paused = true)]
async fn writes_the_final_state_before_shutdown_can_return() {
    let h = harness();
    let gate = h.engine.gate_starts();
    let (id, _) = h.add("1020", Reply::Live, false);
    h.start();
    until(SHORT, "启动中", || h.engine.starts() == 1).await;
    let scheduler = Arc::clone(&h.scheduler);
    let shutdown = tokio::spawn(async move { scheduler.shutdown().await });
    sleep(Duration::from_secs(1)).await;
    gate.add_permits(1);
    shutdown.await.unwrap();
    // 写入空闲状态的那一刻，占位必须还在：shutdown 等的是占位清空，清空时状态已经落盘。
    let writes = h.record_writes.lock().unwrap().clone();
    let idle = writes
        .iter()
        .rev()
        .find(|(task, record, _)| *task == id && *record == RecordState::Idle)
        .expect("应写入空闲状态");
    assert!(
        writes
            .iter()
            .any(|(task, record, _)| *task == id && matches!(record, RecordState::Recording { .. })),
        "应先进入录制中"
    );
    assert_eq!(idle.2, 1, "写入结束状态时占位已被释放");
}

#[tokio::test(start_paused = true)]
async fn deleting_a_task_stops_its_starting_recording() {
    let h = harness();
    let gate = h.engine.gate_starts();
    let (id, _) = h.add("1019", Reply::Live, false);
    h.start();
    until(SHORT, "启动中", || h.engine.starts() == 1).await;
    // 录制状态还是空闲，删除不会被拒绝，但调度要在启动返回后停掉这个录制。
    assert_eq!(h.tasks.delete_tasks(&[id]).unwrap(), vec![id]);
    gate.add_permits(1);
    until(SHORT, "停止", || h.engine.stopped(0)).await;
    until(SHORT, "释放", || h.scheduler.status().recording == 0).await;
}

#[tokio::test(start_paused = true)]
async fn pausing_between_live_detection_and_start_does_not_record() {
    let h = harness();
    let (id, url) = h.add("1021", Reply::Live, false);
    let tasks = Arc::clone(&h.tasks);
    // 检测已写入「已开播」、还没占位时，用户点了暂停。
    *h.engine.on_ffmpeg_path.lock().unwrap() = Some(Box::new(move || {
        tasks.set_monitoring(&[id], false).unwrap();
    }));
    h.start();
    until(SHORT, "检测", || h.source.calls(&url).len() == 1).await;
    sleep(SHORT).await;
    assert_eq!(h.engine.starts(), 0, "已暂停的任务不应开始录制");
    assert_eq!(h.scheduler.status().recording, 0);
    assert_eq!(h.task(id).display, DisplayStatus::Paused);
}

#[tokio::test(start_paused = true)]
async fn switching_to_notify_only_before_start_does_not_record() {
    let h = harness();
    let (id, url) = h.add("1022", Reply::Live, false);
    let tasks = Arc::clone(&h.tasks);
    *h.engine.on_ffmpeg_path.lock().unwrap() = Some(Box::new(move || {
        let input = crate::tasks::UpdateTaskInput {
            id,
            notify_only: Some(true),
            ..Default::default()
        };
        tasks.update_resolved(input, None).unwrap();
    }));
    h.start();
    until(SHORT, "检测", || h.source.calls(&url).len() == 1).await;
    sleep(SHORT).await;
    assert_eq!(h.engine.starts(), 0);
    assert_eq!(h.scheduler.status().recording, 0);
    assert_eq!(h.task(id).display, DisplayStatus::Notify);
}

#[tokio::test(start_paused = true)]
async fn a_panicking_adapter_does_not_block_its_platform() {
    let h = harness();
    h.update_settings(json!({ "platform_max_concurrent_requests": 1 }));
    let (bad, bad_url) = h.add("1023", Reply::Panic, false);
    let (_, good_url) = h.add("1024", Reply::Offline, false);
    h.start();
    // 并发上限为 1：如果 panic 不归还名额，另一个任务永远轮不到。
    until(SHORT, "另一个任务被检测", || {
        h.source.calls(&good_url).len() == 1
    })
    .await;
    until(
        Duration::from_secs(60),
        "panic 的任务退避重试",
        || h.source.calls(&bad_url).len() >= 3,
    )
    .await;
    let task = h.task(bad);
    let LiveState::CheckFailed { kind, message, .. } = &task.live else {
        panic!("应为检测失败：{:?}", task.live);
    };
    assert_eq!(*kind, PlatformErrorKind::Parse);
    assert!(message.contains("内部错误"), "{message}");
    assert_eq!(task.display, DisplayStatus::Retrying);
    until(INTERVAL + SHORT, "另一个任务照常检测", || {
        h.source.calls(&good_url).len() == 2
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_panicking_engine_start_releases_the_slot() {
    let h = harness();
    h.engine.panic_on_start.store(true, Ordering::SeqCst);
    let (id, _) = h.add("1025", Reply::Live, false);
    h.start();
    until(SHORT, "启动", || h.engine.starts() == 1).await;
    until(SHORT, "失败", || {
        matches!(h.task(id).record, RecordState::Failed { .. })
    })
    .await;
    assert_eq!(h.scheduler.status().recording, 0);
    assert_eq!(h.task(id).display, DisplayStatus::NeedsAction);
}

#[tokio::test(start_paused = true)]
async fn a_crashed_recorder_without_finished_releases_the_slot() {
    let h = harness();
    let (id, _) = h.add("1026", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    until(SHORT, "录制中", || {
        matches!(h.task(id).record, RecordState::Recording { .. })
    })
    .await;
    // 引擎内部任务崩溃，没有发出 finished。
    h.engine.shared(0).crash();
    until(SHORT, "失败", || {
        matches!(h.task(id).record, RecordState::Failed { .. })
    })
    .await;
    assert_eq!(h.scheduler.status().recording, 0);
}

#[tokio::test(start_paused = true)]
async fn reenabling_while_the_stopped_recording_finishes_rechecks_at_once() {
    let h = harness();
    let (id, _) = h.add("1027", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    // 暂停（录制随之停止）后马上恢复，录制还在收尾。
    h.tasks.set_monitoring(&[id], false).unwrap();
    h.tasks.set_monitoring(&[id], true).unwrap();
    until(SHORT, "立即续录", || h.engine.starts() == 2).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_time_limit_includes_stop_all() {
    let h = harness();
    h.engine.stop_all_hangs.store(true, Ordering::SeqCst);
    let (_, _) = h.add("1028", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(120), h.scheduler.shutdown())
        .await
        .expect("shutdown 应在时限内返回");
    assert!(
        started.elapsed() <= SHUTDOWN_TIMEOUT,
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn live_without_a_stream_backs_off() {
    let h = harness();
    let (id, url) = h.add("1029", Reply::LiveNoStream, false);
    h.start();
    until(Duration::from_secs(120), "四次检测", || {
        h.source.calls(&url).len() == 4
    })
    .await;
    let calls = h.source.calls(&url);
    let gaps: Vec<u64> = calls.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
    assert_eq!(gaps, vec![10, 20, 40]);
    until(SHORT, "第四次失败", || {
        matches!(h.task(id).live, LiveState::CheckFailed { attempt: 4, .. })
    })
    .await;
    assert_eq!(h.engine.starts(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_socks_proxy_is_used_for_checks_but_recording_goes_direct() {
    let h = harness();
    h.update_settings(json!({
        "proxy_address": "socks5://127.0.0.1:1080",
        "default_platform_with_proxy": "douyin",
    }));
    let (id, _) = h.add("1030", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    assert_eq!(h.engine.request(0).options.proxy, None);
    assert!(matches!(h.task(id).record, RecordState::Recording { .. }));
    let warnings = h
        .tasks
        .list_activity(&crate::tasks::ActivityQuery {
            kinds: vec![ActivityKind::Warning],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(warnings.items.len(), 1);
    assert_eq!(
        warnings.items[0].message,
        "录制不支持 SOCKS5 代理，已直连录制（检测仍使用代理）"
    );
}

#[tokio::test(start_paused = true)]
async fn checks_send_the_platform_cookie_and_only_listed_platforms_use_the_proxy() {
    let h = harness();
    h.update_settings(json!({
        "proxy_address": "127.0.0.1:7890",
        "platform_cookies": { "douyin": " ttwid=abc ", "kuaishou": "did=1" },
    }));
    let (_, url) = h.add("1031", Reply::Offline, false);
    h.start();
    until(SHORT, "首次检测", || h.source.calls(&url).len() == 1).await;
    let first = h.source.options.lock().unwrap()[0].clone();
    assert_eq!(first.cookies.as_deref(), Some("ttwid=abc"));
    // 默认列表只有海外平台：抖音直连。
    assert_eq!(first.proxy, None);

    h.update_settings(json!({
        "default_platform_with_proxy": "tiktok, douyin",
        "platform_cookies": {},
    }));
    until(INTERVAL + SHORT, "第二次检测", || {
        h.source.calls(&url).len() == 2
    })
    .await;
    let second = h.source.options.lock().unwrap()[1].clone();
    assert_eq!(second.proxy.as_deref(), Some("127.0.0.1:7890"));
    assert_eq!(second.cookies, None);
}

#[test]
fn rejects_cookies_that_cannot_be_sent_as_a_header() {
    let h = harness();
    let mut map = Map::new();
    map.insert(
        "platform_cookies".into(),
        json!({ "douyin": "a=1
b=2" }),
    );
    let error = h.tasks.update_settings(map).unwrap_err();
    assert_eq!(error.kind, crate::tasks::ErrorKind::Config);
    assert_eq!(h.tasks.settings()["platform_cookies"], json!({}));
}

// ---- 录后转 MP4 ----

fn test_ffmpeg() -> Option<PathBuf> {
    match std::env::var_os("LUBOBAO_TEST_FFMPEG") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => {
            eprintln!("跳过：未设置 LUBOBAO_TEST_FFMPEG（ffmpeg 可执行文件路径）");
            None
        }
    }
}

fn make_ts(ffmpeg: &std::path::Path, path: &std::path::Path) {
    let output = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=size=160x120:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440"])
        .args(["-t", "2", "-c:v", "libx264", "-preset", "veryfast"])
        .args(["-pix_fmt", "yuv420p", "-c:a", "aac", "-f", "mpegts"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success());
}

fn activity_of(h: &Harness, kind: ActivityKind) -> Vec<crate::tasks::ActivityEntry> {
    h.tasks
        .list_activity(&crate::tasks::ActivityQuery {
            kinds: vec![kind],
            ..Default::default()
        })
        .unwrap()
        .items
}

/// 准备两个真实的 TS 分段，录制结束后交给转换队列。用真实时钟（ffmpeg 是真实进程）。
fn conversion_harness(ffmpeg: &std::path::Path) -> (Harness, Vec<PathBuf>) {
    let h = harness();
    let dir = h._dir.path().join("videos").join("抖音").join("主播 甲");
    std::fs::create_dir_all(&dir).unwrap();
    let files: Vec<PathBuf> = (0..2)
        .map(|n| dir.join(format!("主播 甲_2026-11-22_12-00-00_{n:03}.ts")))
        .collect();
    for file in &files {
        make_ts(ffmpeg, file);
    }
    *h.engine.ffmpeg.lock().unwrap() = Some(ffmpeg.to_path_buf());
    *h.engine.files.lock().unwrap() = Some(files.clone());
    (h, files)
}

#[tokio::test]
async fn finished_recordings_are_converted_to_mp4_with_activity() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let (h, files) = conversion_harness(&ffmpeg);
    h.update_settings(json!({ "delete_original": true }));
    let (id, _) = h.add("2001", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    h.tasks.stop_recording(&[id]).unwrap();
    until(Duration::from_secs(60), "转换完成", || {
        activity_of(&h, ActivityKind::Convert).len() == 4
    })
    .await;

    // 新的在前：完成、开始、完成、开始。
    let entries = activity_of(&h, ActivityKind::Convert);
    let messages: Vec<&str> = entries.iter().map(|entry| entry.message.as_str()).collect();
    assert_eq!(
        messages,
        [
            "已转为 MP4，已删除原 TS 文件",
            "开始转为 MP4",
            "已转为 MP4，已删除原 TS 文件",
            "开始转为 MP4",
        ]
    );
    assert!(entries.iter().all(|entry| entry.task_id == Some(id)));
    for file in &files {
        assert!(!file.exists());
        assert!(file.with_extension("mp4").is_file());
    }
    assert!(
        activity_of(&h, ActivityKind::RecordEnd)[0].id < entries.last().unwrap().id,
        "录制结束的动态应在转换之前"
    );
    assert_eq!(h.scheduler.post_processor().pending(), 0);
}

#[tokio::test]
async fn no_conversion_when_disabled_or_the_disk_is_full() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let (h, files) = conversion_harness(&ffmpeg);
    h.update_settings(json!({ "convert_to_mp4": false }));
    let (id, url) = h.add("2002", Reply::Live, false);
    h.start();
    until(SHORT, "开始录制", || h.engine.starts() == 1).await;
    h.tasks.stop_recording(&[id]).unwrap();
    until(SHORT, "收尾", || h.task(id).record == RecordState::Idle).await;

    // 打开转换后因磁盘不足结束：也不转换。
    h.update_settings(json!({ "convert_to_mp4": true }));
    h.tasks.set_monitoring(&[id], false).unwrap();
    h.source.set(&url, Reply::Live);
    h.tasks.set_monitoring(&[id], true).unwrap();
    until(SHORT, "再次录制", || h.engine.starts() == 2).await;
    h.engine.end(1, EndReason::DiskFull);
    until(SHORT, "需处理", || {
        matches!(h.task(id).record, RecordState::Failed { .. })
    })
    .await;
    sleep(Duration::from_millis(500)).await;
    assert!(activity_of(&h, ActivityKind::Convert).is_empty());
    assert_eq!(h.scheduler.post_processor().pending(), 0);
    for file in &files {
        assert!(file.exists());
        assert!(!file.with_extension("mp4").exists());
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_closes_the_conversion_queue() {
    let h = harness();
    h.start();
    h.scheduler.shutdown().await;
    let accepted = h.scheduler.post_processor().enqueue(
        vec![PathBuf::from("a.ts")],
        crate::recorder::postprocess::ConvertOptions {
            ffmpeg_path: PathBuf::from("ffmpeg.exe"),
            delete_original: false,
        },
        Arc::new(|_: &ConvertEvent| {}),
    );
    assert!(!accepted);
}
