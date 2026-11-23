//! 本地控制通道。每个连接一行 JSON 请求、一行 JSON 应答。
//!
//! - Unix：socket 文件（见 [`Dirs::socket_candidates`]），bind 后立即设为 0600；数据目录
//!   本身为 0700。
//! - Windows：命名管道，只允许当前用户访问，拒绝远程连接；客户端确认服务端进程属于同一个用户。

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

use crate::logging;
use crate::paths::Dirs;
use crate::protocol::{Failure, Request, Response};

/// 一行请求的上限。
const MAX_LINE: u64 = 1 << 20;
/// 服务端读取请求的时限：连上却不发请求的客户端不能一直占着连接。
const READ_TIMEOUT: Duration = Duration::from_secs(10);

pub enum SendError {
    /// 没有 `run` 在监听。
    NotRunning,
    Failed(String),
}

pub type Handler =
    Arc<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync>;

async fn serve_connection<S>(stream: S, handler: Handler)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read).take(MAX_LINE);
    let mut line = String::new();
    let response = match tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(_)) => match serde_json::from_str::<Request>(line.trim()) {
            Ok(request) => handler(request).await,
            Err(error) => Response::Err(Failure::new("protocol", format!("请求无法解析：{error}"))),
        },
        Ok(Err(error)) => Response::Err(Failure::new("protocol", format!("读取请求失败：{error}"))),
        Err(_) => return,
    };
    let mut text = serde_json::to_string(&response).unwrap_or_else(|_| "null".into());
    text.push('\n');
    let _ = write.write_all(text.as_bytes()).await;
    let _ = write.shutdown().await;
}

async fn exchange<S>(stream: S, request: &Request, limit: Duration) -> Result<Response, SendError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let failed = |error: io::Error| SendError::Failed(format!("控制通道通信失败：{error}"));
    let (read, mut write) = tokio::io::split(stream);
    let mut text = serde_json::to_string(request).map_err(|e| SendError::Failed(e.to_string()))?;
    text.push('\n');
    let work = async {
        write.write_all(text.as_bytes()).await.map_err(failed)?;
        write.flush().await.map_err(failed)?;
        let mut line = String::new();
        BufReader::new(read)
            .take(MAX_LINE * 64)
            .read_line(&mut line)
            .await
            .map_err(failed)?;
        serde_json::from_str(line.trim())
            .map_err(|error| SendError::Failed(format!("run 的应答无法解析：{error}")))
    };
    tokio::time::timeout(limit, work).await.unwrap_or_else(|_| {
        Err(SendError::Failed(format!(
            "等待 run 应答超过 {} 秒",
            limit.as_secs()
        )))
    })
}

/// 正在监听的控制通道。drop 时停止监听（Unix 上删除 socket 文件）。
pub struct Server {
    task: JoinHandle<()>,
    /// 实际使用的地址，用于日志。
    pub endpoint: String,
    #[cfg(unix)]
    path: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn bind_socket(path: &std::path::Path) -> io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    // 调用方持有实例锁，留下的 socket 文件只能是上次异常退出的残留。
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    // bind 后立即收紧；数据目录为 0700，其间别的用户也进不来。
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(listener)
}

#[cfg(unix)]
pub fn listen(dirs: &Dirs, handler: Handler) -> io::Result<Server> {
    let mut last_error = None;
    let mut bound = None;
    for path in dirs.socket_candidates() {
        match bind_socket(&path) {
            Ok(listener) => {
                bound = Some((listener, path));
                break;
            }
            Err(error) => {
                // 网络文件系统等不支持 socket 时换下一个候选。
                logging::warn(&format!("无法在 {} 打开控制通道：{error}", path.display()));
                last_error = Some(error);
            }
        }
    }
    let Some((listener, path)) = bound else {
        return Err(last_error.unwrap_or_else(|| io::Error::other("没有可用的控制通道路径")));
    };
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve_connection(stream, Arc::clone(&handler)));
                }
                Err(error) => {
                    logging::warn(&format!("控制通道接受连接失败：{error}"));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
    Ok(Server {
        task,
        endpoint: path.display().to_string(),
        path,
    })
}

#[cfg(unix)]
pub async fn send(dirs: &Dirs, request: &Request, limit: Duration) -> Result<Response, SendError> {
    for path in dirs.socket_candidates() {
        match tokio::net::UnixStream::connect(&path).await {
            Ok(stream) => return exchange(stream, request, limit).await,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => {
                return Err(SendError::Failed(format!(
                    "无法连接控制通道 {}：{error}",
                    path.display()
                )));
            }
        }
    }
    Err(SendError::NotRunning)
}

#[cfg(windows)]
pub fn listen(dirs: &Dirs, handler: Handler) -> io::Result<Server> {
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    use crate::winsec::PipeSecurity;

    let name = dirs.pipe_name();
    let security = Arc::new(PipeSecurity::current_user_only()?);
    let create = {
        let name = name.clone();
        let security = Arc::clone(&security);
        move |first: bool| -> io::Result<NamedPipeServer> {
            let mut options = ServerOptions::new();
            // first_pipe_instance：同名管道已被别的进程创建时失败，防止被抢占。
            options
                .first_pipe_instance(first)
                .reject_remote_clients(true);
            // SAFETY: 安全属性在 `security` 里，生命周期覆盖所有管道实例的创建。
            unsafe { options.create_with_security_attributes_raw(&name, security.as_ptr()) }
        }
    };
    let mut server = create(true)?;
    let task = tokio::spawn(async move {
        loop {
            if let Err(error) = server.connect().await {
                logging::warn(&format!("控制通道接受连接失败：{error}"));
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
            let next = match create(false) {
                Ok(next) => next,
                Err(error) => {
                    logging::error(&format!("控制通道无法继续监听：{error}"));
                    return;
                }
            };
            let connected = std::mem::replace(&mut server, next);
            tokio::spawn(serve_connection(connected, Arc::clone(&handler)));
        }
    });
    Ok(Server {
        task,
        endpoint: name,
    })
}

#[cfg(windows)]
pub async fn send(dirs: &Dirs, request: &Request, limit: Duration) -> Result<Response, SendError> {
    use tokio::net::windows::named_pipe::ClientOptions;

    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;

    let name = dirs.pipe_name();
    let mut attempts = 0;
    loop {
        match ClientOptions::new().open(&name) {
            Ok(client) => {
                match crate::winsec::server_is_same_user(&client) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(SendError::Failed(format!(
                            "控制通道 {name} 属于另一个用户，已拒绝连接"
                        )));
                    }
                    Err(error) => {
                        return Err(SendError::Failed(format!(
                            "无法确认控制通道 {name} 的所有者：{error}"
                        )));
                    }
                }
                return exchange(client, request, limit).await;
            }
            Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {
                return Err(SendError::NotRunning);
            }
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 50 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(SendError::Failed(format!(
                    "无法连接控制通道 {name}：{error}"
                )));
            }
        }
    }
}
