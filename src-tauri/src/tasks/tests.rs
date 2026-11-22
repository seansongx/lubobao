use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use super::*;
use crate::platforms::{LiveInfo, Platform, Quality};

type Events = Arc<Mutex<Vec<TaskEvent>>>;

fn open(dir: &Path) -> (TaskManager, Events) {
    let events: Events = Arc::default();
    let sink = Arc::clone(&events);
    let store = Store::open(&dir.join("lubobao.db")).unwrap();
    let manager = TaskManager::open(
        store,
        &dir.join("videos"),
        Box::new(move |event| sink.lock().unwrap().push(event)),
    )
    .unwrap();
    (manager, events)
}

fn info(room_id: &str, anchor_id: Option<&str>, name: &str) -> LiveInfo {
    LiveInfo {
        platform: Platform::Douyin,
        room_id: room_id.into(),
        anchor_id: anchor_id.map(Into::into),
        live_url: Some(format!("https://live.douyin.com/{room_id}")),
        anchor_name: name.into(),
        title: None,
        is_live: false,
        variants: Vec::new(),
        selected: None,
        stream_headers: BTreeMap::new(),
    }
}

fn add_input(url: &str) -> AddTaskInput {
    AddTaskInput {
        url: url.into(),
        ..AddTaskInput::default()
    }
}

fn added(result: TaskMutation) -> TaskSnapshot {
    match result {
        TaskMutation::Added { task } => task,
        other => panic!("expected added, got {other:?}"),
    }
}

#[test]
fn migrates_a_new_database_to_the_latest_version() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.schema_version().unwrap(), 1);
}

#[test]
fn restores_tasks_and_monitoring_intent_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (first_id, second_id) = {
        let (manager, _) = open(dir.path());
        let first = added(
            manager
                .add_resolved(
                    AddTaskInput {
                        url: " https://live.douyin.com/745964462470 ".into(),
                        quality: Some(Quality::Hd),
                        notify_only: Some(true),
                        note: Some("备注".into()),
                        force: false,
                    },
                    &info("745964462470", Some("sec-a"), "主播甲"),
                )
                .unwrap(),
        );
        let second = added(
            manager
                .add_resolved(
                    add_input("https://live.douyin.com/2"),
                    &info("2", Some("sec-b"), "主播乙"),
                )
                .unwrap(),
        );
        manager.set_monitoring(&[second.id], false).unwrap();
        (first.id, second.id)
    };

    let (manager, _) = open(dir.path());
    let tasks = manager.list();
    assert_eq!(tasks.len(), 2);
    let first = &tasks[0];
    assert_eq!(first.id, first_id);
    assert_eq!(first.config.url, "https://live.douyin.com/745964462470");
    assert_eq!(first.config.anchor_name, "主播甲");
    assert_eq!(first.config.anchor_id.as_deref(), Some("sec-a"));
    assert_eq!(first.config.quality, Quality::Hd);
    assert!(first.config.notify_only);
    assert_eq!(first.config.note, "备注");
    assert_eq!(first.monitoring, Monitoring::Enabled);
    assert_eq!(first.live, LiveState::Unknown);
    assert_eq!(first.display, DisplayStatus::Waiting);
    let second = &tasks[1];
    assert_eq!(second.id, second_id);
    assert_eq!(second.monitoring, Monitoring::Paused);
    assert_eq!(second.display, DisplayStatus::Paused);
}

