//! 任务模型：配置字段、三条独立的运行状态，以及推导出的界面显示状态。
//!
//! 按 `docs/feature-map.md`「状态判定」分开记录监控意图、直播状态和录制状态，
//! 界面显示的七种状态由 [`derive_display`] 从这三条推导，不单独存储。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::platforms::{Platform, PlatformError, Quality};

pub type TaskId = i64;

/// 当前时间，Unix 毫秒。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// 用户添加任务时确定、之后可编辑的字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskConfig {
    /// 用户输入的链接（去掉首尾空白）。
    pub url: String,
    pub platform: Platform,
    pub room_id: String,
    /// 平台内稳定的主播标识（抖音为 `sec_uid`），用于去重。
    pub anchor_id: Option<String>,
    pub anchor_name: String,
    /// 平台给出的规范直播间地址。
    pub live_url: Option<String>,
    pub quality: Quality,
    /// 开播时只通知、不录制（StreamCap `only_notify_no_record`）。
    pub notify_only: bool,
    pub note: String,
    /// 创建时间，Unix 毫秒。
    pub created_at: i64,
}

/// 监控意图：用户是否希望检测这个直播间。持久化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Monitoring {
    Enabled,
    Paused,
}

impl Monitoring {
    pub fn as_str(self) -> &'static str {
        match self {
            Monitoring::Enabled => "enabled",
            Monitoring::Paused => "paused",
        }
    }

    pub fn parse(value: &str) -> Monitoring {
        if value == "paused" {
            Monitoring::Paused
        } else {
            Monitoring::Enabled
        }
    }
}

/// 检测失败时的错误类别，与 [`PlatformError`] 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformErrorKind {
    Network,
    InvalidUrl,
    AuthRequired,
    RateLimited,
    Parse,
    Config,
}

impl PlatformErrorKind {
    pub fn of(error: &PlatformError) -> PlatformErrorKind {
        match error {
            PlatformError::Network(_) => PlatformErrorKind::Network,
            PlatformError::InvalidUrl(_) => PlatformErrorKind::InvalidUrl,
            PlatformError::AuthRequired(_) => PlatformErrorKind::AuthRequired,
            PlatformError::RateLimited(_) => PlatformErrorKind::RateLimited,
            PlatformError::Parse(_) => PlatformErrorKind::Parse,
            PlatformError::Config(_) => PlatformErrorKind::Config,
        }
    }

    /// 网络、限流和解析失败可能自行恢复，自动重试；链接无效、需要登录和配置错误要用户处理。
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            PlatformErrorKind::Network | PlatformErrorKind::RateLimited | PlatformErrorKind::Parse
        )
    }
}

/// 直播间状态。只在内存中保存，启动时为 `Unknown`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LiveState {
    /// 尚未检测。
    Unknown,
    /// 正在请求平台。
    Checking {
        since: i64,
    },
    Offline {
        checked_at: i64,
    },
    Live {
        checked_at: i64,
        title: Option<String>,
    },
    /// 检测失败。`attempt` 为连续失败次数，从 1 开始。
    CheckFailed {
        kind: PlatformErrorKind,
        message: String,
        failed_at: i64,
        next_retry_at: Option<i64>,
        attempt: u32,
    },
}

/// 录制状态。持久化，以便启动时识别异常中断的录制。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RecordState {
    Idle,
    Recording {
        file_path: PathBuf,
        started_at: i64,
        bytes_written: u64,
        /// 当前码率，kbit/s。
        bitrate_kbps: f64,
    },
    /// 已请求停止，正在等 FFmpeg 写完文件尾。
    Finishing {
        file_path: PathBuf,
        started_at: i64,
    },
    Failed {
        reason: String,
        file_path: Option<PathBuf>,
        failed_at: i64,
    },
}

impl RecordState {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            RecordState::Recording { .. } | RecordState::Finishing { .. }
        )
    }
}

/// 界面显示的七种状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DisplayStatus {
    /// 录制中（含收尾中）。
    Recording,
    /// 检测中，或已开播、正在开始录制。
    Checking,
    /// 等待开播。
    Waiting,
    /// 检测失败，将自动重试。
    Retrying,
    /// 已开播，仅通知不录制。
    Notify,
    /// 需要用户处理：录制失败，或检测失败且不会自行恢复。
    NeedsAction,
    /// 已暂停监控。
    Paused,
}

