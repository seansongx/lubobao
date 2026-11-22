//! 录制结束原因的判断。

use serde::{Deserialize, Serialize};

/// 一次录制为什么结束。界面据此显示原因，调度据此决定是否重新检测开播。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EndReason {
    /// 用户手动停止。
    UserStopped,
    /// 直播结束或断流：收到过数据后，源站结束或断开了连接。
    StreamEnded,
    /// 网络错误：连不上源地址、HTTP 错误、读写超时，或长时间收不到新数据。
    NetworkError,
    /// 磁盘剩余空间低于阈值（录制前或录制中检测到）。
    DiskFull,
    /// ffmpeg 无法启动、参数或输出无效，或在收到数据前异常退出。
    FfmpegFailed,
}

impl EndReason {
    /// 调度是否应该重新检测直播状态并在仍在直播时重新开始录制。
    pub fn should_recheck(self) -> bool {
        matches!(self, EndReason::StreamEnded | EndReason::NetworkError)
    }

    /// 是否需要用户处理（界面显示为「需处理」）。
    pub fn needs_attention(self) -> bool {
        matches!(self, EndReason::DiskFull | EndReason::FfmpegFailed)
    }

    /// 中文短标签。
    pub fn label(self) -> &'static str {
        match self {
            EndReason::UserStopped => "已手动停止",
            EndReason::StreamEnded => "直播结束或断流",
            EndReason::NetworkError => "网络错误",
            EndReason::DiskFull => "磁盘空间不足",
            EndReason::FfmpegFailed => "FFmpeg 异常退出",
        }
    }
}

/// 引擎主动发起停止的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopCause {
    /// 用户调用了 `stop`。
    User,
    /// 录制中检测到磁盘剩余空间低于阈值。
    DiskFull,
    /// 超过看门狗时限没有新数据。
    Stalled,
}

/// ffmpeg 退出时收集到的信息。
#[derive(Debug, Clone, Default)]
pub struct ExitSummary<'a> {
    /// 引擎是否主动停止，以及原因。
    pub stop_cause: Option<StopCause>,
    /// 进程退出码；被强制结束且没有退出码时为 `None`。
    pub exit_code: Option<i32>,
    /// 退出前是否收到过媒体数据。
    pub received_media: bool,
    /// stderr 中最近的错误行（不含统计行）。
    pub stderr_tail: &'a [String],
}

/// 读写超时、连接被重置等说明网络中断的错误文本。Windows 版 ffmpeg 常把 WinSock
/// 错误打印成 `Error number -138 occurred`（ETIMEDOUT）、`-10054`（WSAECONNRESET）、
/// `-10060`（WSAETIMEDOUT）、`-10061`（WSAECONNREFUSED）、`-10065`（WSAEHOSTUNREACH）。
const TIMEOUT_MARKERS: &[&str] = &[
    "timed out",
    "timeout",
    "error number -138 ",
    "error number -10060 ",
];

/// 连接阶段失败的错误文本（小写匹配）。
const NETWORK_MARKERS: &[&str] = &[
    "connection to tcp://",
    "connection refused",
    "connection reset",
    "failed to resolve hostname",
    "network is unreachable",
    "no route to host",
    "host is unreachable",
    "server returned",
    "http error",
    "i/o error",
    "error number -10054 ",
    "error number -10061 ",
    "error number -10065 ",
    "error number -11001 ",
    "[tls @",
    "[https @",
];

fn contains_any(lines: &[String], markers: &[&str]) -> bool {
    lines.iter().any(|line| {
        let line = format!("{} ", line.to_lowercase());
        markers.iter().any(|marker| line.contains(marker))
    })
}

/// 最后一条有用的错误信息，用于界面提示。
pub fn last_error(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .cloned()
}

