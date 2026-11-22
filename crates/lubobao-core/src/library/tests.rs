use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::*;

fn write_file(path: &Path, bytes: usize, age_secs: u64) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut file = fs::File::create(path).unwrap();
    file.write_all(&vec![0u8; bytes]).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(age_secs))
        .unwrap();
}

fn names(listing: &LibraryListing) -> Vec<&str> {
    listing.entries.iter().map(|e| e.name.as_str()).collect()
}

/// 在 `link` 处建一个指向 `target` 的目录链接：Windows 用 junction（不需要管理员权限），其他系统用符号链接。
fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J 失败");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn split_relative_rejects_parent_and_absolute_paths() {
    for bad in [
        "..",
        "../outside",
        "a/../../b",
        "a\\..\\b",
        "/etc",
        "\\Windows",
        "C:\\Windows",
        "C:foo",
        "a/C:/b",
        "\\\\server\\share",
        "file.ts:stream",
    ] {
        assert!(split_relative(bad).is_err(), "应拒绝 {bad:?}");
    }
}

#[test]
fn split_relative_normalizes_separators_and_dots() {
    assert_eq!(split_relative("").unwrap(), Vec::<String>::new());
    assert_eq!(split_relative("./抖音//主播\\").unwrap(), ["抖音", "主播"]);
}

#[test]
fn resolve_rejects_parent_traversal_out_of_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("录播宝");
    fs::create_dir(&root).unwrap();
    write_file(&dir.path().join("secret.ts"), 1, 0);
    let error = resolve(&root, "../secret.ts").unwrap_err();
    assert_eq!(error.kind, LibraryErrorKind::InvalidPath);
    let error = list_dir(&root, "..").unwrap_err();
    assert_eq!(error.kind, LibraryErrorKind::InvalidPath);
}

#[test]
fn resolve_rejects_absolute_path_even_inside_root() {
    let dir = tempfile::tempdir().unwrap();
    write_file(&dir.path().join("a.ts"), 1, 0);
    let absolute = dir.path().join("a.ts");
    let error = resolve(dir.path(), &absolute.to_string_lossy()).unwrap_err();
    assert_eq!(error.kind, LibraryErrorKind::InvalidPath);
}

#[test]
fn resolve_rejects_link_that_escapes_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    write_file(&outside.join("leak.ts"), 1, 0);
    fs::create_dir(&root).unwrap();
    link_dir(&outside, &root.join("escape"));

    let error = list_dir(&root, "escape").unwrap_err();
    assert_eq!(error.kind, LibraryErrorKind::InvalidPath);
    let error = resolve_file(&root, "escape/leak.ts", true).unwrap_err();
    assert_eq!(error.kind, LibraryErrorKind::InvalidPath);
    // 列表里也不显示越界的链接。
    let listing = list_dir(&root, "").unwrap();
    assert!(names(&listing).is_empty(), "{:?}", names(&listing));
    // 占用统计不跟随链接。
    assert_eq!(usage(&root).unwrap().used_bytes, 0);
}

#[test]
fn link_inside_root_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(&root.join("真实目录").join("a.ts"), 1, 0);
    link_dir(&root.join("真实目录"), &root.join("快捷方式"));
    // 根目录列表里出现这个链接，按目标当作目录。
    let top = list_dir(root, "").unwrap();
    assert_eq!(names(&top), ["快捷方式", "真实目录"]);
    assert!(top.entries.iter().all(|entry| entry.kind == EntryKind::Dir));
    assert_eq!(top.entries[0].child_count, Some(1));
    let listing = list_dir(root, "快捷方式").unwrap();
    assert_eq!(names(&listing), ["a.ts"]);
    assert_eq!(listing.entries[0].path, "快捷方式/a.ts");
}