/// 由三条运行状态推导显示状态。优先级：
/// 1. 正在写文件（录制中、收尾中）→ 录制中。暂停监控不会让正在写的文件消失。
/// 2. 已暂停 → 已暂停。
/// 3. 录制失败 → 需处理。
/// 4. 检测失败：可重试的类别 → 重试中，其余 → 需处理。
/// 5. 检测中 → 检测中。
/// 6. 已开播：仅通知 → 仅通知；否则正在启动录制 → 检测中。
/// 7. 未知或未开播 → 等待开播。
pub fn derive_display(
    monitoring: Monitoring,
    live: &LiveState,
    record: &RecordState,
    notify_only: bool,
) -> DisplayStatus {
    if record.is_active() {
        return DisplayStatus::Recording;
    }
    if monitoring == Monitoring::Paused {
        return DisplayStatus::Paused;
    }
    if matches!(record, RecordState::Failed { .. }) {
        return DisplayStatus::NeedsAction;
    }
    match live {
        LiveState::CheckFailed { kind, .. } if kind.is_retryable() => DisplayStatus::Retrying,
        LiveState::CheckFailed { .. } => DisplayStatus::NeedsAction,
        LiveState::Checking { .. } => DisplayStatus::Checking,
        LiveState::Live { .. } if notify_only => DisplayStatus::Notify,
        LiveState::Live { .. } => DisplayStatus::Checking,
        LiveState::Unknown | LiveState::Offline { .. } => DisplayStatus::Waiting,
    }
}

/// 需处理和重试中时给界面显示的原因。
pub fn display_reason(
    status: DisplayStatus,
    live: &LiveState,
    record: &RecordState,
) -> Option<String> {
    match status {
        DisplayStatus::NeedsAction | DisplayStatus::Retrying => {
            if let RecordState::Failed { reason, .. } = record {
                return Some(reason.clone());
            }
            if let LiveState::CheckFailed { message, .. } = live {
                return Some(message.clone());
            }
            None
        }
        _ => None,
    }
}

/// 发给前端的任务完整快照。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub id: TaskId,
    #[serde(flatten)]
    pub config: TaskConfig,
    pub platform_name: &'static str,
    pub monitoring: Monitoring,
    pub live: LiveState,
    pub record: RecordState,
    /// 用户手动停止了录制：本次直播不再自动录制，下播或重新启用监控后恢复。只在内存中。
    pub record_held: bool,
    pub display: DisplayStatus,
    pub display_reason: Option<String>,
}

/// 内存中的一条任务。
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub id: TaskId,
    pub config: TaskConfig,
    pub monitoring: Monitoring,
    pub live: LiveState,
    pub record: RecordState,
    /// 见 [`TaskSnapshot::record_held`]。
    pub held: bool,
}

/// 手动停止后仍在直播时给界面显示的原因。
pub const HELD_REASON: &str = "已手动停止录制，下次开播时自动录制";

impl Task {
    /// 手动停止后仍在直播时与仅通知一样显示：已开播、没有写文件，属于正常结果。
    pub fn display(&self) -> DisplayStatus {
        derive_display(
            self.monitoring,
            &self.live,
            &self.record,
            self.config.notify_only || self.held,
        )
    }