/// 判断结束原因，返回原因和给用户看的说明。
///
/// 规则：
/// 1. 引擎主动停止的，按停止原因：用户停止、磁盘不足；看门狗停止记为网络错误。
/// 2. 收到过数据后 ffmpeg 自行退出：错误里有读写超时记为网络错误，否则记为直播结束或断流
///    （源站正常关闭和连接被重置都属于断流，退出码不影响判断）。
/// 3. 没收到数据就退出：有连接、HTTP 或 TLS 错误记为网络错误；正常退出记为直播结束；
///    其余记为 ffmpeg 异常退出。
pub fn classify(summary: &ExitSummary<'_>) -> (EndReason, Option<String>) {
    let tail = summary.stderr_tail;
    match summary.stop_cause {
        Some(StopCause::User) => return (EndReason::UserStopped, None),
        Some(StopCause::DiskFull) => {
            return (
                EndReason::DiskFull,
                Some("磁盘剩余空间低于阈值，已停止录制".to_string()),
            );
        }
        Some(StopCause::Stalled) => {
            return (
                EndReason::NetworkError,
                Some("长时间没有收到新数据，已停止录制".to_string()),
            );
        }
        None => {}
    }

    let detail = last_error(tail);
    if summary.received_media {
        if contains_any(tail, TIMEOUT_MARKERS) {
            return (EndReason::NetworkError, detail);
        }
        return (EndReason::StreamEnded, detail);
    }
    if contains_any(tail, TIMEOUT_MARKERS) || contains_any(tail, NETWORK_MARKERS) {
        return (EndReason::NetworkError, detail);
    }
    if summary.exit_code == Some(0) && tail.is_empty() {
        return (
            EndReason::StreamEnded,
            Some("没有收到媒体数据，源站已结束推流".to_string()),
        );
    }
    let detail = detail.or_else(|| {
        Some(match summary.exit_code {
            Some(code) => format!("ffmpeg 异常退出，退出码 {code}"),
            None => "ffmpeg 被强制结束".to_string(),
        })
    });
    (EndReason::FfmpegFailed, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn classify_with(
        stop_cause: Option<StopCause>,
        exit_code: Option<i32>,
        received_media: bool,
        tail: &[String],
    ) -> EndReason {
        classify(&ExitSummary {
            stop_cause,
            exit_code,
            received_media,
            stderr_tail: tail,
        })
        .0
    }

    // 真实样本：gyan.dev FFmpeg 9.0.2 在 Windows 上的输出。
    fn refused() -> Vec<String> {
        lines(&[
            "[tcp @ 000001ba43a33700] Connection to tcp://127.0.0.1:9 failed: Error number -138 occurred",
            "[in#0 @ 000001ba43a31080] Error opening input: Error number -138 occurred",
            "Error opening input file http://127.0.0.1:9/live.flv.",
            "Error opening input files: Error number -138 occurred",
        ])
    }

    fn server_closed() -> Vec<String> {
        lines(&[
            "[in#0/flv @ 000001dd9cedee80] Attempted to read next track in single-track mode.",
            "[http @ 000001dd9cf43ec0] Stream ends prematurely at 88168, should be 18446744073709551615",
            "[in#0/flv @ 000001dd9cedec00] Error during demuxing: I/O error",
        ])
    }

    fn connection_reset() -> Vec<String> {
        lines(&[
            "[http @ 0000025863bdf0c0] Stream ends prematurely at 88274, should be 18446744073709551615",
            "[out#0/mpegts @ 0000020acf3daf80] Error muxing a packet",
            "[out#0/mpegts @ 0000020acf3daf80] Task finished with error code: -1094995529 (Invalid data found when processing input)",
        ])
    }

    #[test]
    fn engine_stop_causes_take_priority() {
        let tail = server_closed();
        assert_eq!(
            classify_with(Some(StopCause::User), Some(1), true, &tail),
            EndReason::UserStopped
        );
        assert_eq!(
            classify_with(Some(StopCause::DiskFull), Some(0), true, &[]),
            EndReason::DiskFull
        );
        assert_eq!(
            classify_with(Some(StopCause::Stalled), None, true, &[]),
            EndReason::NetworkError
        );
    }

    #[test]
    fn disconnect_after_data_is_stream_end() {
        assert_eq!(
            classify_with(None, Some(0), true, &server_closed()),
            EndReason::StreamEnded
        );
        assert_eq!(
            classify_with(None, Some(183), true, &connection_reset()),
            EndReason::StreamEnded
        );
        assert_eq!(
            classify_with(None, Some(0), true, &[]),
            EndReason::StreamEnded
        );
    }

    #[test]
    fn read_timeout_after_data_is_network_error() {
        let tail = lines(&["[in#0/flv @ 0] Error during demuxing: Connection timed out"]);
        assert_eq!(
            classify_with(None, Some(0), true, &tail),
            EndReason::NetworkError
        );
    }

    #[test]
    fn unreachable_source_is_network_error() {
        let (reason, message) = classify(&ExitSummary {
            stop_cause: None,
            exit_code: Some(127),
            received_media: false,
            stderr_tail: &refused(),
        });
        assert_eq!(reason, EndReason::NetworkError);
        assert_eq!(
            message.as_deref(),
            Some("Error opening input files: Error number -138 occurred")
        );

        let http = lines(&[
            "[in#0 @ 0] Error opening input: Server returned 404 Not Found",
            "Error opening input files: Server returned 404 Not Found",
        ]);
        assert_eq!(
            classify_with(None, Some(8), false, &http),
            EndReason::NetworkError
        );
        let dns =
            lines(&["[tcp @ 0] Failed to resolve hostname nope.invalid: No such host is known."]);
        assert_eq!(
            classify_with(None, Some(1), false, &dns),
            EndReason::NetworkError
        );
    }

    #[test]
    fn early_non_network_failure_is_ffmpeg_failure() {
        let invalid = lines(&[
            "[in#0 @ 0] Error opening input: Invalid data found when processing input",
            "Error opening input files: Invalid data found when processing input",
        ]);
        assert_eq!(
            classify_with(None, Some(1), false, &invalid),
            EndReason::FfmpegFailed
        );
        let disk =
            lines(&["[out#0/mpegts @ 0] Error opening output D:\\a.ts: No space left on device"]);
        assert_eq!(
            classify_with(None, Some(1), false, &disk),
            EndReason::FfmpegFailed
        );
        let (reason, message) = classify(&ExitSummary {
            stop_cause: None,
            exit_code: Some(-22),
            received_media: false,
            stderr_tail: &[],
        });
        assert_eq!(reason, EndReason::FfmpegFailed);
        assert_eq!(message.as_deref(), Some("ffmpeg 异常退出，退出码 -22"));
    }

    #[test]
    fn clean_exit_without_data_is_stream_end() {
        assert_eq!(
            classify_with(None, Some(0), false, &[]),
            EndReason::StreamEnded
        );
    }

    #[test]
    fn recheck_policy_matches_reasons() {
        assert!(EndReason::StreamEnded.should_recheck());
        assert!(EndReason::NetworkError.should_recheck());
        assert!(!EndReason::UserStopped.should_recheck());
        assert!(!EndReason::DiskFull.should_recheck());
        assert!(!EndReason::FfmpegFailed.should_recheck());
        assert!(EndReason::DiskFull.needs_attention());
    }

    #[test]
    fn reasons_serialize_in_camel_case() {
        assert_eq!(
            serde_json::to_value(EndReason::NetworkError).unwrap(),
            serde_json::json!("networkError")
        );
        assert_eq!(
            serde_json::to_value(StopCause::DiskFull).unwrap(),
            serde_json::json!("diskFull")
        );
    }
}
