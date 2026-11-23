//! 录播宝命令行程序：在没有图形界面的服务器上监控直播间并自动录制。
//!
//! `lubobao-cli run` 前台常驻，持有数据目录的实例锁，并在数据目录里开一个本地控制通道
//! （Unix socket / Windows 命名管道）。其他子命令先尝试连接控制通道，交给正在运行的
//! `run` 执行；`run` 没有运行时，修改类命令直接操作数据库，下次 `run` 启动时生效。

mod control;
mod daemon;
mod engine;
mod logging;
mod ops;
mod output;
mod paths;
mod protocol;
#[cfg(windows)]
mod winsec;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use lubobao_core::platforms::Quality;
use lubobao_core::tasks::TaskId;

use protocol::{Request, Target};

#[derive(Parser)]
#[command(
    name = "lubobao-cli",
    bin_name = "lubobao-cli",
    version,
    about = "录播宝命令行：在没有图形界面的服务器上监控直播间，开播自动录制",
    after_help = "先运行 `lubobao-cli run`（前台常驻），再用其他子命令管理直播间。\n文档：https://github.com/seansongx/lubobao/blob/main/docs/cli.md"
)]
struct Cli {
    #[command(flatten)]
    global: GlobalOpts,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone, Debug, Default)]
pub struct GlobalOpts {
    /// 数据目录，存放任务数据库、FFmpeg 和控制通道。
    /// 默认：Linux 为 $XDG_DATA_HOME/lubobao（~/.local/share/lubobao）
    #[arg(long, env = "LUBOBAO_DATA_DIR", global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// 使用系统里已有的 ffmpeg，不自动下载
    #[arg(long, env = "LUBOBAO_FFMPEG", global = true, value_name = "PATH")]
    pub ffmpeg: Option<PathBuf>,

    /// 默认录像目录，即设置 live_save_path 的缺省值。默认为数据目录下的 recordings；
    /// 用 `config set live_save_path …` 改过的以设置为准
    #[arg(long, env = "LUBOBAO_SAVE_DIR", global = true, value_name = "DIR")]
    pub save_dir: Option<PathBuf>,

    /// 控制通道的位置（Unix socket 路径；Windows 上为命名管道名）。默认在数据目录里，
    /// 数据目录在网络盘上或路径太长时改用 $XDG_RUNTIME_DIR
    #[arg(long, env = "LUBOBAO_CONTROL", global = true, value_name = "PATH")]
    pub control: Option<PathBuf>,
}

/// 这些环境变量设为空值时视为没有设置（例如在 Docker 里用 `-e LUBOBAO_FFMPEG=` 关掉内置的
/// FFmpeg，改为自动下载）。
const PATH_ENV_VARS: [&str; 4] = [
    "LUBOBAO_DATA_DIR",
    "LUBOBAO_FFMPEG",
    "LUBOBAO_SAVE_DIR",
    "LUBOBAO_CONTROL",
];

