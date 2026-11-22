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

// ---- 假平台 ----

#[derive(Clone)]
enum Reply {
    Live,
    Offline,
    Fail(PlatformError),
}

#[derive(Default)]
struct FakeSource {
    replies: Mutex<HashMap<String, Reply>>,
    calls: Mutex<Vec<(String, Instant)>>,
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

fn live_info(url: &str, is_live: bool) -> LiveInfo {
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

impl LiveSource for Arc<FakeSource> {
    fn fetch(
        &self,
        url: String,
        _options: FetchOptions,
    ) -> BoxFuture<'static, Result<LiveInfo, PlatformError>> {
        let source = Arc::clone(self);
        Box::pin(async move {
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
                Reply::Offline => Ok(live_info(&url, false)),
                Reply::Fail(error) => Err(error),
            }
        })
    }
}

// ---- 假录制引擎 ----

struct FakeRecording {
    request: RecordingRequest,
    listener: EventListener,
    stopped: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

struct FakeControl {
    listener: EventListener,
    stopped: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

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

fn emit_finished(listener: &EventListener, finished: &AtomicBool, reason: EndReason) {
    if !finished.swap(true, Ordering::SeqCst) {
        listener(&RecorderEvent::Finished {
            outcome: outcome(reason),
        });
    }
}

impl RecordingControl for FakeControl {
    // 与真实引擎一样：只发出请求，事件在另一个任务里送回。
    fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let listener = Arc::clone(&self.listener);
        let finished = Arc::clone(&self.finished);
        tokio::spawn(async move {
            if finished.load(Ordering::SeqCst) {
                return;
            }
            listener(&RecorderEvent::Stopping {
                cause: StopCause::User,
            });
            emit_finished(&listener, &finished, EndReason::UserStopped);
        });
    }
}

struct FakeEngine {
    ffmpeg: Mutex<Option<PathBuf>>,
    /// 为 false 时启动后不发 `started`，模拟 ffmpeg 还在启动。
    emit_started: AtomicBool,
    recordings: Mutex<Vec<FakeRecording>>,
    stop_all_calls: AtomicUsize,
}

impl Default for FakeEngine {
    fn default() -> Self {
        FakeEngine {
            ffmpeg: Mutex::new(Some(PathBuf::from("ffmpeg.exe"))),
            emit_started: AtomicBool::new(true),
            recordings: Mutex::default(),
            stop_all_calls: AtomicUsize::new(0),
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

    fn stopped(&self, index: usize) -> bool {
        self.recordings.lock().unwrap()[index]
            .stopped
            .load(Ordering::SeqCst)
    }

    fn emit(&self, index: usize, event: RecorderEvent) {
        let listener = Arc::clone(&self.recordings.lock().unwrap()[index].listener);
        listener(&event);
    }

    /// 引擎自己结束录制（断流、磁盘满等）。
    fn end(&self, index: usize, reason: EndReason) {
        let (listener, finished) = {
            let recordings = self.recordings.lock().unwrap();
            let recording = &recordings[index];
            (
                Arc::clone(&recording.listener),
                Arc::clone(&recording.finished),
            )
        };
        emit_finished(&listener, &finished, reason);
    }
}

impl RecordEngine for Arc<FakeEngine> {
    fn ffmpeg_path(&self) -> Option<PathBuf> {
        self.ffmpeg.lock().unwrap().clone()
    }

    fn start(
        &self,
        request: RecordingRequest,
        listener: EventListener,
    ) -> BoxFuture<'static, Arc<dyn RecordingControl>> {
        let engine = Arc::clone(self);
        Box::pin(async move {
            let stopped = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicBool::new(false));
            engine.recordings.lock().unwrap().push(FakeRecording {
                request,
                listener: Arc::clone(&listener),
                stopped: Arc::clone(&stopped),
                finished: Arc::clone(&finished),
            });
            if engine.emit_started.load(Ordering::SeqCst) {
                listener(&RecorderEvent::Started {
                    output_dir: PathBuf::from("out"),
                    output_path: PathBuf::from("out/rec_%03d.ts"),
                    source_format: crate::platforms::SourceFormat::Flv,
                    segmented: true,
                    pid: Some(1),
                    started_at_ms: now_ms(),
                });
            }
            Arc::new(FakeControl {
                listener,
                stopped,
                finished,
            }) as Arc<dyn RecordingControl>
        })
    }

    fn stop_all(&self) -> BoxFuture<'static, ()> {
        self.stop_all_calls.fetch_add(1, Ordering::SeqCst);
        let engine = Arc::clone(self);
        Box::pin(async move {
            let pending: Vec<(EventListener, Arc<AtomicBool>)> = engine
                .recordings
                .lock()
                .unwrap()
                .iter()
                .map(|recording| {
                    recording.stopped.store(true, Ordering::SeqCst);
                    (
                        Arc::clone(&recording.listener),
                        Arc::clone(&recording.finished),
                    )
                })
                .collect();
            for (listener, finished) in pending {
                emit_finished(&listener, &finished, EndReason::UserStopped);
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
}

const INTERVAL: Duration = Duration::from_secs(180);

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("lubobao.db")).unwrap();
    let tasks =
        Arc::new(TaskManager::open(store, &dir.path().join("videos"), Box::new(|_| {})).unwrap());
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
    Harness {
        _dir: dir,
        tasks,
        source,
        engine,
        scheduler,
        statuses,
    }
}

impl Harness {
    fn add(&self, room: &str, reply: Reply, notify_only: bool) -> (TaskId, String) {
        let url = format!("https://live.douyin.com/{room}");
        let is_live = matches!(reply, Reply::Live);
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
    assert_eq!(request.options.proxy.as_deref(), Some("127.0.0.1:7890"));
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
async fn shutdown_stops_a_recording_that_is_still_starting() {
    let h = harness();
    h.engine.emit_started.store(false, Ordering::SeqCst);
    let (id, _) = h.add("1017", Reply::Live, false);
    h.start();
    until(SHORT, "启动中", || h.engine.starts() == 1).await;
    h.scheduler.shutdown().await;
    assert!(h.engine.stopped(0));
    assert_eq!(h.scheduler.status().recording, 0);
    assert_eq!(h.task(id).record, RecordState::Idle);
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
async fn deleting_a_task_stops_its_starting_recording() {
    let h = harness();
    h.engine.emit_started.store(false, Ordering::SeqCst);
    let (id, _) = h.add("1019", Reply::Live, false);
    h.start();
    until(SHORT, "启动中", || h.engine.starts() == 1).await;
    // 录制状态还是空闲，删除不会被拒绝，但调度要停掉这个录制。
    assert_eq!(h.tasks.delete_tasks(&[id]).unwrap(), vec![id]);
    assert!(h.engine.stopped(0));
    until(SHORT, "释放", || h.scheduler.status().recording == 0).await;
}