#[test]
fn marks_recording_and_finishing_as_interrupted_on_startup() {
    let dir = tempfile::tempdir().unwrap();
    let (recording_id, finishing_id, idle_id) = {
        let (manager, _) = open(dir.path());
        let ids: Vec<TaskId> = ["1", "2", "3"]
            .iter()
            .map(|room| {
                added(
                    manager
                        .add_resolved(
                            add_input(&format!("https://live.douyin.com/{room}")),
                            &info(room, Some(&format!("sec-{room}")), room),
                        )
                        .unwrap(),
                )
                .id
            })
            .collect();
        manager
            .set_record_state(
                ids[0],
                RecordState::Recording {
                    file_path: PathBuf::from("D:/录像/a.ts"),
                    started_at: 1,
                    bytes_written: 10,
                    bitrate_kbps: 1.0,
                },
            )
            .unwrap();
        manager
            .set_record_state(
                ids[1],
                RecordState::Finishing {
                    file_path: PathBuf::from("b.ts"),
                    started_at: 1,
                },
            )
            .unwrap();
        (ids[0], ids[1], ids[2])
    };

    let (manager, events) = open(dir.path());
    for id in [recording_id, finishing_id] {
        let task = manager.get(id).unwrap();
        match &task.record {
            RecordState::Failed {
                reason, file_path, ..
            } => {
                assert_eq!(reason, INTERRUPTED_REASON);
                assert!(file_path.is_some());
            }
            other => panic!("expected failed, got {other:?}"),
        }
        assert_eq!(task.display, DisplayStatus::NeedsAction);
    }
    assert_eq!(manager.get(idle_id).unwrap().record, RecordState::Idle);

    let page = manager
        .list_activity(&ActivityQuery {
            kinds: vec![ActivityKind::Failure],
            ..ActivityQuery::default()
        })
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(
        page.items
            .iter()
            .all(|entry| entry.message == INTERRUPTED_REASON)
    );
    assert_eq!(
        page.items[1].detail.as_ref().unwrap()["filePath"],
        json!("D:/录像/a.ts")
    );
    let pushed = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, TaskEvent::Activity(_)))
        .count();
    assert_eq!(pushed, 2);

    // 标记已持久化：再次打开不会重复写动态。
    drop(manager);
    let (manager, _) = open(dir.path());
    let page = manager
        .list_activity(&ActivityQuery {
            kinds: vec![ActivityKind::Failure],
            ..ActivityQuery::default()
        })
        .unwrap();
    assert_eq!(page.items.len(), 2);
}

#[test]
fn returns_duplicate_for_same_anchor_and_adds_when_forced() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = open(dir.path());
    let existing = added(
        manager
            .add_resolved(
                add_input("https://live.douyin.com/745964462470"),
                &info("745964462470", Some("MS4w-sec"), "主播"),
            )
            .unwrap(),
    );

    // 主页短链：room_id 不同，anchor_id 相同。
    let profile = info("94014833191", Some("MS4w-sec"), "主播");
    match manager
        .add_resolved(add_input("https://v.douyin.com/CeiU5cbX"), &profile)
        .unwrap()
    {
        TaskMutation::Duplicate {
            existing: found,
            candidate,
        } => {
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].id, existing.id);
            assert_eq!(candidate.anchor_name, "主播");
            assert_eq!(candidate.url, "https://v.douyin.com/CeiU5cbX");
        }
        other => panic!("expected duplicate, got {other:?}"),
    }
    assert_eq!(manager.list().len(), 1);

    // 归一化后链接相同。
    let same_url = manager
        .add_resolved(
            add_input("live.douyin.com/745964462470?from=share"),
            &info("745964462470", None, "主播"),
        )
        .unwrap();
    assert!(matches!(same_url, TaskMutation::Duplicate { .. }));

    let forced = manager
        .add_resolved(
            AddTaskInput {
                force: true,
                ..add_input("https://v.douyin.com/CeiU5cbX")
            },
            &profile,
        )
        .unwrap();
    added(forced);
    assert_eq!(manager.list().len(), 2);
}

#[test]
fn new_tasks_use_settings_defaults_for_quality_and_notify_only() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = open(dir.path());
    let mut values = Map::new();
    values.insert("record_quality".into(), json!("SD"));
    values.insert("only_notify_no_record".into(), json!(true));
    manager.update_settings(values).unwrap();
    let task = added(
        manager
            .add_resolved(
                add_input("https://live.douyin.com/1"),
                &info("1", None, "a"),
            )
            .unwrap(),
    );
    assert_eq!(task.config.quality, Quality::Sd);
    assert!(task.config.notify_only);
}

