//! 子命令的输出：结果写到 stdout，错误写到 stderr。`--json` 时输出原始 JSON。

use std::io::Write;
use std::process::ExitCode;

use chrono::{Local, TimeZone};
use serde_json::Value;

use crate::protocol::{Failure, Request};

/// 写一行到 stdout。下游已关闭（例如 `lubobao-cli list | head`）时安静地退出，不报错。
fn emit(args: std::fmt::Arguments<'_>, newline: bool) {
    let mut stdout = std::io::stdout().lock();
    let result = stdout.write_fmt(args).and_then(|()| {
        if newline {
            stdout.write_all(b"\n")
        } else {
            Ok(())
        }
    });
    if let Err(error) = result.and_then(|()| stdout.flush()) {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("无法写出结果：{error}");
        std::process::exit(1);
    }
}

macro_rules! outln {
    () => { emit(format_args!(""), true) };
    ($($arg:tt)*) => { emit(format_args!($($arg)*), true) };
}

macro_rules! out {
    ($($arg:tt)*) => { emit(format_args!($($arg)*), false) };
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn display_label(key: &str) -> &'static str {
    match key {
        "recording" => "录制中",
        "checking" => "检测中",
        "waiting" => "等待开播",
        "retrying" => "重试中",
        "notify" => "仅通知",
        "needsAction" => "需处理",
        "paused" => "已暂停",
        _ => "未知",
    }
}

fn time_of(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// 一个任务一行：id、状态、平台、主播、画质、链接；有原因或正在录制的文件时另起一行。
fn print_task(task: &Value) {
    let id = task.get("id").and_then(Value::as_i64).unwrap_or_default();
    let display = display_label(text(task, "display"));
    let mut flags = String::new();
    if task.get("notifyOnly").and_then(Value::as_bool) == Some(true) {
        flags.push_str(" 仅通知");
    }
    outln!(
        "{id:>4}  {display:<4}  {:<6}  {:<16}  {:<3}{flags}  {}",
        text(task, "platformName"),
        text(task, "anchorName"),
        text(task, "quality"),
        text(task, "url"),
    );
    let record = task.get("record").cloned().unwrap_or(Value::Null);
    if let Some(path) = record.get("filePath").and_then(Value::as_str)
        && matches!(text(&record, "state"), "recording" | "finishing")
    {
        outln!("      正在写入 {path}");
    }
    if let Some(reason) = task.get("displayReason").and_then(Value::as_str) {
        outln!("      {reason}");
    }
}

fn print_tasks(list: &Value) {
    let tasks = list.as_array().cloned().unwrap_or_default();
    if tasks.is_empty() {
        outln!("还没有直播间，用 `lubobao-cli add <链接>` 添加");
        return;
    }
    outln!("  id  状态      平台    主播              画质  链接");
    for task in &tasks {
        print_task(task);
    }
}

fn print_value(value: &Value) {
    match value {
        Value::String(text) => outln!("{text}"),
        other => outln!("{other}"),
    }
}

fn print_ffmpeg(value: &Value) {
    let state = text(value, "state");
    match state {
        "ready" => {
            let mut line = format!(
                "FFmpeg 可用：{}（{}）",
                text(value, "ffmpegVersion"),
                text(value, "path")
            );
            if value.get("outdated").and_then(Value::as_bool) == Some(true) {
                line.push_str("，使用旧版本");
            }
            if let Some(error) = value.get("installError").and_then(Value::as_str) {
                line.push_str(&format!("，新版本安装失败：{error}"));
            }
            outln!("{line}");
        }
        "external" => match value.get("error").and_then(Value::as_str) {
            Some(error) => outln!("指定的 FFmpeg 无法运行（{}）：{error}", text(value, "path")),
            None => outln!(
                "使用指定的 FFmpeg：{}（{}）",
                text(value, "ffmpegVersion"),
                text(value, "path")
            ),
        },
        "notInstalled" => {
            outln!("FFmpeg 未安装，运行 `lubobao-cli ffmpeg install` 或启动 run 时自动下载")
        }
        "downloading" => {
            let downloaded = value.get("downloaded").and_then(Value::as_u64).unwrap_or(0);
            let total = value
                .get("total")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .max(1);
            outln!(
                "FFmpeg 正在从 {} 下载：{}%",
                text(value, "source"),
                downloaded * 100 / total
            );
        }
        "failed" => outln!("FFmpeg 不可用：{}", text(value, "reason")),
        "checking" => outln!("FFmpeg 正在检查"),
        "verifying" => outln!("FFmpeg 正在校验"),
        "waiting" => outln!("FFmpeg 正在由另一个进程安装"),
        _ => outln!("{value}"),
    }
}

fn print_status(value: &Value) {
    let running = value.get("running").and_then(Value::as_bool) == Some(true);
    if running {
        let pid = value.get("pid").and_then(Value::as_u64).unwrap_or_default();
        let since = value
            .get("startedAt")
            .and_then(Value::as_i64)
            .map(time_of)
            .unwrap_or_default();
        outln!("run：运行中（pid {pid}，{since} 启动）");
    } else {
        outln!("run：未运行");
    }
    outln!("数据目录：{}", text(value, "dataDir"));
    outln!("录像目录：{}", text(value, "saveDir"));
    out!("FFmpeg：");
    if let Some(ffmpeg) = value.get("ffmpeg") {
        print_ffmpeg(ffmpeg);
    } else {
        outln!();
    }
    if let Some(scheduler) = value.get("scheduler").filter(|v| !v.is_null()) {
        let recording = scheduler
            .get("recording")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let checking = scheduler
            .get("checking")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let next = scheduler
            .get("nextCheckAt")
            .and_then(Value::as_i64)
            .map(time_of)
            .unwrap_or_else(|| "—".into());
        outln!("录制中：{recording}  检测中：{checking}  下次检测：{next}");
    }
    let tasks = value.get("tasks").cloned().unwrap_or(Value::Null);
    let total = tasks.get("total").and_then(Value::as_u64).unwrap_or(0);
    let mut parts = Vec::new();
    if let Some(Value::Object(map)) = tasks.get("byStatus") {
        for (key, count) in map {
            parts.push(format!("{} {}", display_label(key), count));
        }
    }
    if parts.is_empty() {
        outln!("直播间：{total}");
    } else {
        outln!("直播间：{total}（{}）", parts.join("，"));
    }
}

fn print_mutation(value: &Value) {
    match text(value, "result") {
        "added" => {
            let task = value.get("task").cloned().unwrap_or(Value::Null);
            outln!(
                "已添加 #{}：{} {}",
                task.get("id").and_then(Value::as_i64).unwrap_or_default(),
                text(&task, "platformName"),
                text(&task, "anchorName")
            );
        }
        "duplicate" => {
            let candidate = value.get("candidate").cloned().unwrap_or(Value::Null);
            let ids: Vec<String> = value
                .get("existing")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|task| task.get("id").and_then(Value::as_i64))
                        .map(|id| format!("#{id}"))
                        .collect()
                })
                .unwrap_or_default();
            outln!(
                "{} {} 已在列表中（{}），确认仍要添加请加 --force",
                text(&candidate, "platformName"),
                text(&candidate, "anchorName"),
                ids.join("、")
            );
        }
        _ => outln!("{value}"),
    }
}

