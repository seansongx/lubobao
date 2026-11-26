//! 时间字幕：在录像文件旁写同名 `.srt`，每秒一条，文字是当时的本地时间。
//!
//! 对应 StreamCap 设置 `generate_time_subtitle_file`。只用于未分段的录制（与上游一致）。
//! 字幕随录制增量追加，每条写完即交给操作系统，进程被强杀时已写部分仍可用。
//! 写入失败只记警告，不影响录制。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Local};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

/// 字幕文件扩展名。
pub const SRT_EXTENSION: &str = "srt";

/// 字幕里时间的格式。
const TEXT_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// 录像文件对应的字幕路径：同目录同主体名，扩展名换成 `.srt`。
/// 录像转成 MP4 后文件名主体不变，仍然匹配。
pub fn srt_path(video_path: &Path) -> PathBuf {
    video_path.with_extension(SRT_EXTENSION)
}

/// SRT 时间轴，`HH:MM:SS,mmm`。
pub fn format_timestamp(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02},000",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// 第 `index`（从 0 开始）秒的字幕条目：覆盖 `[index, index + 1)` 秒，文字是
/// `started_at + index 秒` 的本地时间。
pub fn format_entry(index: u64, started_at: DateTime<Local>) -> String {
    let time = started_at + Duration::seconds(i64::try_from(index).unwrap_or(i64::MAX));
    format!(
        "{}\n{} --> {}\n{}\n\n",
        index + 1,
        format_timestamp(index),
        format_timestamp(index + 1),
        time.format(TEXT_FORMAT)
    )
}

/// 增量写入字幕的写入器。文件在第一次写入时才创建，没写过就不会留下空文件。
pub struct SubtitleWriter {
    path: PathBuf,
    started_at: DateTime<Local>,
    file: Option<File>,
    /// 下一条要写的序号（从 0 开始），即已写入的条数。
    next_index: u64,
    /// 写入失败后不再尝试。
    failed: bool,
}

impl SubtitleWriter {
    pub fn new(video_path: &Path, started_at: DateTime<Local>) -> Self {
        Self {
            path: srt_path(video_path),
            started_at,
            file: None,
            next_index: 0,
            failed: false,
        }
    }

    /// 字幕文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 补写到录制已进行 `elapsed_secs` 秒为止的所有条目（含第 `elapsed_secs` 条）。
    /// 失败时记一次警告，之后不再写。
    pub async fn write_until(&mut self, elapsed_secs: u64) {
        if self.failed || self.next_index > elapsed_secs {
            return;
        }
        let mut text = String::new();
        for index in self.next_index..=elapsed_secs {
            text.push_str(&format_entry(index, self.started_at));
        }
        if let Err(error) = self.append(&text).await {
            log::warn!("写入时间字幕失败 {}：{error}", self.path.display());
            self.failed = true;
            return;
        }
        self.next_index = elapsed_secs + 1;
    }

    async fn append(&mut self, text: &str) -> std::io::Result<()> {
        if self.file.is_none() {
            // 同名旧文件（例如录像已被删除而字幕还在）直接覆盖。
            self.file = Some(File::create(&self.path).await?);
        }
        let file = self.file.as_mut().expect("file was just created");
        file.write_all(text.as_bytes()).await?;
        // tokio 的文件写入先进内部缓冲，flush 后才交给操作系统。
        file.flush().await
    }

    /// 录制结束：关闭文件。没有产出录像文件时删除字幕，不留下孤立的字幕。
    pub async fn finish(mut self, has_video: bool) {
        self.file = None;
        if !has_video
            && let Err(error) = tokio::fs::remove_file(&self.path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("删除多余的时间字幕失败 {}：{error}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn start() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 22, 9, 59, 58).unwrap()
    }

    #[test]
    fn srt_path_replaces_the_video_extension() {
        assert_eq!(
            srt_path(Path::new("out/主播_2026-11-22_09-59-58.ts")),
            Path::new("out/主播_2026-11-22_09-59-58.srt")
        );
        assert_eq!(
            srt_path(Path::new("out/主播_2026-11-22_09-59-58.mp4")),
            srt_path(Path::new("out/主播_2026-11-22_09-59-58.ts"))
        );
    }

    #[test]
    fn timestamps_count_hours_minutes_and_seconds_from_the_start() {
        assert_eq!(format_timestamp(0), "00:00:00,000");
        assert_eq!(format_timestamp(59), "00:00:59,000");
        assert_eq!(format_timestamp(3661), "01:01:01,000");
        assert_eq!(format_timestamp(100 * 3600), "100:00:00,000");
    }

    #[test]
    fn entries_show_the_wall_clock_time_of_that_second() {
        assert_eq!(
            format_entry(0, start()),
            "1\n00:00:00,000 --> 00:00:01,000\n2026-11-22 09:59:58\n\n"
        );
        assert_eq!(
            format_entry(2, start()),
            "3\n00:00:02,000 --> 00:00:03,000\n2026-11-22 10:00:00\n\n"
        );
    }

    #[tokio::test]
    async fn writer_appends_one_entry_per_second_and_flushes_each_write() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("主播_2026-11-22_09-59-58.ts");
        let mut writer = SubtitleWriter::new(&video, start());
        writer.write_until(0).await;
        // 还没有 finish，内容已经能从磁盘读到。
        let first = std::fs::read_to_string(writer.path()).unwrap();
        assert_eq!(first, format_entry(0, start()));
        writer.write_until(2).await;
        // 同一秒重复调用不重复写。
        writer.write_until(2).await;
        let text = std::fs::read_to_string(writer.path()).unwrap();
        assert_eq!(
            text,
            format!(
                "{}{}{}",
                format_entry(0, start()),
                format_entry(1, start()),
                format_entry(2, start())
            )
        );
        writer.finish(true).await;
        assert!(dir.path().join("主播_2026-11-22_09-59-58.srt").exists());
    }

    #[tokio::test]
    async fn writer_leaves_no_file_when_nothing_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let writer = SubtitleWriter::new(&dir.path().join("a.ts"), start());
        writer.finish(false).await;
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn writer_removes_the_subtitle_when_no_video_was_produced() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SubtitleWriter::new(&dir.path().join("a.ts"), start());
        writer.write_until(3).await;
        assert!(dir.path().join("a.srt").exists());
        writer.finish(false).await;
        assert!(!dir.path().join("a.srt").exists());
    }

    #[tokio::test]
    async fn writer_overwrites_a_stale_subtitle_with_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.srt"), "旧内容").unwrap();
        let mut writer = SubtitleWriter::new(&dir.path().join("a.ts"), start());
        writer.write_until(0).await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.srt")).unwrap(),
            format_entry(0, start())
        );
    }

    #[tokio::test]
    async fn write_failure_is_not_fatal_and_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        // 目录不存在，创建文件会失败。
        let mut writer = SubtitleWriter::new(&dir.path().join("missing").join("a.ts"), start());
        writer.write_until(1).await;
        writer.write_until(5).await;
        assert!(writer.failed);
        writer.finish(false).await;
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