    pub fn snapshot(&self) -> TaskSnapshot {
        let display = self.display();
        let reason = if display == DisplayStatus::Notify && self.held && !self.config.notify_only {
            Some(HELD_REASON.to_string())
        } else {
            display_reason(display, &self.live, &self.record)
        };
        TaskSnapshot {
            id: self.id,
            config: self.config.clone(),
            platform_name: self.config.platform.display_name(),
            monitoring: self.monitoring,
            live: self.live.clone(),
            record: self.record.clone(),
            record_held: self.held,
            display,
            display_reason: reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording() -> RecordState {
        RecordState::Recording {
            file_path: PathBuf::from("a.ts"),
            started_at: 1,
            bytes_written: 0,
            bitrate_kbps: 0.0,
        }
    }

    fn failed_check(kind: PlatformErrorKind) -> LiveState {
        LiveState::CheckFailed {
            kind,
            message: "x".into(),
            failed_at: 1,
            next_retry_at: Some(2),
            attempt: 1,
        }
    }

    fn live() -> LiveState {
        LiveState::Live {
            checked_at: 1,
            title: None,
        }
    }

    fn record_failed() -> RecordState {
        RecordState::Failed {
            reason: "磁盘空间不足".into(),
            file_path: None,
            failed_at: 1,
        }
    }

    use DisplayStatus as D;
    use Monitoring::{Enabled as On, Paused as Off};

    #[test]
    fn derives_recording_while_writing_even_when_paused() {
        assert_eq!(
            derive_display(On, &live(), &recording(), false),
            D::Recording
        );
        assert_eq!(
            derive_display(Off, &live(), &recording(), false),
            D::Recording
        );
        let finishing = RecordState::Finishing {
            file_path: PathBuf::from("a.ts"),
            started_at: 1,
        };
        assert_eq!(
            derive_display(On, &LiveState::Unknown, &finishing, false),
            D::Recording
        );
    }

    #[test]
    fn derives_paused_over_other_states() {
        assert_eq!(
            derive_display(Off, &LiveState::Unknown, &RecordState::Idle, false),
            D::Paused
        );
        assert_eq!(
            derive_display(Off, &live(), &RecordState::Idle, true),
            D::Paused
        );
        assert_eq!(
            derive_display(Off, &live(), &record_failed(), false),
            D::Paused
        );
        assert_eq!(
            derive_display(
                Off,
                &failed_check(PlatformErrorKind::AuthRequired),
                &RecordState::Idle,
                false
            ),
            D::Paused
        );
    }

    #[test]
    fn derives_needs_action_for_record_failures_and_unrecoverable_check_errors() {
        // 已开播＋磁盘不足：没有写文件，但要用户处理。
        assert_eq!(
            derive_display(On, &live(), &record_failed(), false),
            D::NeedsAction
        );
        for kind in [
            PlatformErrorKind::InvalidUrl,
            PlatformErrorKind::AuthRequired,
            PlatformErrorKind::Config,
        ] {
            assert_eq!(
                derive_display(On, &failed_check(kind), &RecordState::Idle, false),
                D::NeedsAction,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn derives_retrying_for_transient_check_errors() {
        for kind in [
            PlatformErrorKind::Network,
            PlatformErrorKind::RateLimited,
            PlatformErrorKind::Parse,
        ] {
            assert_eq!(
                derive_display(On, &failed_check(kind), &RecordState::Idle, false),
                D::Retrying,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn derives_notify_only_when_live_and_notify_only() {
        // 已开播＋仅通知：没有写文件，属于正常结果。
        assert_eq!(
            derive_display(On, &live(), &RecordState::Idle, true),
            D::Notify
        );
        assert_eq!(
            derive_display(On, &live(), &RecordState::Idle, false),
            D::Checking
        );
    }

    #[test]
    fn derives_checking_and_waiting() {
        assert_eq!(
            derive_display(
                On,
                &LiveState::Checking { since: 1 },
                &RecordState::Idle,
                false
            ),
            D::Checking
        );
        assert_eq!(
            derive_display(On, &LiveState::Unknown, &RecordState::Idle, false),
            D::Waiting
        );
        assert_eq!(
            derive_display(
                On,
                &LiveState::Offline { checked_at: 1 },
                &RecordState::Idle,
                true
            ),
            D::Waiting
        );
    }

    #[test]
    fn display_reason_prefers_record_failure() {
        let reason = display_reason(
            D::NeedsAction,
            &failed_check(PlatformErrorKind::AuthRequired),
            &record_failed(),
        );
        assert_eq!(reason.as_deref(), Some("磁盘空间不足"));
        assert_eq!(
            display_reason(D::Waiting, &LiveState::Unknown, &RecordState::Idle),
            None
        );
    }

    #[test]
    fn serializes_snapshot_in_camel_case_with_tagged_states() {
        let task = Task {
            id: 7,
            config: TaskConfig {
                url: "https://live.douyin.com/1".into(),
                platform: Platform::Douyin,
                room_id: "1".into(),
                anchor_id: Some("sec".into()),
                anchor_name: "主播".into(),
                live_url: None,
                quality: Quality::Od,
                notify_only: false,
                note: String::new(),
                created_at: 5,
            },
            monitoring: On,
            live: failed_check(PlatformErrorKind::Network),
            record: RecordState::Idle,
            held: false,
        };
        let value = serde_json::to_value(task.snapshot()).unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["anchorName"], "主播");
        assert_eq!(value["platform"], "douyin");
        assert_eq!(value["platformName"], "抖音");
        assert_eq!(value["quality"], "OD");
        assert_eq!(value["notifyOnly"], false);
        assert_eq!(value["createdAt"], 5);
        assert_eq!(value["monitoring"], "enabled");
        assert_eq!(value["live"]["state"], "checkFailed");
        assert_eq!(value["live"]["kind"], "network");
        assert_eq!(value["live"]["nextRetryAt"], 2);
        assert_eq!(value["record"]["state"], "idle");
        assert_eq!(value["display"], "retrying");
        assert_eq!(value["displayReason"], "x");
    }
}
