//! 固定版本的 `@seankit/ffmpeg` 清单，以及按编译目标选出平台包。

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// `@seankit/ffmpeg@9.0.1` 主包中的 `manifest.json`，原样嵌入。
const PINNED_MANIFEST: &str = include_str!("manifest.json");

/// 构建脚本写入的 Rust 编译目标，例如 `x86_64-pc-windows-msvc`。
const BUILD_TARGET: &str = env!("LUBOBAO_TARGET");

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub ffmpeg_version: String,
    pub package_version: String,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    pub platforms: HashMap<String, PlatformPackage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformPackage {
    pub file: String,
    pub size: u64,
    pub sha256: String,
    /// 可执行文件在包内的路径，tarball 中位于 `package/<binary>`。
    pub binary: String,
    pub binary_sha256: String,
    pub binary_size: u64,
    #[serde(default)]
    pub rust_targets: Vec<String>,
    pub urls: Vec<String>,
}

impl PlatformPackage {
    /// tarball 中可执行文件的完整路径。
    pub fn entry_path(&self) -> String {
        format!("package/{}", self.binary.trim_start_matches('/'))
    }

    /// 安装后的文件名，例如 `ffmpeg.exe`。
    pub fn binary_file_name(&self) -> String {
        Path::new(&self.binary)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.binary.clone())
    }
}

/// 当前平台要安装的组件：版本号加上选中的平台包。
#[derive(Debug, Clone)]
pub struct Component {
    pub ffmpeg_version: String,
    pub package_version: String,
    pub platform: String,
    pub package: PlatformPackage,
}

impl Component {
    /// 按版本分开的安装目录名，例如 `9.0.2-9.0.1`。
    pub fn version_dir(&self) -> String {
        format!("{}-{}", self.ffmpeg_version, self.package_version)
    }
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|err| format!("FFmpeg 组件清单无法解析：{err}"))
    }

    pub fn pinned() -> Self {
        Self::parse(PINNED_MANIFEST).expect("embedded FFmpeg manifest must be valid")
    }

    /// 先按 `rustTargets` 匹配编译目标，再按 Node 风格的 `<os>-<arch>` 和 `aliases` 匹配。
    pub fn select(&self, target: &str, os: &str, arch: &str) -> Result<Component, String> {
        let by_target = self
            .platforms
            .iter()
            .filter(|(_, package)| package.rust_targets.iter().any(|t| t == target))
            .min_by(|a, b| a.0.cmp(b.0));
        if let Some((key, package)) = by_target {
            return Ok(self.component(key, package));
        }

        if let (Some(node_os), Some(node_arch)) = (node_os(os), node_arch(arch)) {
            let key = format!("{node_os}-{node_arch}");
            let resolved = self.aliases.get(&key).cloned().unwrap_or(key);
            if let Some(package) = self.platforms.get(&resolved) {
                return Ok(self.component(&resolved, package));
            }
        }

        Err(format!(
            "当前平台（{target}）没有可用的 FFmpeg 组件，暂不支持自动安装"
        ))
    }

    pub fn select_current(&self) -> Result<Component, String> {
        self.select(BUILD_TARGET, std::env::consts::OS, std::env::consts::ARCH)
    }

    fn component(&self, key: &str, package: &PlatformPackage) -> Component {
        Component {
            ffmpeg_version: self.ffmpeg_version.clone(),
            package_version: self.package_version.clone(),
            platform: key.to_string(),
            package: package.clone(),
        }
    }
}

fn node_os(os: &str) -> Option<&'static str> {
    match os {
        "windows" => Some("win32"),
        "macos" => Some("darwin"),
        "linux" => Some("linux"),
        _ => None,
    }
}

fn node_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("x64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_manifest_is_version_9_0_2_package_9_0_1() {
        let manifest = Manifest::pinned();
        assert_eq!(manifest.ffmpeg_version, "9.0.2");
        assert_eq!(manifest.package_version, "9.0.1");
        let win = &manifest.platforms["win32-x64"];
        assert_eq!(win.entry_path(), "package/bin/ffmpeg.exe");
        assert_eq!(win.binary_file_name(), "ffmpeg.exe");
        assert!(win.urls[0].starts_with("https://registry.npmmirror.com/"));
        assert!(win.urls[1].starts_with("https://registry.npmjs.org/"));
    }

    #[test]
    fn selects_platform_by_rust_target() {
        let manifest = Manifest::pinned();
        let component = manifest
            .select("x86_64-unknown-linux-gnu", "none", "none")
            .unwrap();
        assert_eq!(component.platform, "linux-x64");
        assert_eq!(component.version_dir(), "9.0.2-9.0.1");
        let component = manifest
            .select("aarch64-pc-windows-msvc", "windows", "aarch64")
            .unwrap();
        assert_eq!(component.platform, "win32-x64");
    }

    #[test]
    fn selects_platform_by_os_arch_alias() {
        let manifest = Manifest::pinned();
        let component = manifest
            .select("aarch64-pc-windows-gnullvm", "windows", "aarch64")
            .unwrap();
        assert_eq!(component.platform, "win32-x64");
        let component = manifest
            .select("x86_64-unknown-linux-musl", "linux", "x86_64")
            .unwrap();
        assert_eq!(component.platform, "linux-x64");
    }

    #[test]
    fn unsupported_platform_returns_clear_error() {
        let manifest = Manifest::pinned();
        let err = manifest
            .select("riscv64gc-unknown-linux-gnu", "linux", "riscv64")
            .unwrap_err();
        assert!(err.contains("riscv64gc-unknown-linux-gnu"), "{err}");
        assert!(err.contains("不支持"), "{err}");
    }

    #[test]
    fn current_build_target_is_supported_on_dev_machines() {
        if matches!(
            BUILD_TARGET,
            "x86_64-pc-windows-msvc"
                | "aarch64-pc-windows-msvc"
                | "x86_64-apple-darwin"
                | "aarch64-apple-darwin"
                | "x86_64-unknown-linux-gnu"
                | "aarch64-unknown-linux-gnu"
        ) {
            Manifest::pinned().select_current().unwrap();
        }
    }
}