#[test]
fn settings_have_streamcap_defaults_and_persist_updates() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (manager, _) = open(dir.path());
        let settings = manager.settings();
        assert_eq!(settings["loop_time_seconds"], json!(180));
        assert_eq!(settings["platform_max_concurrent_requests"], json!(3));
        assert_eq!(settings["video_format"], json!("TS"));
        assert_eq!(settings["segmented_recording_enabled"], json!(true));
        assert_eq!(settings["video_segment_time"], json!(1800));
        assert_eq!(settings["recording_space_threshold"], json!(2.0));
        assert_eq!(settings["record_quality"], json!("OD"));
        assert_eq!(
            settings["live_save_path"],
            json!(dir.path().join("videos").to_string_lossy())
        );

        let mut values = Map::new();
        values.insert("loop_time_seconds".into(), json!(60));
        values.insert("video_format".into(), json!("FLV"));
        let updated = manager.update_settings(values).unwrap();
        assert_eq!(updated["loop_time_seconds"], json!(60));
        assert_eq!(updated["video_segment_time"], json!(1800));
    }

    let (manager, _) = open(dir.path());
    let settings = manager.settings();
    assert_eq!(settings["loop_time_seconds"], json!(60));
    assert_eq!(settings["video_format"], json!("FLV"));
    assert_eq!(settings["platform_max_concurrent_requests"], json!(3));
}

#[test]
fn rejects_invalid_settings_without_saving_any() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = open(dir.path());
    for (key, value) in [
        ("loop_time_seconds", json!("180")),
        ("loop_time_seconds", json!(0)),
        ("loop_time_seconds", json!(1.5)),
        ("video_format", json!(1)),
        ("no_such_key", json!(true)),
    ] {
        let mut values = Map::new();
        values.insert("video_format".into(), json!("FLV"));
        values.insert(key.into(), value.clone());
        let error = manager.update_settings(values).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidInput, "{key} = {value}");
    }
    assert_eq!(manager.settings()["video_format"], json!("TS"));
    // 浮点阈值接受小数。
    let mut values = Map::new();
    values.insert("recording_space_threshold".into(), json!(5.5));
    assert_eq!(
        manager.update_settings(values).unwrap()["recording_space_threshold"],
        json!(5.5)
    );
}

#[test]
fn set_monitoring_only_changes_intent_and_logs_user_actions() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, events) = open(dir.path());
    let task = added(
        manager
            .add_resolved(
                add_input("https://live.douyin.com/1"),
                &info("1", None, "a"),
            )
            .unwrap(),
    );
    events.lock().unwrap().clear();

    let snapshots = manager.set_monitoring(&[task.id, 999], false).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].monitoring, Monitoring::Paused);
    assert_eq!(snapshots[0].config, task.config);
    // 再次暂停不产生事件。
    manager.set_monitoring(&[task.id], false).unwrap();
    let recorded = events.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2);
    assert!(matches!(&recorded[0], TaskEvent::Updated(s) if s.display == DisplayStatus::Paused));
    assert!(matches!(&recorded[1], TaskEvent::Activity(e) if e.message == "暂停监控"));
}

#[test]
fn delete_tasks_removes_in_batch_and_refuses_while_recording() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, events) = open(dir.path());
    let ids: Vec<TaskId> = ["1", "2", "3"]
        .iter()
        .map(|room| {
            added(
                manager
                    .add_resolved(
                        add_input(&format!("https://live.douyin.com/{room}")),
                        &info(room, None, room),
                    )
                    .unwrap(),
            )
            .id
        })
        .collect();
    manager
        .set_record_state(
            ids[2],
            RecordState::Recording {
                file_path: PathBuf::from("c.ts"),
                started_at: 1,
                bytes_written: 0,
                bitrate_kbps: 0.0,
            },
        )
        .unwrap();
    let error = manager.delete_tasks(&ids).unwrap_err();
    assert_eq!(error.kind, ErrorKind::RecordingActive);
    assert_eq!(manager.list().len(), 3);

    events.lock().unwrap().clear();
    assert_eq!(
        manager.delete_tasks(&[ids[0], ids[1], 999]).unwrap(),
        vec![ids[0], ids[1]]
    );
    assert_eq!(manager.list().len(), 1);
    assert!(matches!(
        &events.lock().unwrap()[0],
        TaskEvent::Removed(removed) if removed == &vec![ids[0], ids[1]]
    ));
    drop(manager);
    let (manager, _) = open(dir.path());
    assert_eq!(manager.list().len(), 1);
}