#[test]
fn list_dir_handles_chinese_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("视频").join("录播宝");
    write_file(
        &root
            .join("抖音")
            .join("一只小羊驼")
            .join("一只小羊驼_2026-11-22_20-14-03.ts"),
        10,
        0,
    );

    let top = list_dir(&root, "").unwrap();
    assert_eq!(names(&top), ["抖音"]);
    assert_eq!(top.entries[0].kind, EntryKind::Dir);
    assert_eq!(top.entries[0].child_count, Some(1));
    assert!(top.root.ends_with("录播宝"));

    let anchor = list_dir(&root, "抖音/一只小羊驼").unwrap();
    assert_eq!(anchor.path, "抖音/一只小羊驼");
    let file = &anchor.entries[0];
    assert_eq!(file.name, "一只小羊驼_2026-11-22_20-14-03.ts");
    assert_eq!(
        file.path,
        "抖音/一只小羊驼/一只小羊驼_2026-11-22_20-14-03.ts"
    );
    assert_eq!(file.size, Some(10));

    let resolved = resolve_file(&root, &file.path, true).unwrap();
    assert!(resolved.path.ends_with("一只小羊驼_2026-11-22_20-14-03.ts"));
    // Windows 反斜杠写法也能解析到同一个文件。
    let backslash = resolve_file(
        &root,
        "抖音\\一只小羊驼\\一只小羊驼_2026-11-22_20-14-03.ts",
        true,
    )
    .unwrap();
    assert_eq!(backslash.canonical, resolved.canonical);
}

#[test]
fn list_dir_puts_dirs_first_and_files_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("b目录")).unwrap();
    fs::create_dir(root.join("A目录")).unwrap();
    write_file(&root.join("old.mp4"), 1, 3_600);
    write_file(&root.join("new.flv"), 1, 10);
    write_file(&root.join("mid.MKV"), 1, 600);
    write_file(&root.join("notes.txt"), 1, 0);
    write_file(&root.join(".hidden.ts"), 1, 0);
    let listing = list_dir(root, "").unwrap();
    assert_eq!(
        names(&listing),
        ["A目录", "b目录", "new.flv", "mid.MKV", "old.mp4"]
    );
}

#[test]
fn segments_are_grouped_and_ordered_by_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // 较早的一次录制，两个分段。
    write_file(&root.join("主播_2026-11-21_20-00-00_000.ts"), 1, 7_200);
    write_file(&root.join("主播_2026-11-21_20-00-00_001.ts"), 1, 5_400);
    // 最近一次录制，分段 0..=10，较晚的分段修改时间更新。
    for index in 0..=10u64 {
        write_file(
            &root.join(format!("主播_2026-11-22_20-00-00_{index:03}.ts")),
            1,
            1_000 - index * 10,
        );
    }
    let listing = list_dir(root, "").unwrap();
    let got = names(&listing);
    let mut expected: Vec<String> = (0..=10)
        .map(|i| format!("主播_2026-11-22_20-00-00_{i:03}.ts"))
        .collect();
    expected.push("主播_2026-11-21_20-00-00_000.ts".into());
    expected.push("主播_2026-11-21_20-00-00_001.ts".into());
    assert_eq!(got, expected);
    assert_eq!(listing.entries[10].segment, Some(10));
    assert_eq!(
        listing.entries[0].group.as_deref(),
        Some("主播_2026-11-22_20-00-00.ts")
    );
}

#[test]
fn split_segment_requires_three_digit_suffix() {
    assert_eq!(
        split_segment("a_2026-11-22_20-14-03_007.ts"),
        ("a_2026-11-22_20-14-03.ts".into(), Some(7))
    );
    assert_eq!(
        split_segment("a_2026-11-22_20-14-03.ts"),
        ("a_2026-11-22_20-14-03.ts".into(), None)
    );
    assert_eq!(split_segment("a_12.ts"), ("a_12.ts".into(), None));
    assert_eq!(split_segment("_000.ts"), ("_000.ts".into(), None));
}

