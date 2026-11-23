//! 执行请求：`run` 收到控制通道的请求时，以及 `run` 没有运行时由子命令直接执行。

use std::sync::Arc;
use std::time::Duration;

use lubobao_core::ffmpeg::FfmpegStatus;
use lubobao_core::platforms::LiveInfo;
use lubobao_core::scheduler::Scheduler;
use lubobao_core::tasks::{self, AddTaskInput, DisplayStatus, TaskId, TaskManager, TaskSnapshot};
use serde_json::{Map, Value, json};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::GlobalOpts;
use crate::control::{self, SendError};
use crate::engine::FfmpegSource;
use crate::logging;
use crate::paths::Dirs;
use crate::protocol::{Failure, Request, Target};

/// 执行请求所需的状态。`scheduler` 为 `None` 表示 `run` 没有运行，直接操作数据库。
pub struct Ctx {
    pub tasks: Arc<TaskManager>,
    pub scheduler: Option<Arc<Scheduler>>,
    pub ffmpeg: FfmpegSource,
    pub dirs: Dirs,
    /// `run` 的退出请求。
    pub shutdown: Option<Arc<Notify>>,
    /// `run` 的启动时间，Unix 毫秒。
    pub started_at: Option<i64>,
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, Failure> {
    serde_json::to_value(value).map_err(|error| Failure::new("protocol", error.to_string()))
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::new("invalid_input", message)
}

pub fn not_running(dirs: &Dirs) -> Failure {
    Failure::new(
        "not_running",
        format!(
            "lubobao-cli run 没有在运行（数据目录 {}），这个命令需要先启动 run",
            dirs.data_dir.display()
        ),
    )
}

fn ids_of(tasks: &TaskManager, target: &Target) -> Vec<TaskId> {
    match target {
        Target::All => tasks.list().iter().map(|task| task.id).collect(),
        Target::Ids(ids) => ids.clone(),
    }
}

/// 不存在的 id 先报错，避免批量操作悄悄跳过。
fn check_ids(tasks: &TaskManager, ids: &[TaskId]) -> Result<(), Failure> {
    match ids.iter().find(|id| tasks.get(**id).is_none()) {
        Some(id) => Err(Failure::new("not_found", format!("任务 {id} 不存在"))),
        None => Ok(()),
    }
}

fn display_key(display: DisplayStatus) -> String {
    serde_json::to_value(display)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn task_counts(list: &[TaskSnapshot]) -> Value {
    let mut by_display = Map::new();
    for task in list {
        let entry = by_display
            .entry(display_key(task.display))
            .or_insert(Value::from(0));
        *entry = Value::from(entry.as_u64().unwrap_or(0) + 1);
    }
    json!({ "total": list.len(), "byStatus": by_display })
}

/// 按默认值的类型解析命令行上的设置值：字符串项直接用原文，其余按 JSON 解析。
pub fn parse_setting(
    defaults: &Map<String, Value>,
    key: &str,
    raw: &str,
) -> Result<Value, Failure> {
    let Some(default) = defaults.get(key) else {
        return Err(invalid(format!("未知的设置项：{key}")));
    };
    match default {
        Value::String(_) => Ok(Value::String(raw.to_string())),
        _ => serde_json::from_str(raw).map_err(|error| {
            invalid(format!(
                "设置项 {key} 需要 JSON 值（例如 true、180、2.5、{{}}）：{raw}（{error}）"
            ))
        }),
    }
}

/// 录像目录的设置项。
pub const SAVE_PATH_KEY: &str = "live_save_path";

/// 录像目录必须是非空的绝对路径。相对路径由客户端按自己的当前目录转换后再提交
/// （见 `main.rs`），这里兜底拒绝，避免按 `run` 的工作目录解释。
fn check_save_path(value: &Value) -> Result<(), Failure> {
    let path = value.as_str().unwrap_or_default().trim();
    if path.is_empty() {
        return Err(invalid("录像目录不能为空"));
    }
    if !std::path::Path::new(path).is_absolute() {
        return Err(invalid(format!("录像目录需要绝对路径：{path}")));
    }
    Ok(())
}

async fn ffmpeg_status(ctx_ffmpeg: &FfmpegSource, running: bool) -> Value {
    match ctx_ffmpeg {
        FfmpegSource::Managed(manager) if !running => {
            let status = manager.check_local().await;
            ctx_ffmpeg.status_json(Some(&status))
        }
        _ => ctx_ffmpeg.status_json(None),
    }
}

async fn ffmpeg_install(source: &FfmpegSource) -> Result<Value, Failure> {
    match source {
        FfmpegSource::Managed(manager) => {
            let status = manager.ensure().await;
            if let FfmpegStatus::Failed { reason, .. } = &status {
                return Err(Failure::new("ffmpeg", format!("FFmpeg 安装失败：{reason}")));
            }
            Ok(source.status_json(Some(&status)))
        }
        FfmpegSource::External { path, .. } => Err(Failure::new(
            "config",
            format!(
                "已用 --ffmpeg / LUBOBAO_FFMPEG 指定 {}，不需要安装",
                path.display()
            ),
        )),
    }
}

pub async fn execute(ctx: &Ctx, request: &Request) -> Result<Value, Failure> {
    let tasks = &ctx.tasks;
    let running = ctx.scheduler.is_some();
    match request {
        Request::Add { .. } => to_value(tasks.add_task(add_input(request)).await?),
        Request::List => to_value(tasks.list()),
        Request::SetMonitoring { target, enabled } => {
            let ids = ids_of(tasks, target);
            check_ids(tasks, &ids)?;
            to_value(tasks.set_monitoring(&ids, *enabled)?)
        }
        Request::Remove { ids } => {
            check_ids(tasks, ids)?;
            to_value(tasks.delete_tasks(ids)?)
        }
        Request::Stop { target } => {
            if !running {
                return Err(not_running(&ctx.dirs));
            }
            let ids = ids_of(tasks, target);
            check_ids(tasks, &ids)?;
            to_value(tasks.stop_recording(&ids)?)
        }
        Request::Status => {
            let list = tasks.list();
            let settings = tasks.settings();
            Ok(json!({
                "running": running,
                "pid": running.then(std::process::id),
                "startedAt": ctx.started_at,
                "version": env!("CARGO_PKG_VERSION"),
                "dataDir": ctx.dirs.data_dir,
                "saveDir": settings.get("live_save_path"),
                "control": ctx.dirs.endpoint(),
                "scheduler": ctx.scheduler.as_ref().map(|scheduler| scheduler.status()),
                "ffmpeg": ffmpeg_status(&ctx.ffmpeg, running).await,
                "tasks": task_counts(&list),
            }))
        }
        Request::ConfigList => to_value(tasks.settings()),
        Request::ConfigGet { key } => match tasks.settings().get(key) {
            Some(value) => Ok(value.clone()),
            None => Err(invalid(format!("未知的设置项：{key}"))),
        },
        Request::ConfigSet { key, value } => {
            let parsed = parse_setting(tasks.defaults(), key, value)?;
            if key == SAVE_PATH_KEY {
                check_save_path(&parsed)?;
            }
            let mut values = Map::new();
            values.insert(key.clone(), parsed);
            let settings = tasks.update_settings(values)?;
            Ok(json!({ key.as_str(): settings.get(key) }))
        }
        Request::FfmpegStatus => Ok(ffmpeg_status(&ctx.ffmpeg, running).await),
        Request::FfmpegInstall => ffmpeg_install(&ctx.ffmpeg).await,
        Request::Shutdown => match &ctx.shutdown {
            Some(notify) => {
                notify.notify_one();
                Ok(json!({ "stopping": true }))
            }
            None => Err(not_running(&ctx.dirs)),
        },
    }
}

/// `run` 正在启动或另一个命令正在写数据库时，最多等这么久。
const BUSY_WAIT: Duration = Duration::from_secs(15);

/// 等 `run` 应答的时限。`add` 要请求平台（单次 20 秒，可能还要跟随跳转），
/// `ffmpeg install` 要下载几十 MB，放宽。
fn response_timeout(request: &Request) -> Duration {
    match request {
        Request::FfmpegInstall => Duration::from_secs(60 * 60),
        Request::Add { .. } => Duration::from_secs(120),
        _ => Duration::from_secs(30),
    }
}

/// `run` 不在时只读数据库的请求：不取实例锁，不处理上次异常中断的录制。
fn is_read_only(request: &Request) -> bool {
    matches!(
        request,
        Request::List | Request::Status | Request::ConfigList | Request::ConfigGet { .. }
    )
}

fn storage(message: String) -> Failure {
    Failure::new("storage", message)
}

/// 子命令入口：`run` 在运行时交给它；否则直接执行。写数据库时持有实例锁，避免与刚启动的
/// `run` 同时写；`add` 先联网解析（不持锁），再加锁写入。
pub async fn dispatch(
    dirs: &Dirs,
    global: &GlobalOpts,
    request: &Request,
) -> Result<Value, Failure> {
    let deadline = Instant::now() + BUSY_WAIT;
    let mut resolved: Option<LiveInfo> = None;
    loop {
        match control::send(dirs, request, response_timeout(request)).await {
            Ok(response) => return response.into(),
            Err(SendError::NotRunning) => {}
            Err(SendError::Failed(message)) => return Err(Failure::new("protocol", message)),
        }
        if request.is_ffmpeg() {
            // FFmpeg 组件自带跨进程的安装锁，不需要数据库，也就不占实例锁。
            let source =
                FfmpegSource::new(dirs, global.ffmpeg.as_ref(), logging::ffmpeg_listener());
            return match request {
                Request::FfmpegInstall => ffmpeg_install(&source).await,
                _ => Ok(ffmpeg_status(&source, false).await),
            };
        }
        if is_read_only(request) {
            let manager =
                tasks::open_read_only_in(&dirs.data_dir, &dirs.save_dir).map_err(storage)?;
            return execute(&offline_ctx(dirs, global, manager), request).await;
        }
        if let (Request::Add { url, quality, .. }, None) = (request, &resolved) {
            let manager =
                tasks::open_read_only_in(&dirs.data_dir, &dirs.save_dir).map_err(storage)?;
            let quality = quality.unwrap_or_else(|| manager.default_quality());
            resolved = Some(TaskManager::resolve(url, quality, &manager.settings()).await?);
            // 解析期间 run 可能已经启动，回到开头重新尝试连接。
            continue;
        }
        let lock = dirs.try_lock().map_err(storage)?;
        if let Some(lock) = lock {
            if request.needs_daemon() {
                return Err(not_running(dirs));
            }
            let manager = tasks::open_in(&dirs.data_dir, &dirs.save_dir, Box::new(|_| {}))
                .map_err(storage)?;
            let ctx = offline_ctx(dirs, global, manager);
            let result = match (request, &resolved) {
                (Request::Add { .. }, Some(info)) => {
                    to_value(ctx.tasks.add_resolved(add_input(request), info)?)
                }
                _ => execute(&ctx, request).await,
            };
            drop(lock);
            return result;
        }
        if Instant::now() >= deadline {
            return Err(Failure::new(
                "busy",
                format!(
                    "数据目录 {} 被另一个 lubobao-cli 进程占用，但连不上它的控制通道 {}",
                    dirs.data_dir.display(),
                    dirs.endpoint()
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn offline_ctx(dirs: &Dirs, global: &GlobalOpts, manager: TaskManager) -> Ctx {
    Ctx {
        tasks: Arc::new(manager),
        scheduler: None,
        ffmpeg: FfmpegSource::new(dirs, global.ffmpeg.as_ref(), Box::new(|_| {})),
        dirs: dirs.clone(),
        shutdown: None,
        started_at: None,
    }
}

fn add_input(request: &Request) -> AddTaskInput {
    match request {
        Request::Add {
            url,
            quality,
            notify_only,
            note,
            force,
        } => AddTaskInput {
            url: url.clone(),
            quality: *quality,
            notify_only: *notify_only,
            note: note.clone(),
            force: *force,
        },
        _ => AddTaskInput::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_by_default_type() {
        let defaults = tasks::settings::defaults(std::path::Path::new("/videos"));
        assert_eq!(
            parse_setting(&defaults, "proxy_address", "127.0.0.1:7890").unwrap(),
            json!("127.0.0.1:7890")
        );
        assert_eq!(
            parse_setting(&defaults, "loop_time_seconds", "60").unwrap(),
            json!(60)
        );
        assert_eq!(
            parse_setting(&defaults, "convert_to_mp4", "false").unwrap(),
            json!(false)
        );
        assert_eq!(
            parse_setting(&defaults, "platform_cookies", r#"{"douyin":"a=b"}"#).unwrap(),
            json!({ "douyin": "a=b" })
        );
        assert_eq!(
            parse_setting(&defaults, "convert_to_mp4", "yes")
                .unwrap_err()
                .kind,
            "invalid_input"
        );
        assert_eq!(
            parse_setting(&defaults, "no_such_key", "1")
                .unwrap_err()
                .kind,
            "invalid_input"
        );
    }

    #[test]
    fn save_path_must_be_absolute_and_non_empty() {
        assert_eq!(
            check_save_path(&json!("")).unwrap_err().kind,
            "invalid_input"
        );
        assert_eq!(
            check_save_path(&json!("  ")).unwrap_err().kind,
            "invalid_input"
        );
        assert_eq!(
            check_save_path(&json!("videos")).unwrap_err().kind,
            "invalid_input"
        );
        let absolute = std::env::temp_dir().join("lubobao-videos");
        assert!(check_save_path(&json!(absolute.to_string_lossy())).is_ok());
    }

    #[test]
    fn read_only_requests_do_not_take_the_lock() {
        assert!(is_read_only(&Request::List));
        assert!(is_read_only(&Request::ConfigGet { key: "x".into() }));
        assert!(!is_read_only(&Request::ConfigSet {
            key: "x".into(),
            value: "1".into()
        }));
        assert!(!is_read_only(&Request::Remove { ids: vec![1] }));
    }

    #[test]
    fn long_requests_get_longer_timeouts() {
        assert!(response_timeout(&Request::FfmpegInstall) > response_timeout(&Request::List));
        assert_eq!(response_timeout(&Request::Status), Duration::from_secs(30));
    }

    #[test]
    fn display_keys_match_frontend_names() {
        assert_eq!(display_key(DisplayStatus::NeedsAction), "needsAction");
        assert_eq!(display_key(DisplayStatus::Recording), "recording");
    }
}
