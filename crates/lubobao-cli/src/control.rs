//! 本地控制通道：Unix 上是数据目录里的 `control.sock`（权限 0600），Windows 上是按数据目录
//! 命名的命名管道（拒绝远程连接）。每个连接一行 JSON 请求、一行 JSON 应答。

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

use crate::logging;
use crate::paths::Dirs;
use crate::protocol::{Failure, Request, Response};

/// 一行请求的上限。
const MAX_LINE: u64 = 1 << 20;

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
    let response = match reader.read_line(&mut line).await {
        Ok(_) => match serde_json::from_str::<Request>(line.trim()) {
            Ok(request) => handler(request).await,
            Err(error) => Response::Err(Failure::new("protocol", format!("请求无法解析：{error}"))),
        },
        Err(error) => Response::Err(Failure::new("protocol", format!("读取请求失败：{error}"))),
    };
    let mut text = serde_json::to_string(&response).unwrap_or_else(|_| "null".into());
    text.push('\n');
    let _ = write.write_all(text.as_bytes()).await;
    let _ = write.shutdown().await;
}

async fn exchange<S>(stream: S, request: &Request) -> Result<Response, SendError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let failed = |error: io::Error| SendError::Failed(format!("控制通道通信失败：{error}"));
    let (read, mut write) = tokio::io::split(stream);
    let mut text = serde_json::to_string(request).map_err(|e| SendError::Failed(e.to_string()))?;
    text.push('\n');
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
}

/// 正在监听的控制通道。drop 时停止监听（Unix 上删除 socket 文件）。
pub struct Server {
    task: JoinHandle<()>,
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
pub fn listen(dirs: &Dirs, handler: Handler) -> io::Result<Server> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let path = dirs.socket_path();
    // 调用方持有实例锁，留下的 socket 文件只能是上次异常退出的残留。
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // 创建时就只有本用户可读写，之后再显式设一次 0600。
    // SAFETY: umask 只修改进程的文件创建掩码，随即恢复。
    let previous = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(&path);
    unsafe { libc::umask(previous) };
    let listener = bound?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve_connection(stream, Arc::clone(&handler)));
                }
                Err(error) => {
                    logging::warn(&format!("控制通道接受连接失败：{error}"));
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    });
    Ok(Server { task, path })
}

#[cfg(unix)]
pub async fn send(dirs: &Dirs, request: &Request) -> Result<Response, SendError> {
    let path = dirs.socket_path();
    match tokio::net::UnixStream::connect(&path).await {
        Ok(stream) => exchange(stream, request).await,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(SendError::NotRunning)
        }
        Err(error) => Err(SendError::Failed(format!(
            "无法连接控制通道 {}：{error}",
            path.display()
        ))),
    }
}

#[cfg(windows)]
pub fn listen(dirs: &Dirs, handler: Handler) -> io::Result<Server> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let name = dirs.pipe_name();
    // first_pipe_instance：同名管道已被别的进程创建时失败，防止被抢占。
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(&name)?;
    let task = tokio::spawn(async move {
        loop {
            if let Err(error) = server.connect().await {
                logging::warn(&format!("控制通道接受连接失败：{error}"));
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
            let next = match ServerOptions::new()
                .reject_remote_clients(true)
                .create(&name)
            {
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
    Ok(Server { task })
}

#[cfg(windows)]
pub async fn send(dirs: &Dirs, request: &Request) -> Result<Response, SendError> {
    use tokio::net::windows::named_pipe::ClientOptions;

    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;

    let name = dirs.pipe_name();
    let mut attempts = 0;
    loop {
        match ClientOptions::new().open(&name) {
            Ok(client) => return exchange(client, request).await,
            Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {
                return Err(SendError::NotRunning);
            }
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 50 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(SendError::Failed(format!(
                    "无法连接控制通道 {name}：{error}"
                )));
            }
        }
    }
}