fn clear_empty_env() {
    for name in PATH_ENV_VARS {
        if std::env::var_os(name).is_some_and(|value| value.is_empty()) {
            // SAFETY: 在 main 开头、启动任何线程之前调用。
            unsafe { std::env::remove_var(name) };
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// 前台常驻：监控直播间并自动录制，日志输出到终端（stderr）
    Run,
    /// 添加直播间
    Add {
        /// 直播间链接，例如 https://live.douyin.com/123456
        url: String,
        /// 画质：OD（原画）、UHD、HD、SD、LD；缺省用设置 record_quality
        #[arg(long, value_parser = parse_quality)]
        quality: Option<Quality>,
        /// 开播时只记录，不录制
        #[arg(long)]
        notify_only: bool,
        /// 备注
        #[arg(long)]
        note: Option<String>,
        /// 与已有任务重复时仍然添加
        #[arg(long)]
        force: bool,
    },
    /// 列出直播间
    List {
        /// 输出 JSON
        #[arg(long)]
        json: bool,
    },
    /// 暂停监控（正在录制的会停止）
    Pause {
        /// 任务 id，可写多个，或 all
        #[arg(required = true, num_args = 1..)]
        target: Vec<String>,
    },
    /// 恢复监控
    Resume {
        /// 任务 id，可写多个，或 all
        #[arg(required = true, num_args = 1..)]
        target: Vec<String>,
    },
    /// 删除直播间（正在录制的需要先 stop）
    Remove {
        /// 任务 id，可写多个
        #[arg(required = true, num_args = 1..)]
        ids: Vec<TaskId>,
    },
    /// 停止录制，本次直播不再自动录制（需要 run 正在运行）
    Stop {
        /// 任务 id，可写多个，或 all
        #[arg(required = true, num_args = 1..)]
        target: Vec<String>,
    },
    /// 查看运行状态
    Status {
        /// 输出 JSON
        #[arg(long)]
        json: bool,
    },
    /// 查看或修改设置（键名、默认值和校验规则与桌面端一致）
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// FFmpeg 组件
    Ffmpeg {
        #[command(subcommand)]
        action: FfmpegAction,
    },
    /// 让正在运行的 run 停止录制、收尾并退出（与 SIGTERM 相同）
    Shutdown,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// 列出全部设置
    List {
        #[arg(long)]
        json: bool,
    },
    /// 读取一项设置
    Get { key: String },
    /// 修改一项设置。字符串直接写；数字、布尔值和对象按 JSON 写，例如 `true`、`180`
    Set { key: String, value: String },
}

#[derive(Subcommand)]
enum FfmpegAction {
    /// 查看 FFmpeg 状态
    Status {
        #[arg(long)]
        json: bool,
    },
    /// 下载并安装 FFmpeg（已安装时重新检查）
    Install,
}

fn parse_quality(value: &str) -> Result<Quality, String> {
    let upper = value.trim().to_ascii_uppercase();
    Quality::ALL
        .into_iter()
        .find(|quality| quality.code() == upper || quality.label() == value.trim())
        .ok_or_else(|| format!("画质只能是 OD、UHD、HD、SD、LD 之一：{value}"))
}

fn parse_target(values: &[String]) -> Result<Target, String> {
    if values.len() == 1 && values[0].eq_ignore_ascii_case("all") {
        return Ok(Target::All);
    }
    values
        .iter()
        .map(|value| {
            value
                .parse::<TaskId>()
                .map_err(|_| format!("任务 id 应为数字或 all：{value}"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Target::Ids)
}

/// 录像目录按执行命令时的当前目录转成绝对路径再提交，`run` 的工作目录不影响结果。
fn absolute_save_path(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("录像目录不能为空".into());
    }
    std::path::absolute(trimmed)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| format!("录像目录无效 {trimmed}：{error}"))
}

/// 一个子命令要发出的请求和输出方式。
struct Invocation {
    request: Request,
    json: bool,
}

fn invocation(command: Command) -> Result<Invocation, String> {
    let plain = |request| Invocation {
        request,
        json: false,
    };
    Ok(match command {
        Command::Run => unreachable!("run is handled separately"),
        Command::Add {
            url,
            quality,
            notify_only,
            note,
            force,
        } => plain(Request::Add {
            url,
            quality,
            notify_only: notify_only.then_some(true),
            note,
            force,
        }),
        Command::List { json } => Invocation {
            request: Request::List,
            json,
        },
        Command::Pause { target } => plain(Request::SetMonitoring {
            target: parse_target(&target)?,
            enabled: false,
        }),
        Command::Resume { target } => plain(Request::SetMonitoring {
            target: parse_target(&target)?,
            enabled: true,
        }),
        Command::Remove { ids } => plain(Request::Remove { ids }),
        Command::Stop { target } => plain(Request::Stop {
            target: parse_target(&target)?,
        }),
        Command::Status { json } => Invocation {
            request: Request::Status,
            json,
        },
        Command::Config { action } => match action {
            ConfigAction::List { json } => Invocation {
                request: Request::ConfigList,
                json,
            },
            ConfigAction::Get { key } => plain(Request::ConfigGet { key }),
            ConfigAction::Set { key, value } => {
                let value = if key == ops::SAVE_PATH_KEY {
                    absolute_save_path(&value)?
                } else {
                    value
                };
                plain(Request::ConfigSet { key, value })
            }
        },
        Command::Ffmpeg { action } => match action {
            FfmpegAction::Status { json } => Invocation {
                request: Request::FfmpegStatus,
                json,
            },
            FfmpegAction::Install => plain(Request::FfmpegInstall),
        },
        Command::Shutdown => plain(Request::Shutdown),
    })
}

fn main() -> ExitCode {
    clear_empty_env();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("无法启动异步运行时：{error}");
            return ExitCode::FAILURE;
        }
    };
    let dirs = match paths::Dirs::resolve(&cli.global) {
        Ok(dirs) => dirs,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    if let Command::Run = cli.command {
        return match runtime.block_on(daemon::run(dirs, cli.global)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                logging::error(&message);
                ExitCode::FAILURE
            }
        };
    }
    let invocation = match invocation(cli.command) {
        Ok(invocation) => invocation,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let result = runtime.block_on(ops::dispatch(&dirs, &cli.global, &invocation.request));
    output::print(&invocation.request, invocation.json, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn targets_parse_ids_or_all() {
        assert_eq!(parse_target(&strings(&["all"])), Ok(Target::All));
        assert_eq!(parse_target(&strings(&["ALL"])), Ok(Target::All));
        assert_eq!(
            parse_target(&strings(&["3", "1"])),
            Ok(Target::Ids(vec![3, 1]))
        );
        assert!(parse_target(&strings(&["1", "all"])).is_err());
        assert!(parse_target(&strings(&["x"])).is_err());
    }

    #[test]
    fn qualities_accept_codes_and_labels() {
        assert_eq!(parse_quality("hd"), Ok(Quality::Hd));
        assert_eq!(parse_quality("原画"), Ok(Quality::Od));
        assert!(parse_quality("4k").is_err());
    }

    #[test]
    fn save_path_becomes_absolute_and_rejects_empty() {
        let value = absolute_save_path("videos").unwrap();
        assert!(std::path::Path::new(&value).is_absolute());
        assert!(value.ends_with("videos"));
        assert!(absolute_save_path("   ").is_err());
    }

    #[test]
    fn command_line_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
