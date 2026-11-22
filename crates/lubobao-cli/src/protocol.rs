//! 控制通道的请求和应答：一行 JSON 请求，一行 JSON 应答，然后关闭连接。

use lubobao_core::platforms::Quality;
use lubobao_core::tasks::{CommandError, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    All,
    Ids(Vec<TaskId>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Add {
        url: String,
        quality: Option<Quality>,
        notify_only: Option<bool>,
        note: Option<String>,
        force: bool,
    },
    List,
    SetMonitoring {
        target: Target,
        enabled: bool,
    },
    Remove {
        ids: Vec<TaskId>,
    },
    Stop {
        target: Target,
    },
    Status,
    ConfigList,
    ConfigGet {
        key: String,
    },
    /// `value` 为命令行上的原文，按该设置默认值的类型解析。
    ConfigSet {
        key: String,
        value: String,
    },
    FfmpegStatus,
    FfmpegInstall,
    Shutdown,
}

impl Request {
    /// 只能由正在运行的 `run` 执行的请求。
    pub fn needs_daemon(&self) -> bool {
        matches!(self, Request::Stop { .. } | Request::Shutdown)
    }

    /// 只涉及 FFmpeg 组件、不需要打开任务数据库的请求。
    pub fn is_ffmpeg(&self) -> bool {
        matches!(self, Request::FfmpegStatus | Request::FfmpegInstall)
    }
}

/// 错误类别沿用任务核心的 `ErrorKind`（snake_case），另加 `not_running`、`busy`、`protocol`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub kind: String,
    pub message: String,
}

impl Failure {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        Failure {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

impl From<CommandError> for Failure {
    fn from(error: CommandError) -> Self {
        let kind = serde_json::to_value(error.kind)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "error".into());
        Failure {
            kind,
            message: error.message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(Value),
    Err(Failure),
}

impl From<Result<Value, Failure>> for Response {
    fn from(result: Result<Value, Failure>) -> Self {
        match result {
            Ok(value) => Response::Ok(value),
            Err(failure) => Response::Err(failure),
        }
    }
}

impl From<Response> for Result<Value, Failure> {
    fn from(response: Response) -> Self {
        match response {
            Response::Ok(value) => Ok(value),
            Response::Err(failure) => Err(failure),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let request = Request::SetMonitoring {
            target: Target::Ids(vec![1, 2]),
            enabled: false,
        };
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(
            text,
            r#"{"op":"set_monitoring","target":{"ids":[1,2]},"enabled":false}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);
        let all = serde_json::to_string(&Request::Stop {
            target: Target::All,
        })
        .unwrap();
        assert_eq!(all, r#"{"op":"stop","target":"all"}"#);
    }

    #[test]
    fn command_errors_keep_their_kind() {
        let failure: Failure =
            CommandError::new(lubobao_core::tasks::ErrorKind::RecordingActive, "正在录制").into();
        assert_eq!(failure.kind, "recording_active");
    }
}
