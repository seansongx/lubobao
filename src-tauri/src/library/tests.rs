use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::*;

fn write_file(path: &Path, bytes: usize, age_secs: u64) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, vec![0u8; bytes]).unwrap();
    let time = SystemTime::now() - Duration::from_secs(age_secs);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
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
    assert_eq!(stats(&root).used_bytes, 0);
}

#[test]
fn link_inside_root_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(&root.join("真实目录").join("a.ts"), 1, 0);
    link_dir(&root.join("真实目录"), &root.join("快捷方式"));
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

    let stats = stats(&root);
    assert!(!stats.root_exists);
    assert_eq!(stats.used_bytes, 0);
    // 根目录不存在时仍能查询上级目录所在磁盘的剩余空间。
    assert!(stats.available_bytes.is_some());
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
fn stats_sums_all_files_and_counts_media() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(&root.join("抖音").join("甲").join("a.ts"), 100, 0);
    write_file(&root.join("抖音").join("乙").join("b.mp4"), 50, 0);
    write_file(&root.join("readme.txt"), 7, 0);
    let stats = stats(root);
    assert!(stats.root_exists);
    assert_eq!(stats.used_bytes, 157);
    assert_eq!(stats.file_count, 2);
    assert!(stats.available_bytes.is_some());
    assert!(stats.total_bytes.unwrap() >= stats.available_bytes.unwrap());
}

#[test]
fn library_error_serializes_kind_in_snake_case() {
    let error = LibraryError::new(LibraryErrorKind::RootMissing, "x", Path::new("D:/录播宝"));
    let value = serde_json::to_value(&error).unwrap();
    assert_eq!(value["kind"], "root_missing");
    assert_eq!(value["root"], "D:/录播宝");
}
