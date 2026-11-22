//! 录后转 MP4 的测试。用 lavfi 生成带音视频的 TS 样本，需要设置 `LUBOBAO_TEST_FFMPEG`，
//! 没设置时跳过（解析时长、磁盘空间等不需要 ffmpeg 的测试照常运行）。

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Instant;

use super::*;

/// 转换时按实时速率读取的原件，用来测试中途退出。
static SLOW: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

pub(super) fn is_slow(source: &Path) -> bool {
    lock(&SLOW).as_ref().is_some_and(|set| set.contains(source))
}

fn mark_slow(source: &Path) {
    lock(&SLOW)
        .get_or_insert_with(HashSet::new)
        .insert(source.to_path_buf());
}

fn test_ffmpeg() -> Option<PathBuf> {
    match std::env::var_os("LUBOBAO_TEST_FFMPEG") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => {
            eprintln!("跳过：未设置 LUBOBAO_TEST_FFMPEG（ffmpeg 可执行文件路径）");
            None
        }
    }
}

/// 生成 TS 样本：320×240、25 fps 的 H.264，加 44.1 kHz 正弦波 AAC（TS 里是 ADTS）。
fn make_ts(ffmpeg: &Path, path: &Path, secs: u32) {
    let output = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=size=320x240:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100"])
        .args(["-t", &secs.to_string()])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-g", "25",
        ])
        .args(["-c:a", "aac", "-b:a", "96k", "-f", "mpegts"])
        .arg(path)
        .output()
        .expect("run ffmpeg to create sample");
    assert!(
        output.status.success(),
        "sample generation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 解码整个文件，确认没有任何错误输出。
fn assert_playable(ffmpeg: &Path, file: &Path) {
    let output = std::process::Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg to decode");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.trim().is_empty(),
        "{} 解码出错：{stderr}",
        file.display()
    );
}

/// 确认 MP4 里有视频和音频两路流。
fn assert_has_av(ffmpeg: &Path, file: &Path) {
    let output = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(file)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Video: h264"), "{stderr}");
    assert!(stderr.contains("Audio: aac"), "{stderr}");
}

/// 收集事件，测试结束时读取。
fn collector() -> (ConvertListener, Arc<Mutex<Vec<ConvertEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let listener: ConvertListener = Arc::new(move |event: &ConvertEvent| {
        lock(&sink).push(event.clone());
    });
    (listener, events)
}

fn options(ffmpeg: &Path, delete_original: bool) -> ConvertOptions {
    ConvertOptions {
        ffmpeg_path: ffmpeg.to_path_buf(),
        delete_original,
    }
}

/// 等队列清空（测试用，不关闭队列）。
async fn drain(processor: &PostProcessor) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while processor.pending() > 0 {
        assert!(Instant::now() < deadline, "转换超时");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn finals(events: &[ConvertEvent]) -> Vec<&ConvertEvent> {
    events
        .iter()
        .filter(|event| !matches!(event, ConvertEvent::Started { .. }))
        .collect()
}

fn leftover_parts(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".part"))
        .collect()
}

#[test]
fn parses_duration_from_ffmpeg_header_output() {
    let stderr = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'a.mp4':\n  Duration: 00:01:02.50, start: 0.000000, bitrate: 100 kb/s\n";
    assert_eq!(parse_duration(stderr), Some(62.5));
    assert_eq!(
        parse_duration("  Duration: 01:00:00.00, start: 1.4"),
        Some(3600.0)
    );
    assert_eq!(parse_duration("  Duration: N/A, start: 0.0"), None);
    assert_eq!(parse_duration("  Duration: 00:00:00.00, start: 0.0"), None);
    assert_eq!(parse_duration("moov atom not found"), None);
}

#[test]
fn convert_args_copy_streams_and_write_mp4_to_the_temp_file() {
    let args = convert_args(Path::new("a b/录像.ts"), Path::new("a b/录像.mp4.part"));
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let joined = args.join(" ");
    assert!(joined.contains("-i a b/录像.ts"));
    assert!(joined.contains("-c copy"));
    assert!(joined.contains("-bsf:a aac_adtstoasc"));
    assert!(joined.contains("-movflags +faststart"));
    assert!(joined.contains("-f mp4"));
    assert_eq!(args.last().unwrap(), "a b/录像.mp4.part");
}

