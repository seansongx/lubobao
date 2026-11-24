//! 固定的 yt-dlp 和 Deno 版本、下载地址和 SHA-256。
//!
//! 都来自官方 GitHub Release：yt-dlp 的 `SHA2-256SUMS`，Deno 每个压缩包旁的
//! `.sha256sum`。更新时改这里的版本、大小和哈希，随录播宝发布（见模块文档里的更新办法）。

/// 下载的文件怎么取出可执行文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    /// 下载的就是可执行文件。
    Raw,
    /// zip 包里的某个文件。
    Zip { entry: &'static str },
}

/// 一个组件在当前平台的下载包。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    /// 显示名，例如 `yt-dlp`。
    pub name: &'static str,
    /// 安装目录名。
    pub dir: &'static str,
    pub version: &'static str,
    pub url: String,
    /// 下载文件的大小和 SHA-256。
    pub size: u64,
    pub sha256: &'static str,
    pub archive: Archive,
    /// 安装后的可执行文件名。
    pub binary: &'static str,
}

impl Tool {
    /// 下载文件名（URL 最后一段）。
    pub fn asset(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(self.name)
    }
}

/// YouTube 需要的两个组件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolset {
    pub ytdlp: Tool,
    pub deno: Tool,
}

impl Toolset {
    pub fn tools(&self) -> [&Tool; 2] {
        [&self.ytdlp, &self.deno]
    }
}

pub const YTDLP_VERSION: &str = "2026.11.23";
pub const DENO_VERSION: &str = "2.9.8";

fn ytdlp(asset: &str, size: u64, sha256: &'static str, binary: &'static str) -> Tool {
    Tool {
        name: "yt-dlp",
        dir: "yt-dlp",
        version: YTDLP_VERSION,
        url: format!("https://github.com/yt-dlp/yt-dlp/releases/download/{YTDLP_VERSION}/{asset}"),
        size,
        sha256,
        archive: Archive::Raw,
        binary,
    }
}

fn deno(target: &str, size: u64, sha256: &'static str, binary: &'static str) -> Tool {
    Tool {
        name: "Deno",
        dir: "deno",
        version: DENO_VERSION,
        url: format!(
            "https://github.com/denoland/deno/releases/download/v{DENO_VERSION}/deno-{target}.zip"
        ),
        size,
        sha256,
        archive: Archive::Zip { entry: binary },
        binary,
    }
}

/// 指定系统和架构（`std::env::consts` 的写法）的组件。
pub fn pinned(os: &str, arch: &str) -> Result<Toolset, String> {
    let set = match (os, arch) {
        ("windows", "x86_64") => Toolset {
            ytdlp: ytdlp(
                "yt-dlp.exe",
                23_627_865,
                "622ed48023cac1daa52a3bf3a6ac38f03514de214b5f0fa069389fd0f6d17e5c",
                "yt-dlp.exe",
            ),
            deno: deno(
                "x86_64-pc-windows-msvc",
                42_917_472,
                "cba84b26e0df5ae7f74fc66b86a95b866872a86292ebd84fa1e959927edd93bf",
                "deno.exe",
            ),
        },
        ("macos", "x86_64") => Toolset {
            ytdlp: ytdlp(
                "yt-dlp_macos",
                37_478_576,
                "57654854886d0398a798895aadf5173e2749fbb5e249b9f3728022ab376e11fb",
                "yt-dlp",
            ),
            deno: deno(
                "x86_64-apple-darwin",
                42_797_031,
                "4b2f5edd2639b7470f034238b321d7002d8dfb4726d8558f52c17afac370eb2c",
                "deno",
            ),
        },
        ("macos", "aarch64") => Toolset {
            ytdlp: ytdlp(
                "yt-dlp_macos",
                37_478_576,
                "57654854886d0398a798895aadf5173e2749fbb5e249b9f3728022ab376e11fb",
                "yt-dlp",
            ),
            deno: deno(
                "aarch64-apple-darwin",
                38_770_795,
                "9f0bf660393da2eaff044cdc7444cb08c73f235849b61d224d825a194ce7c98e",
                "deno",
            ),
        },
        ("linux", "x86_64") => Toolset {
            ytdlp: ytdlp(
                "yt-dlp_linux",
                41_184_064,
                "249ddb058234276bc449b9cd9492aad9f133bda5923c61a9e39983ae114ef17d",
                "yt-dlp",
            ),
            deno: deno(
                "x86_64-unknown-linux-gnu",
                41_963_064,
                "e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609",
                "deno",
            ),
        },
        ("linux", "aarch64") => Toolset {
            ytdlp: ytdlp(
                "yt-dlp_linux_aarch64",
                40_807_480,
                "fb847a2e4a9ddc8cf5fcfb4bb7f42ba8969ecf72068def0278074f9d300bd13c",
                "yt-dlp",
            ),
            deno: deno(
                "aarch64-unknown-linux-gnu",
                40_120_596,
                "d49596c20b9788cf8f5d8ddf644103d4bde56ed239753f0bdb92ba9361d51905",
                "deno",
            ),
        },
        _ => {
            return Err(format!("YouTube 组件（yt-dlp 和 Deno）不支持 {os} {arch}"));
        }
    };
    Ok(set)
}

/// 当前编译目标的组件。
pub fn current() -> Result<Toolset, String> {
    pinned(std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_target_has_both_tools_with_hashes() {
        for (os, arch) in [
            ("windows", "x86_64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
        ] {
            let set = pinned(os, arch).unwrap();
            for tool in set.tools() {
                assert_eq!(tool.sha256.len(), 64, "{os} {arch} {}", tool.name);
                assert!(tool.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
                assert!(tool.size > 10_000_000);
                assert!(tool.url.starts_with("https://github.com/"));
            }
            assert_eq!(
                set.deno.archive,
                Archive::Zip {
                    entry: set.deno.binary
                }
            );
        }
        assert_eq!(
            pinned("windows", "x86_64").unwrap().deno.asset(),
            "deno-x86_64-pc-windows-msvc.zip"
        );
        assert!(pinned("windows", "aarch64").is_err());
        assert!(pinned("freebsd", "x86_64").is_err());
    }
}
