//! 动态记录：开播、下播、开始录制、录制结束、转为 MP4、失败和用户操作。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::TaskId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    LiveStart,
    LiveEnd,
    RecordStart,
    RecordEnd,
    Failure,
    /// 不影响录制、但用户应当知道的问题，例如代理协议不支持而改为直连。
    Warning,
    UserAction,
    /// 录后转 MP4：开始和完成。转换失败记为 `Failure`，退出时中止记为 `Warning`。
    Convert,
}

impl ActivityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActivityKind::LiveStart => "live_start",
            ActivityKind::LiveEnd => "live_end",
            ActivityKind::RecordStart => "record_start",
            ActivityKind::RecordEnd => "record_end",
            ActivityKind::Failure => "failure",
            ActivityKind::Warning => "warning",
            ActivityKind::UserAction => "user_action",
            ActivityKind::Convert => "convert",
        }
    }

    pub fn parse(value: &str) -> Option<ActivityKind> {
        [
            ActivityKind::LiveStart,
            ActivityKind::LiveEnd,
            ActivityKind::RecordStart,
            ActivityKind::RecordEnd,
            ActivityKind::Failure,
            ActivityKind::Warning,
            ActivityKind::UserAction,
            ActivityKind::Convert,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == value)
    }
}

/// 一条动态。任务删除后动态保留，`anchor_name` 是写入时的主播名。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub id: i64,
    /// Unix 毫秒。
    pub at: i64,
    pub task_id: Option<TaskId>,
    pub anchor_name: String,
    pub kind: ActivityKind,
    /// 给用户看的一句话，例如「检测到开播」。
    pub message: String,
    /// 附加信息，例如文件路径、错误类别。
    pub detail: Option<Value>,
}

/// 待写入的动态。
#[derive(Debug, Clone, PartialEq)]
pub struct NewActivity {
    pub task_id: Option<TaskId>,
    pub anchor_name: String,
    pub kind: ActivityKind,
    pub message: String,
    pub detail: Option<Value>,
}

pub const DEFAULT_PAGE_SIZE: u32 = 50;
pub const MAX_PAGE_SIZE: u32 = 200;

/// `list_activity` 的筛选与分页参数。按 id 倒序，`before` 为上一页的 `next_before`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActivityQuery {
    pub task_id: Option<TaskId>,
    /// 为空表示全部类型。
    pub kinds: Vec<ActivityKind>,
    pub before: Option<i64>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityPage {
    pub items: Vec<ActivityEntry>,
    /// 还有更早的记录时，作为下一页的 `before`。
    pub next_before: Option<i64>,
}
