//! `run` 的日志：每行一条，写到 stderr，格式为 `时间 级别 内容`。
//!
//! 桌面端的系统通知在命令行里没有：开播、下播、录制开始和结束、需处理等事件都写成日志。

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::Local;
use lubobao_core::ffmpeg::{self, FfmpegStatus};
use lubobao_core::tasks::{ActivityEntry, ActivityKind, DisplayStatus, TaskEvent, TaskId};
use serde_json::Value;

#[derive(Clone, Copy)]
enum Level {
    Info,
    Warn,
    Error,
}

fn write(level: Level, message: &str) {
    let level = match level {
        Level::Info => "INFO ",
        Level::Warn => "WARN ",
        Level::Error => "ERROR",
    };
    let now = Local::now().format("%Y-%m-%d %H:%M:%S");
    // 多行内容逐行加前缀，便于 grep 和 journald。
    for line in message.lines() {
        eprintln!("{now} {level} {line}");
    }
}

pub fn info(message: &str) {
    write(Level::Info, message);
}

pub fn warn(message: &str) {
    write(Level::Warn, message);
}

pub fn error(message: &str) {
    write(Level::Error, message);
}

/// 动态附加信息里给人看的部分：文件路径和原因。
fn detail_summary(detail: &Option<Value>) -> String {
    let Some(Value::Object(map)) = detail else {
        return String::new();
    };
    let mut parts = Vec::new();
    for key in ["filePath", "file", "path", "output", "reason", "error"] {
        if let Some(Value::String(text)) = map.get(key)
            && !text.is_empty()
        {
            parts.push(text.clone());
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("（{}）", parts.join("；"))
    }
}

fn log_activity(entry: &ActivityEntry) {
    let level = match entry.kind {
        ActivityKind::Failure => Level::Error,
        ActivityKind::Warning => Level::Warn,
        _ => Level::Info,
    };
    let who = if entry.anchor_name.is_empty() {
        String::new()
    } else {
        format!("[{}] ", entry.anchor_name)
    };
    let task = entry
        .task_id
        .map(|id| format!("#{id} "))
        .unwrap_or_default();
    write(
        level,
        &format!(
            "{task}{who}{}{}",
            entry.message,
            detail_summary(&entry.detail)
        ),
    );
}

/// 任务核心的事件：动态全部写日志；任务进入「需处理」时另写一条警告。
pub fn task_listener() -> lubobao_core::tasks::Listener {
    let last_display: Mutex<HashMap<TaskId, DisplayStatus>> = Mutex::new(HashMap::new());
    Box::new(move |event| match event {
        TaskEvent::Activity(entry) => log_activity(&entry),
        TaskEvent::Updated(snapshot) => {
            let mut map = last_display.lock().unwrap_or_else(|e| e.into_inner());
            let previous = map.insert(snapshot.id, snapshot.display);
            if snapshot.display == DisplayStatus::NeedsAction
                && previous != Some(DisplayStatus::NeedsAction)
            {
                warn(&format!(
                    "#{} [{}] 需处理：{}",
                    snapshot.id,
                    snapshot.config.anchor_name,
                    snapshot.display_reason.as_deref().unwrap_or("请查看动态")
                ));
            }
        }
        TaskEvent::Removed(ids) => {
            let mut map = last_display.lock().unwrap_or_else(|e| e.into_inner());
            for id in ids {
                map.remove(&id);
            }
        }
    })
}

/// 一行说明 FFmpeg 状态。
pub fn describe_ffmpeg(status: &FfmpegStatus) -> String {
    match status {
        FfmpegStatus::Checking => "FFmpeg：正在检查".into(),
        FfmpegStatus::NotInstalled => "FFmpeg：未安装".into(),
        FfmpegStatus::Waiting => "FFmpeg：另一个进程正在安装，等待中".into(),
        FfmpegStatus::Downloading {
            downloaded,
            total,
            source,
        } => {
            let percent = if *total > 0 {
                *downloaded as f64 * 100.0 / *total as f64
            } else {
                0.0
            };
            format!(
                "FFmpeg：正在从 {source} 下载 {percent:.0}%（{:.1}/{:.1} MiB）",
                *downloaded as f64 / 1_048_576.0,
                *total as f64 / 1_048_576.0
            )
        }
        FfmpegStatus::Verifying => "FFmpeg：正在校验".into(),
        FfmpegStatus::Ready {
            path,
            ffmpeg_version,
            outdated,
            install_error,
            ..
        } => {
            let mut text = format!("FFmpeg：可用 {ffmpeg_version}（{}）", path.display());
            if *outdated {
                text.push_str("，使用旧版本");
            }
            if let Some(error) = install_error {
                text.push_str(&format!("，新版本安装失败：{error}"));
            }
            text
        }
        FfmpegStatus::Failed { reason, retryable } => {
            let hint = if *retryable {
                "，可运行 `lubobao-cli ffmpeg install` 重试"
            } else {
                ""
            };
            format!("FFmpeg：不可用：{reason}{hint}")
        }
    }
}

/// FFmpeg 状态变化写日志。下载进度每 10% 写一次。
pub fn ffmpeg_listener() -> ffmpeg::Listener {
    let last_step: Mutex<Option<u64>> = Mutex::new(None);
    Box::new(move |status| {
        let mut step = last_step.lock().unwrap_or_else(|e| e.into_inner());
        if let FfmpegStatus::Downloading {
            downloaded, total, ..
        } = status
        {
            let current = if *total > 0 {
                downloaded * 10 / total
            } else {
                0
            };
            if *step == Some(current) {
                return;
            }
            *step = Some(current);
        } else {
            *step = None;
        }
        match status {
            FfmpegStatus::Failed { .. } => error(&describe_ffmpeg(status)),
            _ => info(&describe_ffmpeg(status)),
        }
    })
}
