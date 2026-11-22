//! 校验、解压、试运行和校验缓存。这里的函数都是阻塞的，在 `spawn_blocking` 中调用。

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::InstallError;
use super::manifest::PlatformPackage;

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn same_hash(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected.trim())
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// tarball 的大小和 SHA-256 都与清单一致才算通过。
pub fn verify_tarball(path: &Path, package: &PlatformPackage) -> Result<(), InstallError> {
    let size = fs::metadata(path)
        .map_err(|err| InstallError::io("读取下载文件", err))?
        .len();
    if size != package.size {
        return Err(InstallError::TarballMismatch);
    }
    let hash = sha256_file(path).map_err(|err| InstallError::io("读取下载文件", err))?;
    if !same_hash(&hash, &package.sha256) {
        return Err(InstallError::TarballMismatch);
    }
    Ok(())
}

/// 写入时同时计算 SHA-256 和字节数。
struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// 只从 gzip + tar 包中取出 `package/<binary>`，写到 `dest`，并核对大小和 `binarySha256`。
/// 失败时删除 `dest`。
pub fn extract_binary(
    tarball: &Path,
    package: &PlatformPackage,
    dest: &Path,
) -> Result<(), InstallError> {
    let result = extract_binary_inner(tarball, package, dest);
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

fn extract_binary_inner(
    tarball: &Path,
    package: &PlatformPackage,
    dest: &Path,
) -> Result<(), InstallError> {
    let wanted = package.entry_path();
    let file = File::open(tarball).map_err(|err| InstallError::io("打开安装包", err))?;
    let mut archive = tar::Archive::new(GzDecoder::new(io::BufReader::new(file)));
    let entries = archive
        .entries()
        .map_err(|err| InstallError::io("解压安装包", err))?;

    for entry in entries {
        let mut entry = entry.map_err(|err| InstallError::io("解压安装包", err))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|err| InstallError::io("解压安装包", err))?
            .to_string_lossy()
            .replace('\\', "/");
        if path != wanted {
            continue;
        }

        let out = File::create(dest).map_err(|err| InstallError::io("写入临时文件", err))?;
        let mut writer = HashingWriter {
            inner: io::BufWriter::new(out),
            hasher: Sha256::new(),
            written: 0,
        };
        // 多读一个字节即可发现超出预期大小的条目，不必整块读完。
        let mut limited = (&mut entry).take(package.binary_size + 1);
        io::copy(&mut limited, &mut writer).map_err(|err| InstallError::io("解压安装包", err))?;
        writer
            .flush()
            .map_err(|err| InstallError::io("写入临时文件", err))?;
        let HashingWriter {
            inner,
            hasher,
            written,
        } = writer;
        let out = inner
            .into_inner()
            .map_err(|err| InstallError::io("写入临时文件", err.into_error()))?;
        out.sync_all()
            .map_err(|err| InstallError::io("写入临时文件", err))?;
        drop(out);

        if written != package.binary_size
            || !same_hash(&to_hex(&hasher.finalize()), &package.binary_sha256)
        {
            return Err(InstallError::BinaryMismatch);
        }
        set_executable(dest)?;
        return Ok(());
    }

    Err(InstallError::BinaryMissing(wanted))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|err| InstallError::io("设置执行权限", err))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), InstallError> {
    Ok(())
}

/// 运行 `<path> -version`，返回输出的第一行。
pub fn run_version(path: &Path) -> Result<String, String> {
    let mut command = Command::new(path);
    command
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command.spawn().map_err(|err| format!("无法启动：{err}"))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("运行超时".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => return Err(format!("无法等待进程：{err}")),
        }
    };
    let output = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("退出状态 {status}"));
    }
    let first = output.lines().next().unwrap_or("").trim();
    if !first.starts_with("ffmpeg version") {
        return Err("输出不是 FFmpeg 版本信息".into());
    }
    Ok(first.to_string())
}

/// 已校验文件的记录。文件的大小或修改时间变了就重新计算哈希。
/// 版本号用于在当前版本安装失败时找回旧版本。
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct VerifiedRecord {
    size: u64,
    modified_nanos: u64,
    sha256: String,
    #[serde(default)]
    ffmpeg_version: Option<String>,
    #[serde(default)]
    package_version: Option<String>,
}