#[test]
fn update_task_edits_fields_and_detects_duplicate_links() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = open(dir.path());
    let a = added(
        manager
            .add_resolved(
                add_input("https://live.douyin.com/1"),
                &info("1", Some("a"), "a"),
            )
            .unwrap(),
    );
    let b = added(
        manager
            .add_resolved(
                add_input("https://live.douyin.com/2"),
                &info("2", Some("b"), "b"),
            )
            .unwrap(),
    );
    let result = manager
        .update_resolved(
            UpdateTaskInput {
                id: b.id,
                quality: Some(Quality::Ld),
                note: Some("n".into()),
                ..UpdateTaskInput::default()
            },
            None,
        )
        .unwrap();
    match result {
        TaskMutation::Updated { task } => {
            assert_eq!(task.config.quality, Quality::Ld);
            assert_eq!(task.config.note, "n");
        }
        other => panic!("{other:?}"),
    }
    let duplicate = manager
        .update_resolved(
            UpdateTaskInput {
                id: b.id,
                url: Some("https://live.douyin.com/1".into()),
                ..UpdateTaskInput::default()
            },
            Some(&info("1", Some("a"), "a")),
        )
        .unwrap();
    assert!(
        matches!(duplicate, TaskMutation::Duplicate { ref existing, .. } if existing[0].id == a.id)
    );
    assert_eq!(
        manager.get(b.id).unwrap().config.url,
        "https://live.douyin.com/2"
    );
    let missing = manager
        .update_resolved(
            UpdateTaskInput {
                id: 999,
                ..UpdateTaskInput::default()
            },
            None,
        )
        .unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound);
}

#[test]
fn live_and_record_transitions_write_activity() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = open(dir.path());
    let task = added(
        manager
            .add_resolved(
                add_input("https://live.douyin.com/1"),
                &info("1", None, "a"),
            )
            .unwrap(),
    );
    let live = LiveState::Live {
        checked_at: 1,
        title: Some("标题".into()),
    };
    manager.set_live_state(task.id, LiveState::Offline { checked_at: 1 });
    manager.set_live_state(task.id, live.clone());
    manager.set_live_state(task.id, live.clone());
    let failure = LiveState::CheckFailed {
        kind: PlatformErrorKind::Network,
        message: "超时".into(),
        failed_at: 1,
        next_retry_at: Some(2),
        attempt: 1,
    };
    let snapshot = manager.set_live_state(task.id, failure.clone()).unwrap();
    assert_eq!(snapshot.display, DisplayStatus::Retrying);
    manager.set_live_state(task.id, failure);
    // 检测失败后恢复为开播，不算新的开播。
    manager.set_live_state(task.id, live);
    manager
        .set_record_state(
            task.id,
            RecordState::Recording {
                file_path: PathBuf::from("a.ts"),
                started_at: now_ms(),
                bytes_written: 0,
                bitrate_kbps: 0.0,
            },
        )
        .unwrap();
    let progressed = manager
        .update_record_progress(task.id, 2048, 6200.0)
        .unwrap();
    assert!(matches!(
        progressed.record,
        RecordState::Recording {
            bytes_written: 2048,
            ..
        }
    ));
    manager
        .set_record_state(task.id, RecordState::Idle)
        .unwrap();
    manager.set_live_state(task.id, LiveState::Offline { checked_at: 2 });
    assert!(manager.update_record_progress(task.id, 1, 1.0).is_none());

    let kinds: Vec<ActivityKind> = manager
        .list_activity(&ActivityQuery {
            task_id: Some(task.id),
            ..ActivityQuery::default()
        })
        .unwrap()
        .items
        .into_iter()
        .rev()
        .map(|entry| entry.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            ActivityKind::UserAction,
            ActivityKind::LiveStart,
            ActivityKind::Failure,
            ActivityKind::RecordStart,
            ActivityKind::RecordEnd,
            ActivityKind::LiveEnd,
        ]
    );
}

