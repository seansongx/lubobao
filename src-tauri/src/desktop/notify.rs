//! 系统通知：哪条动态在什么设置下发通知、文案是什么（纯函数），以及发送和授权。
//!
//! 文案参照 StreamCap 的 `push_content` / `push_content_end`（「直播间状态更新：[room_name]
//! 正在直播中」），去掉时间（系统通知自带），加上平台名。

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;
use serde_json::{Map, Value};
use tauri_plugin_notification::{NotificationExt, PermissionState};

use crate::tasks::{ActivityKind, DisplayStatus, TaskId};

/// 通知相关的三个设置，键名同 StreamCap。缺省值与 `tasks::settings` 的默认值一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifySettings {
    pub system: bool,
    pub stream_start: bool,
    pub stream_end: bool,
}

impl NotifySettings {
    pub fn from_settings(settings: &Map<String, Value>) -> NotifySettings {
        let flag = |key: &str, default: bool| {
            settings
                .get(key)
                .and_then(Value::as_bool)
                .unwrap_or(default)
        };
        NotifySettings {
            system: flag("system_notification_enabled", true),
            stream_start: flag("stream_start_notification_enabled", false),
            stream_end: flag("stream_end_notification_enabled", false),
        }
    }
}

/// `activity` 事件载荷中通知用得到的字段（见 `tasks::ActivityEntry`）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityNotice {
    pub task_id: Option<TaskId>,
    pub anchor_name: String,
    pub kind: ActivityKind,
    pub message: String,
}

/// 收到动态时该任务的当前情况。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskNotice {
    pub platform_name: String,
    pub display: DisplayStatus,
    pub display_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

/// 决定一条动态要不要发系统通知，要发时给出文案。
///
/// - 开播（live_start）：需要 `stream_start_notification_enabled`；
/// - 下播（live_end）：需要 `stream_end_notification_enabled`；
/// - 失败（failure）：只在任务当前显示为「需处理」时发，会自行重试的不打扰；
/// - 都受总开关 `system_notification_enabled` 控制；其他动态不发。
pub fn decide(
    activity: &ActivityNotice,
    task: Option<&TaskNotice>,
    settings: &NotifySettings,
) -> Option<Notice> {
    if !settings.system {
        return None;
    }
    let anchor = match activity.anchor_name.trim() {
        "" => "直播间",
        name => name,
    };
    let who = match task {
        Some(task) => format!("{anchor}（{}）", task.platform_name),
        None => anchor.to_string(),
    };
    match activity.kind {
        ActivityKind::LiveStart if settings.stream_start => Some(Notice {
            title: "开播提醒".into(),
            body: format!("直播间状态更新：{who}正在直播中"),
        }),
        ActivityKind::LiveEnd if settings.stream_end => Some(Notice {
            title: "下播提醒".into(),
            body: format!("直播间状态更新：{who}直播已结束"),
        }),
        ActivityKind::Failure => {
            let task = task.filter(|task| task.display == DisplayStatus::NeedsAction)?;
            let reason = task
                .display_reason
                .as_deref()
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or(&activity.message);
            Some(Notice {
                title: "录播宝 · 需处理".into(),
                body: format!("{who}：{reason}"),
            })
        }
        _ => None,
    }
}

/// 用户拒绝通知授权后置位，本次运行不再请求。
static PERMISSION_DENIED: AtomicBool = AtomicBool::new(false);

/// 需要时请求授权（只请求一次）。桌面端通常直接为已授权。
fn ensure_permission(app: &tauri::AppHandle) -> bool {
    if PERMISSION_DENIED.load(Ordering::SeqCst) {
        return false;
    }
    let notification = app.notification();
    let state = match notification.permission_state() {
        Ok(PermissionState::Prompt | PermissionState::PromptWithRationale) => {
            notification.request_permission()
        }
        other => other,
    };
    match state {
        Ok(PermissionState::Granted) => true,
        Ok(_) | Err(_) => {
            if let Err(error) = &state {
                eprintln!("录播宝无法取得通知权限：{error}");
            }
            PERMISSION_DENIED.store(true, Ordering::SeqCst);
            false
        }
    }
}

