//! 系统通知：什么情况发、发什么（纯逻辑，配单元测试），以及发送和授权。
//!
//! 规则与 StreamCap 对齐（见 `docs/specs/feature-map.md` 的有意差异）：
//! - 总开关 `system_notification_enabled`；
//! - 开播（`stream_start_notification_enabled`，默认开）、下播（`stream_end_notification_enabled`，
//!   默认关）：普通任务只在主窗口隐藏或最小化时发；
//! - 「仅通知」任务开播：只看总开关，不受开播开关和窗口状态影响（否则关掉开播通知后，这类
//!   任务既不录也不提醒）；下播仍看下播开关，但不论窗口状态；
//! - 任务进入「需处理」时发一次，不论窗口状态；恢复正常后再次进入才会再发。
//!
//! 文案参照 StreamCap 的 `push_content`（「直播间状态更新：[room_name] 正在直播中」），
//! 去掉时间（系统通知自带），加上平台名。短时间内的多条开播（或下播）合并成一条。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value};
use tauri_plugin_notification::{NotificationExt, PermissionState};

use crate::tasks::{ActivityEntry, ActivityKind, DisplayStatus, TaskId, TaskSnapshot};

/// 合并窗口：第一条开播或下播之后等这么久，再把攒下的一起发出。
pub const MERGE_WINDOW_MS: u64 = 3_000;

/// 合并通知里最多列出的主播名。
const MAX_LISTED: usize = 5;

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
            stream_start: flag("stream_start_notification_enabled", true),
            stream_end: flag("stream_end_notification_enabled", false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveChange {
    Start,
    End,
}

/// 一条待发的开播或下播。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveItem {
    pub change: LiveChange,
    /// 「主播（平台）」。
    pub who: String,
    pub notify_only: bool,
}

/// 任务的通知相关信息，来自最近一次 `TaskEvent::Updated`。
#[derive(Debug, Clone, PartialEq)]
struct TaskInfo {
    anchor_name: String,
    platform_name: String,
    notify_only: bool,
    display: DisplayStatus,
    display_reason: Option<String>,
}

impl TaskInfo {
    fn who(&self) -> String {
        who(&self.anchor_name, Some(&self.platform_name))
    }
}

fn who(anchor_name: &str, platform_name: Option<&str>) -> String {
    let anchor = match anchor_name.trim() {
        "" => "直播间",
        name => name,
    };
    match platform_name {
        Some(platform) => format!("{anchor}（{platform}）"),
        None => anchor.to_string(),
    }
}

/// 跟踪任务状态，把任务事件变成待发的通知。不读设置、不看窗口：那些在发送时判断。
#[derive(Debug, Default)]
pub struct Tracker {
    tasks: HashMap<TaskId, TaskInfo>,
    /// 已经为这一轮「需处理」发过通知的任务。回到正常状态时移除。
    needs_action_notified: HashSet<TaskId>,
}

impl Tracker {
    /// 任务快照更新。任务进入「需处理」时返回一条通知（每轮只一次）。
    ///
    /// 检测中、重试中不算恢复正常：连续失败期间先重试、后需处理，或者需处理后的每次重新
    /// 检测，都不会重复或漏掉通知。
    pub fn on_updated(&mut self, task: &TaskSnapshot) -> Option<Notice> {
        let info = TaskInfo {
            anchor_name: task.config.anchor_name.clone(),
            platform_name: task.platform_name.to_string(),
            notify_only: task.config.notify_only,
            display: task.display,
            display_reason: task.display_reason.clone(),
        };
        let notice = match info.display {
            DisplayStatus::NeedsAction => self.needs_action_notified.insert(task.id).then(|| {
                let reason = info
                    .display_reason
                    .as_deref()
                    .filter(|reason| !reason.trim().is_empty())
                    .unwrap_or("需要处理");
                Notice {
                    title: "录播宝 · 需处理".into(),
                    body: format!("{}：{reason}", info.who()),
                }
            }),
            DisplayStatus::Checking | DisplayStatus::Retrying => None,
            DisplayStatus::Recording
            | DisplayStatus::Waiting
            | DisplayStatus::Notify
            | DisplayStatus::Paused => {
                self.needs_action_notified.remove(&task.id);
                None
            }
        };
        self.tasks.insert(task.id, info);
        notice
    }