#[test]
fn missing_root_returns_root_missing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("不存在的目录");
    let error = list_dir(&root, "").unwrap_err();
    assert_eq!(error.kind, LibraryErrorKind::RootMissing);
    assert!(error.root.ends_with("不存在的目录"));
    assert!(error.message.contains("不存在的目录"));

    assert_eq!(usage(&root), None);
    let space = disk_space(&root);
    assert!(!space.root_exists);
    // 根目录不存在时仍能查询上级目录所在磁盘的剩余空间。
    assert!(space.available_bytes.is_some());
}

#[test]
fn root_that_is_a_file_returns_root_not_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("file.ts");
    write_file(&root, 1, 0);
    assert_eq!(
        list_dir(&root, "").unwrap_err().kind,
        LibraryErrorKind::RootNotDirectory
    );
}

#[test]
fn missing_subdirectory_returns_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let error = list_dir(dir.path(), "已删除").unwrap_err();
    assert_eq!(error.kind, LibraryErrorKind::NotFound);
}

#[test]
fn empty_directory_lists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("空目录")).unwrap();
    let listing = list_dir(dir.path(), "空目录").unwrap();
    assert!(listing.entries.is_empty());
    let top = list_dir(dir.path(), "").unwrap();
    assert_eq!(top.entries[0].child_count, Some(0));
}

#[test]
fn resolve_file_only_opens_media_files() {
    let dir = tempfile::tempdir().unwrap();
    write_file(&dir.path().join("run.bat"), 1, 0);
    fs::create_dir(dir.path().join("目录")).unwrap();
    for path in ["run.bat", "目录", ""] {
        let error = resolve_file(dir.path(), path, true).unwrap_err();
        assert_eq!(error.kind, LibraryErrorKind::InvalidTarget, "{path}");
    }
    // 在文件夹中显示不限类型。
    assert!(resolve_file(dir.path(), "目录", false).is_ok());
}

#[test]
fn usage_sums_all_files_and_counts_media() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(&root.join("抖音").join("甲").join("a.ts"), 100, 0);
    write_file(&root.join("抖音").join("乙").join("b.mp4"), 50, 0);
    write_file(&root.join("readme.txt"), 7, 0);
    assert_eq!(
        usage(root),
        Some(LibraryUsage {
            used_bytes: 157,
            file_count: 2
        })
    );
    let space = disk_space(root);
    assert!(space.root_exists);
    assert!(space.total_bytes.unwrap() >= space.available_bytes.unwrap());
}

#[test]
fn library_error_serializes_kind_in_snake_case() {
    let error = LibraryError::new(LibraryErrorKind::RootMissing, "x", Path::new("D:/录播宝"));
    let value = serde_json::to_value(&error).unwrap();
    assert_eq!(value["kind"], "root_missing");
    assert_eq!(value["root"], "D:/录播宝");
}

#[test]
fn canonical_check_rejects_nested_path_through_escaping_link() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    write_file(&outside.join("leak.ts"), 1, 0);
    fs::create_dir_all(root.join("内层")).unwrap();
    link_dir(&outside, &root.join("内层").join("escape"));

    // 路径本身合法，只有规范化后的越界检查能拦住。
    let path = "内层/escape/leak.ts";
    assert!(split_relative(path).is_ok());
    for media_only in [true, false] {
        let error = resolve_file(&root, path, media_only).unwrap_err();
        assert_eq!(error.kind, LibraryErrorKind::InvalidPath);
        assert!(error.message.contains("之外"), "{}", error.message);
    }
    assert_eq!(
        list_dir(&root, "内层/escape").unwrap_err().kind,
        LibraryErrorKind::InvalidPath
    );
    assert!(list_dir(&root, "内层").unwrap().entries.is_empty());
    // 越界的链接也不计入子项数。
    assert_eq!(count_in(&root, ""), Some(0));
}

/// 根目录下第一个目录的子项数。
fn count_in(root: &Path, relative: &str) -> Option<u64> {
    list_dir(root, relative).unwrap().entries[0].child_count
}