/// 记录中的版本号。
#[derive(Debug, Clone, Copy)]
pub struct Versions<'a> {
    pub ffmpeg: &'a str,
    pub package: &'a str,
}

fn modified_nanos(meta: &fs::Metadata) -> Option<u64> {
    let modified = meta.modified().ok()?;
    let nanos = modified.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    u64::try_from(nanos).ok()
}

fn read_record(cache: &Path) -> Option<VerifiedRecord> {
    let text = fs::read_to_string(cache).ok()?;
    serde_json::from_str(&text).ok()
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// 记录 `binary` 已通过校验。写入失败只会让下次启动重新计算哈希。
pub fn write_record(binary: &Path, cache: &Path, sha256: &str, versions: Versions<'_>) {
    let Ok(meta) = fs::metadata(binary) else {
        return;
    };
    let Some(modified_nanos) = modified_nanos(&meta) else {
        return;
    };
    let record = VerifiedRecord {
        size: meta.len(),
        modified_nanos,
        sha256: sha256.to_ascii_lowercase(),
        ffmpeg_version: Some(versions.ffmpeg.to_string()),
        package_version: Some(versions.package.to_string()),
    };
    if let Ok(text) = serde_json::to_string(&record) {
        // 多个进程可能同时写，临时文件名要各不相同。
        let tmp = cache.with_extension(format!("json.{}.tmp", unique_suffix()));
        if fs::write(&tmp, text).is_err() || fs::rename(&tmp, cache).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

/// 已安装的文件大小和 `binarySha256` 都一致时返回 `true`。不联网。
pub fn check_installed(
    binary: &Path,
    cache: &Path,
    package: &PlatformPackage,
    versions: Versions<'_>,
) -> bool {
    let Ok(meta) = fs::metadata(binary) else {
        return false;
    };
    if !meta.is_file() || meta.len() != package.binary_size {
        return false;
    }
    if let (Some(record), Some(modified)) = (read_record(cache), modified_nanos(&meta))
        && record.size == meta.len()
        && record.modified_nanos == modified
        && same_hash(&record.sha256, &package.binary_sha256)
    {
        return true;
    }
    match sha256_file(binary) {
        Ok(hash) if same_hash(&hash, &package.binary_sha256) => {
            write_record(binary, cache, &package.binary_sha256, versions);
            true
        }
        _ => {
            let _ = fs::remove_file(cache);
            false
        }
    }
}

/// 通过校验的旧版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledVersion {
    pub path: PathBuf,
    pub ffmpeg_version: String,
    pub package_version: String,
}

/// 目录名 `9.0.2-9.0.1` 按数字比较的排序键。
fn version_key(name: &str) -> Vec<u64> {
    name.split(['-', '.'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// 在 `root` 下找最新的旧版本：二进制与它的 `verified.json` 一致（重新计算哈希）。
/// 跳过 `skip_dir`（当前版本）和以 `.` 开头的目录。
pub fn find_fallback(
    root: &Path,
    skip_dir: &str,
    binary_name: &str,
    record_name: &str,
) -> Option<InstalledVersion> {
    let mut dirs: Vec<(Vec<u64>, String, PathBuf)> = fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name != skip_dir && !name.starts_with('.'))
                .then(|| (version_key(&name), name, entry.path()))
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    dirs.into_iter().find_map(|(_, name, dir)| {
        let binary = dir.join(binary_name);
        let record = read_record(&dir.join(record_name))?;
        let meta = fs::metadata(&binary).ok()?;
        if !meta.is_file() || meta.len() != record.size {
            return None;
        }
        let hash = sha256_file(&binary).ok()?;
        if !same_hash(&hash, &record.sha256) {
            return None;
        }
        let (dir_ffmpeg, dir_package) = name.split_once('-').unwrap_or((&name, ""));
        Some(InstalledVersion {
            path: binary,
            ffmpeg_version: record
                .ffmpeg_version
                .unwrap_or_else(|| dir_ffmpeg.to_string()),
            package_version: record
                .package_version
                .unwrap_or_else(|| dir_package.to_string()),
        })
    })
}

#[cfg(test)]
pub fn sha256_bytes(bytes: &[u8]) -> String {
    to_hex(&Sha256::digest(bytes))
}
