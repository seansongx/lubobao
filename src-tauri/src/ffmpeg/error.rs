//! 安装失败的原因，`Display` 输出给用户看的中文说明。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// 所有下载源都没有传回数据。
    SourcesUnreachable { sources: usize, last_error: String },
    /// 下载了一部分后中断，已保留部分文件供续传。
    Interrupted {
        downloaded: u64,
        total: u64,
        last_error: String,
    },
    /// 下载的 tarball 大小或 SHA-256 不一致。
    TarballMismatch,
    /// tarball 中没有清单指定的可执行文件。
    BinaryMissing(String),
    /// 解出的可执行文件大小或 SHA-256 不一致。
    BinaryMismatch,
    /// 校验通过的可执行文件运行 `-version` 失败。
    ProbeFailed(String),
    /// 读写本地文件失败。
    Io { action: String, detail: String },
}

impl InstallError {
    pub fn io(action: &str, err: impl fmt::Display) -> Self {
        Self::Io {
            action: action.to_string(),
            detail: err.to_string(),
        }
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourcesUnreachable {
                sources,
                last_error,
            } => {
                let which = match sources {
                    1 => "下载源".to_string(),
                    2 => "两个下载源都".to_string(),
                    n => format!("{n} 个下载源都"),
                };
                write!(f, "{which}无法连接，请检查网络后重试（{last_error}）")
            }
            Self::Interrupted {
                downloaded,
                total,
                last_error,
            } => write!(
                f,
                "下载中断（已下载 {} / {}），重试时会从断点继续（{last_error}）",
                mib(*downloaded),
                mib(*total)
            ),
            Self::TarballMismatch => f.write_str("下载的文件校验不一致，已删除，请重试"),
            Self::BinaryMissing(path) => write!(f, "安装包中没有找到 {path}"),
            Self::BinaryMismatch => f.write_str("FFmpeg 可执行文件校验不一致，未安装"),
            Self::ProbeFailed(detail) => write!(f, "FFmpeg 无法正常运行，未安装（{detail}）"),
            Self::Io { action, detail } => write!(f, "{action}失败：{detail}"),
        }
    }
}

impl std::error::Error for InstallError {}