/// 建文件符号链接。Windows 上需要开发者模式或管理员权限，没有时返回 false，调用方跳过。
fn link_file(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(target, link);
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    match result {
        Ok(()) => true,
        Err(error) => {
            eprintln!("无法创建文件符号链接，跳过：{error}");
            false
        }
    }
}

#[test]
fn media_check_uses_real_target_of_file_link() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(&root.join("payload.exe"), 1, 0);
    write_file(&root.join("真实录像.ts"), 1, 0);
    if !link_file(&root.join("payload.exe"), &root.join("a.ts"))
        || !link_file(&root.join("真实录像.ts"), &root.join("b.ts"))
    {
        return;
    }
    // 名为 a.ts、实际指向 exe 的链接：不列出，也不能打开。
    let error = resolve_file(root, "a.ts", true).unwrap_err();
    assert_eq!(error.kind, LibraryErrorKind::InvalidTarget);
    let listing = list_dir(root, "").unwrap();
    assert!(!names(&listing).contains(&"a.ts"), "{:?}", names(&listing));
    // 指向录像的链接可以打开，交给系统的是真实路径。
    assert!(names(&listing).contains(&"b.ts"));
    let resolved = resolve_file(root, "b.ts", true).unwrap();
    assert!(
        resolved.path.ends_with("真实录像.ts"),
        "{:?}",
        resolved.path
    );
}

#[test]
fn media_extension_check_is_case_insensitive_and_exact() {
    assert!(is_media_file("VIDEO.TS"));
    assert!(is_media_file("录像.Mp4"));
    assert!(!is_media_file("video.ts.exe"));
    assert!(!is_media_file("video.tsx"));
    assert!(!is_media_file("ts"));

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(&root.join("VIDEO.TS"), 1, 0);
    write_file(&root.join("video.ts.exe"), 1, 0);
    assert_eq!(names(&list_dir(root, "").unwrap()), ["VIDEO.TS"]);
    assert!(resolve_file(root, "VIDEO.TS", true).is_ok());
    assert_eq!(
        resolve_file(root, "video.ts.exe", true).unwrap_err().kind,
        LibraryErrorKind::InvalidTarget
    );
}

#[test]
fn resolved_path_is_the_real_path_without_verbatim_prefix() {
    let dir = tempfile::tempdir().unwrap();
    write_file(&dir.path().join("录像").join("a.ts"), 1, 0);
    let resolved = resolve_file(dir.path(), "录像/a.ts", true).unwrap();
    assert!(!resolved.path.to_string_lossy().starts_with(r"\\?\"));
    assert_eq!(
        fs::canonicalize(&resolved.path).unwrap(),
        resolved.canonical
    );
}

#[test]
fn names_with_backslash_or_colon_are_not_listable() {
    assert!(is_listable_name("主播_000.ts"));
    for name in [".hidden.ts", r"a\b.ts", "a:b.ts"] {
        assert!(!is_listable_name(name), "{name}");
    }
    // Linux、macOS 上真的能建出这种文件名：列表里跳过，避免列出后无法进入或打开。
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join(r"a\b.ts"), 1, 0);
        write_file(&dir.path().join("a:b.ts"), 1, 0);
        fs::create_dir(dir.path().join(r"x\y")).unwrap();
        write_file(&dir.path().join("ok.ts"), 1, 0);
        assert_eq!(names(&list_dir(dir.path(), "").unwrap()), ["ok.ts"]);
    }
}