/// 发一条系统通知。没有授权或发送失败时只记日志。
pub fn show(app: &tauri::AppHandle, notice: &Notice) {
    if !ensure_permission(app) {
        return;
    }
    if let Err(error) = app
        .notification()
        .builder()
        .title(&notice.title)
        .body(&notice.body)
        .show()
    {
        eprintln!("录播宝发送系统通知失败：{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn activity(kind: ActivityKind) -> ActivityNotice {
        ActivityNotice {
            task_id: Some(1),
            anchor_name: "偏暖的少年".into(),
            kind,
            message: "检测失败：需要登录".into(),
        }
    }

    fn task(display: DisplayStatus) -> TaskNotice {
        TaskNotice {
            platform_name: "抖音".into(),
            display,
            display_reason: None,
        }
    }

    fn all_on() -> NotifySettings {
        NotifySettings {
            system: true,
            stream_start: true,
            stream_end: true,
        }
    }

    #[test]
    fn defaults_match_streamcap_settings() {
        assert_eq!(
            NotifySettings::from_settings(&Map::new()),
            NotifySettings {
                system: true,
                stream_start: false,
                stream_end: false,
            }
        );
        let map = json!({
            "system_notification_enabled": false,
            "stream_start_notification_enabled": true,
            "stream_end_notification_enabled": true,
        });
        assert_eq!(
            NotifySettings::from_settings(map.as_object().unwrap()),
            NotifySettings {
                system: false,
                stream_start: true,
                stream_end: true,
            }
        );
    }

    #[test]
    fn live_start_names_anchor_and_platform() {
        let notice = decide(
            &activity(ActivityKind::LiveStart),
            Some(&task(DisplayStatus::Checking)),
            &all_on(),
        )
        .unwrap();
        assert_eq!(notice.title, "开播提醒");
        assert_eq!(notice.body, "直播间状态更新：偏暖的少年（抖音）正在直播中");
    }

    #[test]
    fn live_end_names_anchor_and_platform() {
        let notice = decide(
            &activity(ActivityKind::LiveEnd),
            Some(&task(DisplayStatus::Waiting)),
            &all_on(),
        )
        .unwrap();
        assert_eq!(notice.title, "下播提醒");
        assert_eq!(notice.body, "直播间状态更新：偏暖的少年（抖音）直播已结束");
    }

    #[test]
    fn live_start_and_end_follow_their_own_switches() {
        let settings = NotifySettings {
            system: true,
            stream_start: false,
            stream_end: true,
        };
        let waiting = task(DisplayStatus::Waiting);
        assert!(
            decide(
                &activity(ActivityKind::LiveStart),
                Some(&waiting),
                &settings
            )
            .is_none()
        );
        assert!(decide(&activity(ActivityKind::LiveEnd), Some(&waiting), &settings).is_some());
        let settings = NotifySettings {
            stream_start: true,
            stream_end: false,
            ..settings
        };
        assert!(
            decide(
                &activity(ActivityKind::LiveStart),
                Some(&waiting),
                &settings
            )
            .is_some()
        );
        assert!(decide(&activity(ActivityKind::LiveEnd), Some(&waiting), &settings).is_none());
    }

    #[test]
    fn master_switch_silences_everything() {
        let settings = NotifySettings {
            system: false,
            ..all_on()
        };
        let needs_action = task(DisplayStatus::NeedsAction);
        for kind in [
            ActivityKind::LiveStart,
            ActivityKind::LiveEnd,
            ActivityKind::Failure,
        ] {
            assert!(decide(&activity(kind), Some(&needs_action), &settings).is_none());
        }
    }

    #[test]
    fn failure_notifies_only_when_the_task_needs_action() {
        let settings = NotifySettings {
            system: true,
            stream_start: false,
            stream_end: false,
        };
        let failure = activity(ActivityKind::Failure);
        let notice = decide(&failure, Some(&task(DisplayStatus::NeedsAction)), &settings).unwrap();
        assert_eq!(notice.title, "录播宝 · 需处理");
        assert_eq!(notice.body, "偏暖的少年（抖音）：检测失败：需要登录");
        for display in [
            DisplayStatus::Retrying,
            DisplayStatus::Waiting,
            DisplayStatus::Recording,
            DisplayStatus::Paused,
        ] {
            assert!(decide(&failure, Some(&task(display)), &settings).is_none());
        }
        assert!(decide(&failure, None, &settings).is_none());
    }

    #[test]
    fn failure_prefers_the_current_display_reason() {
        let mut needs_action = task(DisplayStatus::NeedsAction);
        needs_action.display_reason = Some("磁盘空间不足：剩余 0.8 GB".into());
        let notice = decide(
            &activity(ActivityKind::Failure),
            Some(&needs_action),
            &all_on(),
        )
        .unwrap();
        assert_eq!(notice.body, "偏暖的少年（抖音）：磁盘空间不足：剩余 0.8 GB");
    }

    #[test]
    fn other_activity_kinds_do_not_notify() {
        let recording = task(DisplayStatus::Recording);
        for kind in [
            ActivityKind::RecordStart,
            ActivityKind::RecordEnd,
            ActivityKind::Warning,
            ActivityKind::UserAction,
        ] {
            assert!(decide(&activity(kind), Some(&recording), &all_on()).is_none());
        }
    }

    #[test]
    fn missing_anchor_name_and_task_fall_back() {
        let mut live = activity(ActivityKind::LiveStart);
        live.anchor_name = "  ".into();
        let notice = decide(&live, None, &all_on()).unwrap();
        assert_eq!(notice.body, "直播间状态更新：直播间正在直播中");
    }

    #[test]
    fn parses_the_activity_event_payload() {
        let payload = json!({
            "id": 7,
            "at": 1_700_000_000_000_i64,
            "taskId": 3,
            "anchorName": "主播",
            "kind": "live_start",
            "message": "检测到开播",
            "detail": null,
        });
        let notice: ActivityNotice = serde_json::from_value(payload).unwrap();
        assert_eq!(notice.task_id, Some(3));
        assert_eq!(notice.kind, ActivityKind::LiveStart);
    }
}