fn print_settings(value: &Value) {
    if let Value::Object(map) = value {
        for (key, value) in map {
            outln!("{key} = {value}");
        }
    }
}

/// 退出码：1 执行失败，2 参数或设置值无效，3 需要 `run` 但它没有运行。
pub fn exit_code_for(kind: &str) -> u8 {
    match kind {
        "not_running" => 3,
        "invalid_input" => 2,
        _ => 1,
    }
}

fn print_failure(failure: &Failure) -> ExitCode {
    eprintln!("错误：{}", failure.message);
    ExitCode::from(exit_code_for(&failure.kind))
}

pub fn print(request: &Request, json: bool, result: Result<Value, Failure>) -> ExitCode {
    let value = match result {
        Ok(value) => value,
        Err(failure) => return print_failure(&failure),
    };
    // 重复添加没有写入，与不加 --json 时一样按失败退出。
    let duplicate = matches!(request, Request::Add { .. }) && text(&value, "result") == "duplicate";
    if json {
        outln!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return if duplicate {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }
    match request {
        Request::Add { .. } => {
            print_mutation(&value);
            if duplicate {
                return ExitCode::FAILURE;
            }
        }
        Request::List => print_tasks(&value),
        Request::SetMonitoring { enabled, .. } => {
            let count = value.as_array().map(Vec::len).unwrap_or(0);
            let action = if *enabled {
                "恢复监控"
            } else {
                "暂停监控"
            };
            outln!("已{action} {count} 个直播间");
        }
        Request::Remove { .. } => {
            let count = value.as_array().map(Vec::len).unwrap_or(0);
            outln!("已删除 {count} 个直播间");
        }
        Request::Stop { .. } => {
            let tasks = value.as_array().cloned().unwrap_or_default();
            outln!("已请求停止录制，本次直播不再自动录制：");
            for task in &tasks {
                print_task(task);
            }
        }
        Request::Start { .. } => {
            let tasks = value.as_array().cloned().unwrap_or_default();
            if tasks.is_empty() {
                outln!(
                    "没有可以开始录制的直播间：已暂停监控的请先 resume，仅通知的不录制，正在录制的无需开始"
                );
            } else {
                outln!("已重新开始录制，正在检测是否开播：");
                for task in &tasks {
                    print_task(task);
                }
            }
        }
        Request::Status => print_status(&value),
        Request::ConfigList => print_settings(&value),
        Request::ConfigGet { .. } => print_value(&value),
        Request::ConfigSet { .. } => print_settings(&value),
        Request::FfmpegStatus | Request::FfmpegInstall => print_ffmpeg(&value),
        Request::Shutdown => outln!("run 正在收尾并退出"),
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_documentation() {
        assert_eq!(exit_code_for("not_running"), 3);
        assert_eq!(exit_code_for("invalid_input"), 2);
        assert_eq!(exit_code_for("recording_active"), 1);
        assert_eq!(exit_code_for("network"), 1);
    }
}