#[test]
fn large_directory_lists_correctly_in_reasonable_time() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let anchor = root.join("抖音").join("主播");
    // 三次录制各 1000 段，另有 2000 个非录像文件（只按文件名过滤，不取元数据）。
    for (recording, age) in [("A", 30_000u64), ("B", 20_000), ("C", 10_000)] {
        for index in 0..1000u64 {
            write_file(
                &anchor.join(format!("主播_{recording}_{index:03}.ts")),
                0,
                age - index,
            );
        }
    }
    for index in 0..2000 {
        write_file(&anchor.join(format!("缩略图_{index}.jpg")), 0, 0);
    }

    let started = std::time::Instant::now();
    let listing = list_dir(root, "抖音/主播").unwrap();
    let top = list_dir(root, "抖音").unwrap();
    let elapsed = started.elapsed();

    assert_eq!(listing.entries.len(), 3000);
    assert_eq!(top.entries[0].child_count, Some(3000));
    // 最近一次录制（C）在前，组内按序号升序。
    let expected: Vec<String> = ["C", "B", "A"]
        .iter()
        .flat_map(|recording| (0..1000).map(move |i| format!("主播_{recording}_{i:03}.ts")))
        .collect();
    assert_eq!(names(&listing), expected);
    assert!(
        elapsed < Duration::from_secs(5),
        "列出 5000 个目录项耗时 {elapsed:?}"
    );
    eprintln!("列出 5000 个目录项（3000 个录像）两次，耗时 {elapsed:?}");
}

mod usage_cache {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::super::LibraryUsage;
    use super::super::usage::UsageCache;

    const RESULT: LibraryUsage = LibraryUsage {
        used_bytes: 42,
        file_count: 1,
    };

    static SINGLE_FLIGHT_WALKS: AtomicUsize = AtomicUsize::new(0);
    fn slow_walker(_: &Path) -> LibraryUsage {
        SINGLE_FLIGHT_WALKS.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
        RESULT
    }

    #[test]
    fn runs_one_walk_at_a_time_and_caches_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(UsageCache::with_walker(
            slow_walker,
            Duration::from_secs(60),
        ));

        // 遍历进行中的并发请求立即返回「统计中」，不会再启动遍历。
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let root = dir.path().to_path_buf();
                std::thread::spawn(move || cache.request(&root, false, Duration::ZERO))
            })
            .collect();
        for handle in handles {
            let snapshot = handle.join().unwrap();
            assert!(snapshot.computing);
            assert_eq!(snapshot.usage, None);
        }

        // 等待后拿到结果；有效期内再请求直接用缓存。
        let done = cache.request(dir.path(), false, Duration::from_secs(5));
        assert!(!done.computing);
        assert_eq!(done.usage, Some(RESULT));
        assert!(done.computed_at.is_some());
        let cached = cache.request(dir.path(), false, Duration::ZERO);
        assert_eq!(cached, done);
        assert_eq!(SINGLE_FLIGHT_WALKS.load(Ordering::SeqCst), 1);

        // 刷新：返回上次结果并标明正在统计，之后再遍历一次。
        let refreshing = cache.request(dir.path(), true, Duration::ZERO);
        assert!(refreshing.computing);
        assert_eq!(refreshing.usage, Some(RESULT));
        let refreshed = cache.request(dir.path(), false, Duration::from_secs(5));
        assert!(!refreshed.computing);
        assert_eq!(SINGLE_FLIGHT_WALKS.load(Ordering::SeqCst), 2);
    }

    static MISSING_WALKS: AtomicUsize = AtomicUsize::new(0);
    fn counting_walker(_: &Path) -> LibraryUsage {
        MISSING_WALKS.fetch_add(1, Ordering::SeqCst);
        RESULT
    }

    #[test]
    fn missing_root_is_reported_without_walking() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(UsageCache::with_walker(
            counting_walker,
            Duration::from_secs(60),
        ));
        let snapshot = cache.request(&dir.path().join("不存在"), true, Duration::from_secs(1));
        assert!(!snapshot.root_exists);
        assert!(!snapshot.computing);
        assert_eq!(snapshot.usage, None);
        assert_eq!(MISSING_WALKS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn real_walker_counts_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.ts"), [0u8; 10]).unwrap();
        let cache = Arc::new(UsageCache::default());
        let snapshot = cache.request(dir.path(), false, Duration::from_secs(5));
        assert_eq!(
            snapshot.usage,
            Some(LibraryUsage {
                used_bytes: 10,
                file_count: 1
            })
        );
    }
}