#[test]
fn target_name_does_not_overwrite_an_existing_mp4() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rec_000.ts");
    assert_eq!(unique_target(&source), temp.path().join("rec_000.mp4"));
    std::fs::write(temp.path().join("rec_000.mp4"), b"x").unwrap();
    assert_eq!(unique_target(&source), temp.path().join("rec_000_2.mp4"));
    assert_eq!(
        part_path(&temp.path().join("rec_000_2.mp4")),
        temp.path().join("rec_000_2.mp4.part")
    );
}

#[tokio::test]
async fn too_little_disk_space_fails_and_keeps_the_original() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rec.ts");
    std::fs::write(&source, vec![0u8; 1024]).unwrap();
    let processor = PostProcessor::with_space_probe(1, Arc::new(|_: &Path| Ok(1024)));
    let (listener, events) = collector();
    // ffmpeg 路径不存在也没关系：空间检查在启动 ffmpeg 之前。
    assert!(processor.enqueue(
        vec![source.clone()],
        options(Path::new("missing-ffmpeg"), true),
        listener
    ));
    drain(&processor).await;
    let events = lock(&events).clone();
    assert!(
        matches!(&events[..], [ConvertEvent::Failed { message, .. }] if message.contains("磁盘剩余空间不足")),
        "{events:?}"
    );
    assert!(source.exists());
}

#[tokio::test]
async fn converts_ts_to_a_playable_mp4_and_keeps_the_original() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rec_000.ts");
    make_ts(&ffmpeg, &source, 4);
    let processor = PostProcessor::default();
    let (listener, events) = collector();
    assert!(processor.enqueue(vec![source.clone()], options(&ffmpeg, false), listener));
    drain(&processor).await;

    let target = temp.path().join("rec_000.mp4");
    let events = lock(&events).clone();
    assert!(
        matches!(events.first(), Some(ConvertEvent::Started { target: t, .. }) if *t == target),
        "{events:?}"
    );
    match events.last() {
        Some(ConvertEvent::Succeeded {
            target: t,
            duration_secs,
            original_deleted,
            ..
        }) => {
            assert_eq!(*t, target);
            assert!((3.5..4.6).contains(duration_secs), "{duration_secs}");
            assert!(!original_deleted);
        }
        other => panic!("{other:?}"),
    }
    assert!(source.exists(), "不删除原件时应保留 TS");
    assert_playable(&ffmpeg, &target);
    assert_has_av(&ffmpeg, &target);
    assert!(leftover_parts(temp.path()).is_empty());
}

#[tokio::test]
async fn converts_every_segment_one_at_a_time_and_deletes_originals_when_asked() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let sources: Vec<PathBuf> = (0..3)
        .map(|n| temp.path().join(format!("rec_{n:03}.ts")))
        .collect();
    for source in &sources {
        make_ts(&ffmpeg, source, 2);
    }
    let processor = PostProcessor::default();
    let (listener, events) = collector();
    assert!(processor.enqueue(sources.clone(), options(&ffmpeg, true), listener));
    drain(&processor).await;

    let events = lock(&events).clone();
    // 一次只转一个：开始、完成交替出现，并按分段顺序。
    assert_eq!(events.len(), 6, "{events:?}");
    for (n, pair) in events.chunks(2).enumerate() {
        match pair {
            [
                ConvertEvent::Started { source, .. },
                ConvertEvent::Succeeded {
                    source: done,
                    target,
                    original_deleted: true,
                    ..
                },
            ] => {
                assert_eq!(*source, sources[n]);
                assert_eq!(*done, sources[n]);
                assert_eq!(*target, sources[n].with_extension("mp4"));
            }
            other => panic!("{other:?}"),
        }
    }
    for source in &sources {
        assert!(!source.exists(), "确认 MP4 有效后应删除 TS");
        assert_playable(&ffmpeg, &source.with_extension("mp4"));
    }
}

