//! 解析 ffmpeg stderr：统计行（进度）和错误行。
//!
//! 开启 `-stats` 后，ffmpeg 每秒向 stderr 写一行以 `\r` 结尾的统计信息，例如
//! `frame=  126 fps= 37 q=-1.0 size=     119KiB time=00:00:04.99 bitrate= 195.2kbits/s speed=1.45x elapsed=0:00:03.44`。
//! 最后一行把 `size=` 写作 `Lsize=`，以 `\n` 结尾。

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// 一次进度快照。字段在 ffmpeg 输出 `N/A` 时为 `None`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    /// 已写入的字节数。分段录制时 ffmpeg 报告 `N/A`，引擎改用磁盘上分段文件的总大小。
    pub size_bytes: Option<u64>,
    /// 已录制的媒体时长（秒）。
    pub duration_secs: Option<f64>,
    /// 平均码率（kbit/s）。分段录制时由文件大小和时长推算。
    pub bitrate_kbps: Option<f64>,
    /// 处理速度，直播录制时约为 1.0。
    pub speed: Option<f64>,
}

impl Progress {
    /// 是否已经收到媒体数据。
    pub fn has_media(&self) -> bool {
        self.duration_secs.is_some_and(|secs| secs > 0.0) || self.size_bytes.is_some_and(|b| b > 0)
    }
}

/// 用已知的文件总大小补全分段录制时缺失的大小和码率。
pub fn fill_from_files(progress: &mut Progress, bytes_on_disk: u64) {
    if progress.size_bytes.is_none() && bytes_on_disk > 0 {
        progress.size_bytes = Some(bytes_on_disk);
    }
    if progress.bitrate_kbps.is_none()
        && let (Some(bytes), Some(secs)) = (progress.size_bytes, progress.duration_secs)
        && bytes > 0
        && secs > 0.0
    {
        progress.bitrate_kbps = Some((bytes as f64 * 8.0 / secs / 100.0).round() / 10.0);
    }
}

/// 解析一行统计信息；不是统计行时返回 `None`。
pub fn parse_progress_line(line: &str) -> Option<Progress> {
    let time = field(line, "time")?;
    let size = field(line, "size").or_else(|| field(line, "Lsize"))?;
    Some(Progress {
        size_bytes: parse_size(size),
        duration_secs: parse_time(time),
        bitrate_kbps: field(line, "bitrate").and_then(parse_bitrate),
        speed: field(line, "speed").and_then(|value| parse_number(value.strip_suffix('x')?)),
    })
}

/// 取 `key=value` 中的值。值前可以有空格；`key` 必须位于行首或空格之后。
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("{key}=");
    let mut search_from = 0;
    while let Some(found) = line[search_from..].find(&pattern) {
        let start = search_from + found;
        let at_boundary = start == 0 || line[..start].ends_with(' ');
        if at_boundary {
            let rest = line[start + pattern.len()..].trim_start();
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let value = &rest[..end];
            return (!value.is_empty()).then_some(value);
        }
        search_from = start + pattern.len();
    }
    None
}

fn parse_number(value: &str) -> Option<f64> {
    let number: f64 = value.parse().ok()?;
    number.is_finite().then_some(number)
}