#[test]
fn lists_activity_with_paging_and_filters() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = open(dir.path());
    for index in 0..5 {
        manager
            .record_activity(NewActivity {
                task_id: Some(index % 2),
                anchor_name: format!("主播{index}"),
                kind: if index == 4 {
                    ActivityKind::Failure
                } else {
                    ActivityKind::LiveStart
                },
                message: format!("m{index}"),
                detail: Some(json!({ "n": index })),
            })
            .unwrap();
    }
    let first = manager
        .list_activity(&ActivityQuery {
            limit: Some(2),
            ..ActivityQuery::default()
        })
        .unwrap();
    assert_eq!(
        first
            .items
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>(),
        vec!["m4", "m3"]
    );
    assert_eq!(first.items[0].detail, Some(json!({ "n": 4 })));
    let second = manager
        .list_activity(&ActivityQuery {
            limit: Some(2),
            before: first.next_before,
            ..ActivityQuery::default()
        })
        .unwrap();
    assert_eq!(
        second
            .items
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>(),
        vec!["m2", "m1"]
    );
    let last = manager
        .list_activity(&ActivityQuery {
            limit: Some(2),
            before: second.next_before,
            ..ActivityQuery::default()
        })
        .unwrap();
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.next_before, None);

    let filtered = manager
        .list_activity(&ActivityQuery {
            task_id: Some(0),
            kinds: vec![ActivityKind::LiveStart],
            ..ActivityQuery::default()
        })
        .unwrap();
    assert_eq!(
        filtered
            .items
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>(),
        vec!["m2", "m0"]
    );
}

#[test]
fn stop_recording_reports_not_implemented() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = open(dir.path());
    let error = manager.stop_recording(&[1]).unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotImplemented);
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({ "kind": "not_implemented", "message": "录制功能尚未接入" })
    );
}

#[test]
fn serializes_mutation_results_with_result_tag() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = open(dir.path());
    let result = manager
        .add_resolved(
            add_input("https://live.douyin.com/1"),
            &info("1", Some("s"), "a"),
        )
        .unwrap();
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["result"], "added");
    assert_eq!(value["task"]["anchorName"], "a");
    let duplicate = manager
        .add_resolved(
            add_input("https://live.douyin.com/1"),
            &info("1", Some("s"), "a"),
        )
        .unwrap();
    let value: Value = serde_json::to_value(&duplicate).unwrap();
    assert_eq!(value["result"], "duplicate");
    assert_eq!(value["existing"][0]["id"], 1);
    assert_eq!(value["candidate"]["platformName"], "抖音");
    assert_eq!(value["candidate"]["anchorId"], "s");
}

/// 联网：解析真实抖音链接并检查短链判重。`cargo test -- --ignored live_douyin` 运行。
#[tokio::test]
#[ignore]
async fn live_douyin_add_and_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = open(dir.path());
    let first = manager
        .add_task(add_input("https://live.douyin.com/745964462470"))
        .await
        .unwrap();
    let task = added(first);
    println!(
        "added: {} {:?}",
        task.config.anchor_name, task.config.anchor_id
    );
    assert!(!task.config.anchor_name.is_empty());
    let second = manager
        .add_task(add_input("https://v.douyin.com/CeiU5cbX"))
        .await
        .unwrap();
    println!("second: {second:?}");
    assert!(matches!(second, TaskMutation::Duplicate { .. }));
}