#[tokio::test]
async fn corrupt_input_keeps_the_original_and_leaves_no_mp4() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("broken.ts");
    std::fs::write(&source, b"<html>not a transport stream</html>".repeat(200)).unwrap();
    let processor = PostProcessor::default();
    let (listener, events) = collector();
    assert!(processor.enqueue(vec![source.clone()], options(&ffmpeg, true), listener));
    drain(&processor).await;

    let events = lock(&events).clone();
    assert!(
        matches!(finals(&events)[..], [ConvertEvent::Failed { .. }]),
        "{events:?}"
    );
    assert!(source.exists(), "转换失败时应保留原件");
    assert!(!temp.path().join("broken.mp4").exists());
    assert!(leftover_parts(temp.path()).is_empty());
}

#[tokio::test]
async fn handles_chinese_names_and_spaces_in_paths() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("抖音").join("偏暖的 少年(声控)");
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("偏暖的 少年_2026-11-22_12-00-00_000.ts");
    make_ts(&ffmpeg, &source, 2);
    let processor = PostProcessor::default();
    let (listener, events) = collector();
    assert!(processor.enqueue(vec![source.clone()], options(&ffmpeg, false), listener));
    drain(&processor).await;

    let events = lock(&events).clone();
    assert!(
        matches!(finals(&events)[..], [ConvertEvent::Succeeded { .. }]),
        "{events:?}"
    );
    assert_playable(&ffmpeg, &source.with_extension("mp4"));
}

#[tokio::test]
async fn existing_mp4_is_not_overwritten() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("rec.ts");
    make_ts(&ffmpeg, &source, 2);
    std::fs::write(temp.path().join("rec.mp4"), b"keep me").unwrap();
    let processor = PostProcessor::default();
    let (listener, _events) = collector();
    assert!(processor.enqueue(vec![source], options(&ffmpeg, false), listener));
    drain(&processor).await;
    assert_eq!(
        std::fs::read(temp.path().join("rec.mp4")).unwrap(),
        b"keep me"
    );
    assert_playable(&ffmpeg, &temp.path().join("rec_2.mp4"));
}

#[tokio::test]
async fn shutdown_aborts_the_running_conversion_and_drops_the_queue() {
    let Some(ffmpeg) = test_ffmpeg() else { return };
    let temp = tempfile::tempdir().unwrap();
    let slow = temp.path().join("slow.ts");
    let queued = temp.path().join("queued.ts");
    make_ts(&ffmpeg, &slow, 20);
    make_ts(&ffmpeg, &queued, 2);
    mark_slow(&slow);
    let processor = PostProcessor::default();
    let (listener, events) = collector();
    assert!(processor.enqueue(
        vec![slow.clone(), queued.clone()],
        options(&ffmpeg, true),
        listener
    ));

    // 等 ffmpeg 开始写临时文件。
    let part = temp.path().join("slow.mp4.part");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !part.exists() {
        assert!(Instant::now() < deadline, "转换没有开始");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let started = Instant::now();
    assert!(processor.shutdown(SHUTDOWN_TIMEOUT).await);
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "退出用了 {elapsed:?}");

    let events = lock(&events).clone();
    let finals = finals(&events);
    assert!(
        finals.contains(&&ConvertEvent::Cancelled {
            source: queued.clone(),
            started: false
        }),
        "{events:?}"
    );
    assert!(
        finals.contains(&&ConvertEvent::Cancelled {
            source: slow.clone(),
            started: true
        }),
        "{events:?}"
    );
    assert!(slow.exists() && queued.exists(), "中止时应保留原件");
    assert!(!temp.path().join("slow.mp4").exists());
    assert!(!temp.path().join("queued.mp4").exists());
    assert!(leftover_parts(temp.path()).is_empty());
    // 关闭后不再接收。
    let (listener, _) = collector();
    assert!(!processor.enqueue(vec![slow], options(&ffmpeg, false), listener));
}