/// `119KiB`、`1.5MiB`、`512kB`、`0B`。ffmpeg 的 kB 也是 1024 字节。
fn parse_size(value: &str) -> Option<u64> {
    let split = value
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number = parse_number(number)?;
    let multiplier = match unit {
        "" | "B" => 1.0,
        "kB" | "KiB" | "KB" => 1024.0,
        "mB" | "MiB" | "MB" => 1024.0 * 1024.0,
        "gB" | "GiB" | "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * multiplier).round() as u64)
}

/// `HH:MM:SS.ss`，可能带负号（开头几帧的时间戳），负值按 0 处理。
fn parse_time(value: &str) -> Option<f64> {
    let (negative, value) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let mut parts = value.split(':');
    let hours = parse_number(parts.next()?)?;
    let minutes = parse_number(parts.next()?)?;
    let seconds = parse_number(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let total = hours * 3600.0 + minutes * 60.0 + seconds;
    Some(if negative { 0.0 } else { total })
}

/// `195.2kbits/s`。
fn parse_bitrate(value: &str) -> Option<f64> {
    if let Some(number) = value.strip_suffix("kbits/s") {
        return parse_number(number);
    }
    if let Some(number) = value.strip_suffix("Mbits/s") {
        return parse_number(number).map(|mbps| mbps * 1000.0);
    }
    value
        .strip_suffix("bits/s")
        .and_then(parse_number)
        .map(|bps| bps / 1000.0)
}

/// 把 stderr 字节流按 `\r` 或 `\n` 切成行，跳过空行。
#[derive(Debug, Default)]
pub struct LineSplitter {
    pending: Vec<u8>,
}

impl LineSplitter {
    /// 追加一段字节，返回其中完整的行。
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in bytes {
            if byte == b'\r' || byte == b'\n' {
                self.take_line(&mut lines);
            } else {
                self.pending.push(byte);
            }
        }
        // 防止没有换行的异常输出无限增长。
        if self.pending.len() > 64 * 1024 {
            self.take_line(&mut lines);
        }
        lines
    }

    /// 流结束时取出最后一行。
    pub fn finish(&mut self) -> Option<String> {
        let mut lines = Vec::new();
        self.take_line(&mut lines);
        lines.pop()
    }

    fn take_line(&mut self, lines: &mut Vec<String>) {
        if self.pending.is_empty() {
            return;
        }
        let line = String::from_utf8_lossy(&self.pending).trim().to_string();
        self.pending.clear();
        if !line.is_empty() {
            lines.push(line);
        }
    }
}

/// 保留最近若干行错误输出，用于结束原因判断和界面提示。
#[derive(Debug, Clone)]
pub struct ErrorTail {
    lines: VecDeque<String>,
    capacity: usize,
}

impl ErrorTail {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, line: String) {
        // 「Last message repeated N times」没有信息量。
        if line.starts_with("Last message repeated") {
            return;
        }
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 以下样本取自 gyan.dev FFmpeg 9.0.2 essentials 录制本地 FLV 流时的真实输出。
    const RUNNING: &str = "frame=   27 fps=0.0 q=-1.0 size=      28KiB time=00:00:01.04 bitrate= 219.0kbits/s speed=2.02x elapsed=0:00:00.51";
    const FINAL: &str = "frame=  126 fps= 37 q=-1.0 Lsize=     119KiB time=00:00:04.99 bitrate= 195.2kbits/s speed=1.45x elapsed=0:00:03.44    ";
    const STARTING: &str = "frame=    0 fps=0.0 q=0.0 size=       0KiB time=N/A bitrate=N/A speed=N/A elapsed=0:00:00.50";
    // 分段录制（segment 封装器）时 size 和 bitrate 为 N/A。
    const SEGMENT: &str = "frame=   91 fps= 30 q=-1.0 size=N/A time=00:00:03.59 bitrate=N/A speed=1.18x elapsed=0:00:03.04";

    #[test]
    fn parses_running_stats_line() {
        let progress = parse_progress_line(RUNNING).unwrap();
        assert_eq!(progress.size_bytes, Some(28 * 1024));
        assert_eq!(progress.duration_secs, Some(1.04));
        assert_eq!(progress.bitrate_kbps, Some(219.0));
        assert_eq!(progress.speed, Some(2.02));
        assert!(progress.has_media());
    }

    #[test]
    fn parses_final_stats_line_with_lsize() {
        let progress = parse_progress_line(FINAL).unwrap();
        assert_eq!(progress.size_bytes, Some(119 * 1024));
        assert_eq!(progress.duration_secs, Some(4.99));
        assert_eq!(progress.bitrate_kbps, Some(195.2));
    }

    #[test]
    fn missing_values_are_none() {
        let progress = parse_progress_line(STARTING).unwrap();
        assert_eq!(progress.size_bytes, Some(0));
        assert_eq!(progress.duration_secs, None);
        assert_eq!(progress.bitrate_kbps, None);
        assert_eq!(progress.speed, None);
        assert!(!progress.has_media());

        let progress = parse_progress_line(SEGMENT).unwrap();
        assert_eq!(progress.size_bytes, None);
        assert_eq!(progress.duration_secs, Some(3.59));
        assert_eq!(progress.bitrate_kbps, None);
        assert_eq!(progress.speed, Some(1.18));
        assert!(progress.has_media());
    }

    #[test]
    fn segment_progress_is_filled_from_file_sizes() {
        let mut progress = parse_progress_line(SEGMENT).unwrap();
        fill_from_files(&mut progress, 100_000);
        assert_eq!(progress.size_bytes, Some(100_000));
        // 100000 B × 8 / 3.59 s ≈ 222.8 kbit/s
        assert_eq!(progress.bitrate_kbps, Some(222.8));

        let mut running = parse_progress_line(RUNNING).unwrap();
        fill_from_files(&mut running, 1);
        assert_eq!(running.size_bytes, Some(28 * 1024));
        assert_eq!(running.bitrate_kbps, Some(219.0));
    }

    #[test]
    fn parses_long_durations_and_large_sizes() {
        let line = "frame=162000 fps= 25 q=-1.0 size= 1536000KiB time=01:48:00.52 bitrate=1941.6kbits/s speed=   1x";
        let progress = parse_progress_line(line).unwrap();
        assert_eq!(progress.size_bytes, Some(1_536_000 * 1024));
        assert_eq!(progress.duration_secs, Some(6480.52));
        assert_eq!(progress.bitrate_kbps, Some(1941.6));
    }

    #[test]
    fn audio_only_and_negative_time_are_supported() {
        let line = "size=     256KiB time=-00:00:00.02 bitrate=N/A speed=N/A";
        let progress = parse_progress_line(line).unwrap();
        assert_eq!(progress.size_bytes, Some(256 * 1024));
        assert_eq!(progress.duration_secs, Some(0.0));
    }

    #[test]
    fn error_lines_are_not_progress() {
        for line in [
            "[http @ 000001dd9cf43ec0] Stream ends prematurely at 88168, should be 18446744073709551615",
            "[in#0 @ 000001ba43a31080] Error opening input: Error number -138 occurred",
            "Error opening input file http://127.0.0.1:9/live.flv.",
            "[out#0/mpegts @ 0000020acf3daf80] Error muxing a packet",
        ] {
            assert_eq!(parse_progress_line(line), None, "{line}");
        }
    }

    #[test]
    fn field_requires_word_boundary() {
        // `Lsize=` 不能被当成 `size=`，但可以单独取到。
        assert_eq!(field(FINAL, "size"), None);
        assert_eq!(field(FINAL, "Lsize"), Some("119KiB"));
    }

    #[test]
    fn splitter_handles_carriage_returns_and_partial_chunks() {
        let mut splitter = LineSplitter::default();
        let stream = format!("{RUNNING}\r{FINAL}\r\n[x] err");
        let bytes = stream.as_bytes();
        let mut lines = splitter.push(&bytes[..10]);
        lines.extend(splitter.push(&bytes[10..]));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], RUNNING);
        assert_eq!(lines[1], FINAL.trim());
        assert_eq!(splitter.finish().as_deref(), Some("[x] err"));
        assert_eq!(splitter.finish(), None);
    }

    #[test]
    fn splitter_decodes_utf8_paths() {
        let mut splitter = LineSplitter::default();
        let lines = splitter.push("D:\\录像\\主播.ts: No space left on device\n".as_bytes());
        assert_eq!(lines, vec!["D:\\录像\\主播.ts: No space left on device"]);
    }

    #[test]
    fn error_tail_keeps_latest_lines() {
        let mut tail = ErrorTail::new(2);
        tail.push("a".into());
        tail.push("Last message repeated 1 times".into());
        tail.push("b".into());
        tail.push("c".into());
        assert_eq!(tail.lines(), vec!["b".to_string(), "c".to_string()]);
    }
}