    pub fn on_removed(&mut self, ids: &[TaskId]) {
        for id in ids {
            self.tasks.remove(id);
            self.needs_action_notified.remove(id);
        }
    }

    /// 动态。开播、下播返回待合并的一条，其余忽略（失败由 [`Tracker::on_updated`] 处理）。
    pub fn on_activity(&self, entry: &ActivityEntry) -> Option<LiveItem> {
        let change = match entry.kind {
            ActivityKind::LiveStart => LiveChange::Start,
            ActivityKind::LiveEnd => LiveChange::End,
            _ => return None,
        };
        let task = entry.task_id.and_then(|id| self.tasks.get(&id));
        Some(LiveItem {
            change,
            who: match task {
                Some(task) => task.who(),
                None => who(&entry.anchor_name, None),
            },
            notify_only: task.is_some_and(|task| task.notify_only),
        })
    }
}

/// 需处理的通知是否发：只看总开关。
pub fn allow_needs_action(settings: &NotifySettings) -> bool {
    settings.system
}

/// 把合并窗口内攒下的开播或下播变成至多一条通知。`background`：主窗口隐藏或最小化。
pub fn live_notice(
    change: LiveChange,
    items: &[LiveItem],
    settings: &NotifySettings,
    background: bool,
) -> Option<Notice> {
    if !settings.system {
        return None;
    }
    let switch = match change {
        LiveChange::Start => settings.stream_start,
        LiveChange::End => settings.stream_end,
    };
    let wanted = |item: &LiveItem| match (change, item.notify_only) {
        (LiveChange::Start, true) => true,
        (LiveChange::End, true) => switch,
        (_, false) => switch && background,
    };
    let mut names: Vec<&str> = Vec::new();
    for item in items {
        if item.change == change && wanted(item) && !names.contains(&item.who.as_str()) {
            names.push(&item.who);
        }
    }
    let (title, verb) = match change {
        LiveChange::Start => ("开播提醒", "正在直播中"),
        LiveChange::End => ("下播提醒", "直播已结束"),
    };
    let body = match names.as_slice() {
        [] => return None,
        [one] => format!("直播间状态更新：{one}{verb}"),
        many => {
            let listed = many[..many.len().min(MAX_LISTED)].join("、");
            let more = if many.len() > MAX_LISTED { " 等" } else { "" };
            let verb = match change {
                LiveChange::Start => "正在直播",
                LiveChange::End => "已下播",
            };
            format!("{} 个直播间{verb}：{listed}{more}", many.len())
        }
    };
    Some(Notice {
        title: title.into(),
        body,
    })
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
                log::warn!("录播宝无法取得通知权限：{error}");
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
        log::warn!("录播宝发送系统通知失败：{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{LiveState, Monitoring, RecordState, TaskConfig};
    use serde_json::json;

    fn snapshot(id: TaskId, display: DisplayStatus, reason: Option<&str>) -> TaskSnapshot {
        let config: TaskConfig = serde_json::from_value(json!({
            "url": "https://live.douyin.com/1",
            "platform": "douyin",
            "roomId": "1",
            "anchorId": null,
            "anchorName": "偏暖的少年",
            "liveUrl": "https://live.douyin.com/1",
            "quality": "OD",
            "notifyOnly": false,
            "note": "",
            "createdAt": 0,
        }))
        .expect("task config");
        TaskSnapshot {
            id,
            config,
            platform_name: "抖音",
            monitoring: Monitoring::Enabled,
            live: LiveState::Unknown,
            record: RecordState::Idle,
            record_held: false,
            display,
            display_reason: reason.map(str::to_string),
        }
    }

    fn activity(task_id: Option<TaskId>, kind: ActivityKind) -> ActivityEntry {
        ActivityEntry {
            id: 1,
            at: 0,
            task_id,
            anchor_name: "偏暖的少年".into(),
            kind,
            message: "检测到开播".into(),
            detail: None,
        }
    }

    fn item(change: LiveChange, who: &str, notify_only: bool) -> LiveItem {
        LiveItem {
            change,
            who: who.into(),
            notify_only,
        }
    }

    const DEFAULTS: NotifySettings = NotifySettings {
        system: true,
        stream_start: true,
        stream_end: false,
    };

    #[test]
    fn defaults_follow_streamcap_semantics() {
        assert_eq!(NotifySettings::from_settings(&Map::new()), DEFAULTS);
        let map = json!({
            "system_notification_enabled": false,
            "stream_start_notification_enabled": false,
            "stream_end_notification_enabled": true,
        });
        assert_eq!(
            NotifySettings::from_settings(map.as_object().unwrap()),
            NotifySettings {
                system: false,
                stream_start: false,
                stream_end: true,
            }
        );
    }

    #[test]
    fn defaults_match_the_task_settings_defaults() {
        let defaults = crate::tasks::settings::defaults(std::path::Path::new("videos"));
        assert_eq!(NotifySettings::from_settings(&defaults), DEFAULTS);
    }

    #[test]
    fn live_start_names_anchor_and_platform() {
        let mut tracker = Tracker::default();
        tracker.on_updated(&snapshot(1, DisplayStatus::Checking, None));
        let live = tracker
            .on_activity(&activity(Some(1), ActivityKind::LiveStart))
            .unwrap();
        assert_eq!(live, item(LiveChange::Start, "偏暖的少年（抖音）", false));
        let notice = live_notice(LiveChange::Start, &[live], &DEFAULTS, true).unwrap();
        assert_eq!(notice.title, "开播提醒");
        assert_eq!(notice.body, "直播间状态更新：偏暖的少年（抖音）正在直播中");
    }

    #[test]
    fn regular_tasks_notify_only_while_the_window_is_in_the_background() {
        let items = [item(LiveChange::Start, "A（抖音）", false)];
        assert!(live_notice(LiveChange::Start, &items, &DEFAULTS, false).is_none());
        assert!(live_notice(LiveChange::Start, &items, &DEFAULTS, true).is_some());
    }

    #[test]
    fn notify_only_tasks_always_notify_on_live_start() {
        let items = [item(LiveChange::Start, "A（抖音）", true)];
        assert!(live_notice(LiveChange::Start, &items, &DEFAULTS, false).is_some());
    }

    #[test]
    fn notify_only_live_start_ignores_the_start_switch_but_not_the_master_switch() {
        let items = [
            item(LiveChange::Start, "仅通知", true),
            item(LiveChange::Start, "普通", false),
        ];
        let no_start = NotifySettings {
            stream_start: false,
            ..DEFAULTS
        };
        let notice = live_notice(LiveChange::Start, &items, &no_start, true).unwrap();
        assert_eq!(notice.body, "直播间状态更新：仅通知正在直播中");
        let silent = NotifySettings {
            system: false,
            ..DEFAULTS
        };
        assert!(live_notice(LiveChange::Start, &items, &silent, true).is_none());
        // 下播仍看下播开关（默认关），开启后不论窗口状态。
        let end = [item(LiveChange::End, "仅通知", true)];
        assert!(live_notice(LiveChange::End, &end, &DEFAULTS, false).is_none());
        let with_end = NotifySettings {
            stream_end: true,
            ..DEFAULTS
        };
        assert!(live_notice(LiveChange::End, &end, &with_end, false).is_some());
    }

    #[test]
    fn live_switches_and_master_switch_apply() {
        let start = [item(LiveChange::Start, "A", false)];
        let end = [item(LiveChange::End, "A", false)];
        // 下播默认关。
        assert!(live_notice(LiveChange::End, &end, &DEFAULTS, true).is_none());
        let both = NotifySettings {
            stream_end: true,
            ..DEFAULTS
        };
        let notice = live_notice(LiveChange::End, &end, &both, true).unwrap();
        assert_eq!(notice.title, "下播提醒");
        assert_eq!(notice.body, "直播间状态更新：A直播已结束");
        let no_start = NotifySettings {
            stream_start: false,
            ..both
        };
        assert!(live_notice(LiveChange::Start, &start, &no_start, true).is_none());
        let silent = NotifySettings {
            system: false,
            ..both
        };
        assert!(live_notice(LiveChange::Start, &start, &silent, true).is_none());
        assert!(live_notice(LiveChange::End, &end, &silent, true).is_none());
        assert!(!allow_needs_action(&silent));
        assert!(allow_needs_action(&DEFAULTS));
    }

    #[test]
    fn several_live_starts_merge_into_one_notice() {
        let items = [
            item(LiveChange::Start, "A（抖音）", false),
            item(LiveChange::Start, "B（快手直播）", false),
            item(LiveChange::Start, "A（抖音）", false),
            item(LiveChange::End, "C（抖音）", false),
            item(LiveChange::Start, "D（抖音）", false),
        ];
        let notice = live_notice(LiveChange::Start, &items, &DEFAULTS, true).unwrap();
        assert_eq!(
            notice.body,
            "3 个直播间正在直播：A（抖音）、B（快手直播）、D（抖音）"
        );
        // 窗口在前台时只列出「仅通知」的任务。
        let mixed = [
            item(LiveChange::Start, "A", false),
            item(LiveChange::Start, "B", true),
        ];
        let notice = live_notice(LiveChange::Start, &mixed, &DEFAULTS, false).unwrap();
        assert_eq!(notice.body, "直播间状态更新：B正在直播中");
    }

    #[test]
    fn long_merged_lists_are_truncated() {
        let items: Vec<_> = (1..=7)
            .map(|n| item(LiveChange::Start, &format!("主播{n}"), false))
            .collect();
        let notice = live_notice(LiveChange::Start, &items, &DEFAULTS, true).unwrap();
        assert_eq!(
            notice.body,
            "7 个直播间正在直播：主播1、主播2、主播3、主播4、主播5 等"
        );
    }

    #[test]
    fn needs_action_notifies_once_per_episode() {
        let mut tracker = Tracker::default();
        assert!(
            tracker
                .on_updated(&snapshot(1, DisplayStatus::Waiting, None))
                .is_none()
        );
        let notice = tracker
            .on_updated(&snapshot(1, DisplayStatus::NeedsAction, Some("需要登录")))
            .unwrap();
        assert_eq!(notice.title, "录播宝 · 需处理");
        assert_eq!(notice.body, "偏暖的少年（抖音）：需要登录");
        // 重新检测（检测中）后仍需处理：不重复。
        assert!(
            tracker
                .on_updated(&snapshot(1, DisplayStatus::Checking, None))
                .is_none()
        );
        assert!(
            tracker
                .on_updated(&snapshot(1, DisplayStatus::NeedsAction, Some("需要登录")))
                .is_none()
        );
        // 恢复正常后再次需处理：再发一次。
        tracker.on_updated(&snapshot(1, DisplayStatus::Waiting, None));
        assert!(
            tracker
                .on_updated(&snapshot(1, DisplayStatus::NeedsAction, None))
                .is_some()
        );
    }

    #[test]
    fn retrying_then_needs_action_still_notifies() {
        // 连续失败先可重试、后需处理：动态已去重不再写，通知仍要发。
        let mut tracker = Tracker::default();
        assert!(
            tracker
                .on_updated(&snapshot(1, DisplayStatus::Retrying, Some("网络错误")))
                .is_none()
        );
        assert!(
            tracker
                .on_updated(&snapshot(1, DisplayStatus::Checking, None))
                .is_none()
        );
        let notice = tracker
            .on_updated(&snapshot(1, DisplayStatus::NeedsAction, Some("风控")))
            .unwrap();
        assert_eq!(notice.body, "偏暖的少年（抖音）：风控");
    }

    #[test]
    fn removed_tasks_are_forgotten() {
        let mut tracker = Tracker::default();
        tracker.on_updated(&snapshot(1, DisplayStatus::NeedsAction, None));
        tracker.on_removed(&[1]);
        assert!(
            tracker
                .on_updated(&snapshot(1, DisplayStatus::NeedsAction, None))
                .is_some()
        );
    }

    #[test]
    fn other_activity_is_ignored_and_unknown_tasks_fall_back() {
        let tracker = Tracker::default();
        for kind in [
            ActivityKind::RecordStart,
            ActivityKind::RecordEnd,
            ActivityKind::Failure,
            ActivityKind::Warning,
            ActivityKind::UserAction,
        ] {
            assert!(tracker.on_activity(&activity(Some(1), kind)).is_none());
        }
        let mut entry = activity(None, ActivityKind::LiveStart);
        entry.anchor_name = " ".into();
        assert_eq!(
            tracker.on_activity(&entry),
            Some(item(LiveChange::Start, "直播间", false))
        );
    }
}
