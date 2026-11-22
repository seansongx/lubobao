//! 子命令的输出：结果写到 stdout，错误写到 stderr。`--json` 时输出原始 JSON。

use std::process::ExitCode;

use chrono::{Local, TimeZone};
use serde_json::Value;

use crate::protocol::{Failure, Request};

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
    println!(
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
        println!("      正在写入 {path}");
    }
    if let Some(reason) = task.get("displayReason").and_then(Value::as_str) {
        println!("      {reason}");
    }
}

fn print_tasks(list: &Value) {
    let tasks = list.as_array().cloned().unwrap_or_default();
    if tasks.is_empty() {
        println!("还没有直播间，用 `lubobao-cli add <链接>` 添加");
        return;
    }
    println!("  id  状态      平台    主播              画质  链接");
    for task in &tasks {
        print_task(task);
    }
}

fn print_value(value: &Value) {
    match value {
        Value::String(text) => println!("{text}"),
        other => println!("{other}"),
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
            println!("{line}");
        }
        "external" => match value.get("error").and_then(Value::as_str) {
            Some(error) => println!("指定的 FFmpeg 无法运行（{}）：{error}", text(value, "path")),
            None => println!(
                "使用指定的 FFmpeg：{}（{}）",
                text(value, "ffmpegVersion"),
                text(value, "path")
            ),
        },
        "notInstalled" => {
            println!("FFmpeg 未安装，运行 `lubobao-cli ffmpeg install` 或启动 run 时自动下载")
        }
        "downloading" => {
            let downloaded = value.get("downloaded").and_then(Value::as_u64).unwrap_or(0);
            let total = value
                .get("total")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .max(1);
            println!(
                "FFmpeg 正在从 {} 下载：{}%",
                text(value, "source"),
                downloaded * 100 / total
            );
        }
        "failed" => println!("FFmpeg 不可用：{}", text(value, "reason")),
        "checking" => println!("FFmpeg 正在检查"),
        "verifying" => println!("FFmpeg 正在校验"),
        "waiting" => println!("FFmpeg 正在由另一个进程安装"),
        _ => println!("{value}"),
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
        println!("run：运行中（pid {pid}，{since} 启动）");
    } else {
        println!("run：未运行");
    }
    println!("数据目录：{}", text(value, "dataDir"));
    println!("录像目录：{}", text(value, "saveDir"));
    print!("FFmpeg：");
    if let Some(ffmpeg) = value.get("ffmpeg") {
        print_ffmpeg(ffmpeg);
    } else {
        println!();
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
        println!("录制中：{recording}  检测中：{checking}  下次检测：{next}");
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
        println!("直播间：{total}");
    } else {
        println!("直播间：{total}（{}）", parts.join("，"));
    }
}

fn print_mutation(value: &Value) {
    match text(value, "result") {
        "added" => {
            let task = value.get("task").cloned().unwrap_or(Value::Null);
            println!(
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
            println!(
                "{} {} 已在列表中（{}），确认仍要添加请加 --force",
                text(&candidate, "platformName"),
                text(&candidate, "anchorName"),
                ids.join("、")
            );
        }
        _ => println!("{value}"),
    }
}

fn print_settings(value: &Value) {
    if let Value::Object(map) = value {
        for (key, value) in map {
            println!("{key} = {value}");
        }
    }
}

fn print_failure(failure: &Failure) -> ExitCode {
    eprintln!("错误：{}", failure.message);
    match failure.kind.as_str() {
        "not_running" => ExitCode::from(3),
        _ => ExitCode::FAILURE,
    }
}

pub fn print(request: &Request, json: bool, result: Result<Value, Failure>) -> ExitCode {
    let value = match result {
        Ok(value) => value,
        Err(failure) => return print_failure(&failure),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return ExitCode::SUCCESS;
    }
    match request {
        Request::Add { .. } => {
            print_mutation(&value);
            if text(&value, "result") == "duplicate" {
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
            println!("已{action} {count} 个直播间");
        }
        Request::Remove { .. } => {
            let count = value.as_array().map(Vec::len).unwrap_or(0);
            println!("已删除 {count} 个直播间");
        }
        Request::Stop { .. } => {
            let tasks = value.as_array().cloned().unwrap_or_default();
            println!("已请求停止录制，本次直播不再自动录制：");
            for task in &tasks {
                print_task(task);
            }
        }
        Request::Status => print_status(&value),
        Request::ConfigList => print_settings(&value),
        Request::ConfigGet { .. } => print_value(&value),
        Request::ConfigSet { .. } => print_settings(&value),
        Request::FfmpegStatus | Request::FfmpegInstall => print_ffmpeg(&value),
        Request::Shutdown => println!("run 正在收尾并退出"),
    }
    ExitCode::SUCCESS
}
