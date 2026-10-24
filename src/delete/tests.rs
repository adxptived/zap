use super::report::{lock_failures, DeletionFailure};
use super::*;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Serializes tests that use the process-global failure injection.
static INJECT_SERIAL: Mutex<()> = Mutex::new(());

fn create_test_dir() -> PathBuf {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let temp = std::env::temp_dir().join(format!("zap-del-test-{pid}-{id}"));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(&temp).unwrap();
    temp
}

fn create_nested_dirs(base: &Path, depth: usize) -> Vec<PathBuf> {
    let mut dirs = vec![];
    let mut current = base.to_path_buf();
    for i in 0..depth {
        current = current.join(format!("level{}", i));
        fs::create_dir(&current).unwrap();
        dirs.push(current.clone());
    }
    dirs
}

#[test]
fn test_set_writable_makes_readonly_file_writable() {
    let temp = create_test_dir();
    let file_path = temp.join("readonly.txt");
    let mut file = File::create(&file_path).unwrap();
    file.write_all(b"test").unwrap();
    drop(file);
    let mut perms = fs::metadata(&file_path).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&file_path, perms).unwrap();
    assert!(fs::metadata(&file_path).unwrap().permissions().readonly());
    set_writable(&file_path).unwrap();
    assert!(!fs::metadata(&file_path).unwrap().permissions().readonly());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_file_removes_file() {
    let temp = create_test_dir();
    let file_path = temp.join("test.txt");
    File::create(&file_path).unwrap();
    assert!(file_path.exists());
    remove_file_with_retry(&file_path).unwrap();
    assert!(!file_path.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_directory_removes_nested_structure() {
    let temp = create_test_dir();
    let dirs = create_nested_dirs(&temp, 3);
    for dir in &dirs {
        File::create(dir.join("file.txt")).unwrap();
    }
    delete_directory(&temp, None).unwrap();
    assert!(!temp.exists());
}

#[test]
fn test_delete_directory_with_readonly_files() {
    let temp = create_test_dir();
    let file = temp.join("readonly.txt");
    File::create(&file).unwrap();
    let mut perms = fs::metadata(&file).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&file, perms).unwrap();
    delete_directory(&temp, None).unwrap();
    assert!(!temp.exists());
}

#[test]
fn test_delete_file_roots_bulk_removes_many_files() {
    let temp = create_test_dir();
    let files: Vec<PathBuf> = (0..128)
        .map(|i| {
            let path = temp.join(format!("file-{i}.txt"));
            File::create(&path).unwrap();
            path
        })
        .collect();

    let summary = delete_file_roots_bulk(&files, false, false, None);
    assert_eq!(summary.deleted, files.len() as u64);
    assert!(summary.errors.is_empty());
    assert!(files.iter().all(|path| !path.exists()));
    assert!(temp.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_file_roots_bulk_reports_failures() {
    let temp = create_test_dir();
    let ok = temp.join("ok.txt");
    let blocked = temp.join("blocked.txt");
    File::create(&ok).unwrap();
    File::create(&blocked).unwrap();
    let _inject = INJECT_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    set_test_remove_file_failure(Some(blocked.clone()));

    let summary = delete_file_roots_bulk(&[ok.clone(), blocked.clone()], false, false, None);
    set_test_remove_file_failure(None);

    assert_eq!(summary.deleted, 1);
    assert_eq!(summary.errors.len(), 1);
    assert_eq!(summary.errors[0].0, blocked);
    assert!(!ok.exists());
    assert!(blocked.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_file_roots_bulk_rejects_directory_target() {
    let temp = create_test_dir();
    let directory = temp.join("swapped-to-directory");
    fs::create_dir(&directory).unwrap();
    File::create(directory.join("must-survive.txt")).unwrap();

    let summary = delete_file_roots_bulk(std::slice::from_ref(&directory), false, false, None);

    assert_eq!(summary.deleted, 0);
    assert_eq!(summary.errors.len(), 1);
    assert_eq!(summary.errors[0].0, directory);
    assert!(directory.join("must-survive.txt").exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_file_roots_bulk_marks_skipped_paths_cancelled_on_stop() {
    // A stop request must not let skipped paths look like successes:
    // the GUI and the operation journal treat "no error" as deleted.
    let temp = create_test_dir();
    let files: Vec<PathBuf> = (0..64)
        .map(|i| {
            let path = temp.join(format!("file-{i}.txt"));
            File::create(&path).unwrap();
            path
        })
        .collect();

    // Serialize with all other tests that touch the global stop flag.
    let _guard = crate::stop::TEST_FLAG_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    crate::stop::request_stop();
    let summary = delete_file_roots_bulk(&files, false, false, None);
    crate::stop::reset();

    // Every path must be accounted for: deleted + errors == total.
    assert_eq!(
        summary.deleted as usize + summary.errors.len(),
        files.len(),
        "no path may silently disappear from the summary"
    );
    for (_, err) in &summary.errors {
        assert_eq!(err, "cancelled by user");
    }
    // Paths reported as errors must still exist on disk.
    for (path, _) in &summary.errors {
        assert!(path.exists(), "cancelled path {path:?} was deleted");
    }
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_path_handles_file() {
    let temp = create_test_dir();
    let file = temp.join("file.txt");
    File::create(&file).unwrap();
    delete_path(&file, DeleteOptions::default()).unwrap();
    assert!(!file.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_path_handles_directory() {
    let temp = create_test_dir();
    let dir = temp.join("subdir");
    fs::create_dir(&dir).unwrap();
    File::create(dir.join("f.txt")).unwrap();
    delete_path(&dir, DeleteOptions::default()).unwrap();
    assert!(!dir.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_path_refuses_protected_path() {
    let result = delete_path(
        std::path::Path::new(r"C:\Windows"),
        DeleteOptions::default(),
    );
    assert!(result.is_err());
}

#[test]
#[cfg(windows)]
fn test_delete_path_refuses_filesystem_root() {
    let result = delete_path(std::path::Path::new(r"C:\"), DeleteOptions::default());
    assert!(result.is_err());
}

#[test]
fn test_preview_does_not_delete_and_counts_everything() {
    let temp = create_test_dir();
    let file = temp.join("keep.txt");
    fs::write(&file, b"12345").unwrap();
    fs::create_dir(temp.join("sub")).unwrap();
    fs::write(temp.join("sub").join("b.bin"), b"xy").unwrap();
    let stats = preview_path(&temp, &FilterConfig::default(), false).unwrap();
    assert!(file.exists());
    assert_eq!((stats.files, stats.dirs, stats.bytes), (2, 2, 7));
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_preview_matches_what_a_filtered_run_deletes() {
    // Regression: the preview ignored --include/--exclude and reported
    // the whole folder. It must count exactly what the run removes.
    use glob::Pattern;
    let temp = create_test_dir();
    let build = || {
        fs::create_dir_all(temp.join("mixed")).unwrap();
        fs::create_dir_all(temp.join("logs-only").join("deeper")).unwrap();
        fs::write(temp.join("a.log"), b"1234").unwrap();
        fs::write(temp.join("keep.txt"), b"x").unwrap();
        fs::write(temp.join("mixed").join("b.log"), b"12").unwrap();
        fs::write(temp.join("mixed").join("keep2.txt"), b"x").unwrap();
        fs::write(temp.join("logs-only").join("deeper").join("c.log"), b"1").unwrap();
    };
    build();
    let filter = FilterConfig {
        excludes: vec![Pattern::new("*.txt").unwrap()],
        ..Default::default()
    };
    let stats = preview_path(&temp, &filter, false).unwrap();
    // 3 logs (7 bytes); logs-only and logs-only\deeper become empty and
    // go; mixed and the root keep a .txt and stay.
    assert_eq!((stats.files, stats.dirs, stats.bytes), (3, 2, 7));

    delete_path(&temp, DeleteOptions::default().with_filter(filter).silent()).unwrap();
    assert!(!temp.join("logs-only").exists());
    assert!(temp.join("mixed").join("keep2.txt").exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_preview_applies_filters_to_top_level_files() {
    let temp = create_test_dir();
    let small = temp.join("small.bin");
    fs::write(&small, b"123").unwrap();
    let filter = FilterConfig {
        min_size: Some(100),
        ..Default::default()
    };
    assert_eq!(
        preview_path(&small, &filter, false).unwrap(),
        TreeStats::default()
    );
    assert_eq!(
        preview_path(&small, &FilterConfig::default(), false).unwrap(),
        TreeStats::file(3)
    );
    let _ = fs::remove_dir_all(&temp);
}

#[test]
#[cfg(windows)]
fn test_preview_refuses_filesystem_root_and_protected_paths() {
    let err = preview_path(Path::new(r"C:\"), &FilterConfig::default(), true).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let err = preview_path(Path::new(r"C:\Windows"), &FilterConfig::default(), false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn test_delete_readonly_file_with_retry() {
    let temp = create_test_dir();
    let file = temp.join("ro.txt");
    File::create(&file).unwrap();
    let mut perms = fs::metadata(&file).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&file, perms).unwrap();
    remove_file_with_retry(&file).unwrap();
    assert!(!file.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_path_nonexistent_returns_error() {
    let temp = create_test_dir();
    let nonexistent = temp.join("does-not-exist");
    assert!(delete_path(&nonexistent, DeleteOptions::default()).is_err());
    assert!(preview_path(&nonexistent, &FilterConfig::default(), false).is_err());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_remove_dir_with_retry_handles_readonly_dir() {
    let temp = create_test_dir();
    let readonly_dir = temp.join("readonly_dir");
    fs::create_dir(&readonly_dir).unwrap();
    let mut perms = fs::metadata(&readonly_dir).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&readonly_dir, perms).unwrap();
    remove_dir_with_retry(&readonly_dir).unwrap();
    assert!(!readonly_dir.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
#[cfg(windows)]
fn test_delete_directory_does_not_follow_junction() {
    let temp = create_test_dir();
    let delete_root = temp.join("delete-root");
    let external_target = temp.join("external-target");
    fs::create_dir(&delete_root).unwrap();
    fs::create_dir(&external_target).unwrap();
    File::create(external_target.join("keep.txt")).unwrap();
    let junction = delete_root.join("junction");
    let status = std::process::Command::new("cmd")
        .args([
            "/c",
            "mklink",
            "/J",
            junction.to_str().unwrap(),
            external_target.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(status.status.success());
    delete_directory(&delete_root, None).unwrap();
    assert!(!delete_root.exists());
    assert!(external_target.exists());
    assert!(external_target.join("keep.txt").exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_directory_handles_very_deep_tree() {
    // Deeper than tree_delete's parallel depth limit: exercises the
    // iterative fallback, and with 8-char names exceeds MAX_PATH.
    let temp = create_test_dir();
    let mut current = temp.clone();
    for i in 0..120 {
        current = current.join(format!("lvl{i:05}"));
    }
    fs::create_dir_all(&current).unwrap();
    fs::write(current.join("leaf.txt"), b"x").unwrap();
    assert!(current.as_os_str().len() > 260);
    delete_directory(&temp, None).unwrap();
    assert!(!temp.exists());
}

#[test]
// Windows semantics: the read-only attribute on a directory does not
// protect its entries. On Unix a non-writable directory does — exactly
// as for `rm -rf` — so this case is Windows-only.
#[cfg(windows)]
fn test_delete_directory_large_flat_dir_and_readonly_subdirs() {
    let temp = create_test_dir();
    let flat = temp.join("flat");
    fs::create_dir(&flat).unwrap();
    for i in 0..1000 {
        fs::write(flat.join(format!("f{i}.bin")), b"x").unwrap();
    }
    let ro_dir = temp.join("ro-dir");
    fs::create_dir(&ro_dir).unwrap();
    let ro_file = ro_dir.join("ro.txt");
    fs::write(&ro_file, b"x").unwrap();
    for p in [&ro_file, &ro_dir] {
        let mut perms = fs::metadata(p).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(p, perms).unwrap();
    }
    delete_directory(&temp, Some(4)).unwrap();
    assert!(!temp.exists());
}

#[test]
#[cfg(windows)]
fn test_delete_directory_does_not_follow_nested_junction() {
    let temp = create_test_dir();
    let delete_root = temp.join("delete-root");
    let nested = delete_root.join("a").join("b");
    let external_target = temp.join("external-target");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir(&external_target).unwrap();
    File::create(external_target.join("keep.txt")).unwrap();
    let junction = nested.join("junction");
    let status = std::process::Command::new("cmd")
        .args([
            "/c",
            "mklink",
            "/J",
            junction.to_str().unwrap(),
            external_target.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(status.status.success());
    delete_directory(&delete_root, None).unwrap();
    assert!(!delete_root.exists());
    assert!(external_target.join("keep.txt").exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_directory_failure_does_not_cascade_to_parents() {
    // One locked file deep in the tree must yield exactly one reported
    // failure, not one per ancestor directory.
    let temp = create_test_dir();
    let deep = temp.join("x").join("y");
    fs::create_dir_all(&deep).unwrap();
    let blocked = deep.join("locked.bin");
    File::create(&blocked).unwrap();
    let _inject = INJECT_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    set_test_remove_file_failure(Some(blocked.clone()));
    let result = delete_directory(&temp, None);
    set_test_remove_file_failure(None);
    let err = result.unwrap_err().to_string();
    assert!(err.starts_with("1 entry could not be deleted"), "{err}");
    assert!(blocked.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_directory_reports_failed_entries_and_keeps_root() {
    let temp = create_test_dir();
    let file = temp.join("blocked.txt");
    let empty = temp.join("empty");
    File::create(&file).unwrap();
    fs::create_dir(&empty).unwrap();
    let _inject = INJECT_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    set_test_remove_file_failure(Some(file.clone()));
    let result = delete_directory(&temp, None);
    set_test_remove_file_failure(None);
    let err = result.unwrap_err();
    assert!(err.to_string().contains("blocked.txt"));
    assert!(err.to_string().contains("Close apps using these files"));
    assert!(temp.exists());
    assert!(!empty.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_delete_path_with_bar_removes_multiple_directories() {
    let base = create_test_dir();
    let dirs: Vec<PathBuf> = (0..3)
        .map(|i| {
            let d = base.join(format!("dir{}", i));
            fs::create_dir(&d).unwrap();
            File::create(d.join("file.txt")).unwrap();
            d
        })
        .collect();
    for d in &dirs {
        let bar = ProgressBar::hidden();
        delete_path(d, DeleteOptions::default().with_bar(bar)).unwrap();
    }
    for d in &dirs {
        assert!(!d.exists());
    }
    assert!(base.exists());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn test_delete_path_in_current_pool_uses_existing_rayon_pool() {
    let temp = create_test_dir();
    for i in 0..64 {
        File::create(temp.join(format!("file{i}.txt"))).unwrap();
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    pool.install(|| {
        delete_path(&temp, DeleteOptions::default().silent()).unwrap();
    });
    assert!(!temp.exists());
}

#[test]
fn test_lock_failures_recovers_from_poisoned_mutex() {
    let failures: Mutex<Vec<DeletionFailure>> = Mutex::new(vec![DeletionFailure {
        path: PathBuf::from("kept"),
        error: "pre-existing".into(),
    }]);
    let _ = std::panic::catch_unwind(|| {
        let _guard = failures.lock().unwrap();
        panic!("simulate task panic while holding the failure buffer");
    });
    assert!(failures.is_poisoned());
    let guard = lock_failures(&failures);
    assert_eq!(guard.len(), 1);
    assert_eq!(guard[0].path, PathBuf::from("kept"));
}

#[test]
fn test_delete_path_empty_dir_uses_fast_path() {
    let base = create_test_dir();
    let empty = base.join("empty-fast-path");
    fs::create_dir(&empty).unwrap();
    delete_path(&empty, DeleteOptions::default()).unwrap();
    assert!(!empty.exists());
    assert!(base.exists());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn test_only_empty_removes_empty_dir_via_delete_path() {
    let temp = create_test_dir();
    let empty = temp.join("empty");
    fs::create_dir(&empty).unwrap();
    delete_path(&empty, DeleteOptions::default().only_empty().silent()).unwrap();
    assert!(!empty.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_only_empty_keeps_non_empty_dir_and_contents() {
    let temp = create_test_dir();
    let dir = temp.join("full");
    fs::create_dir(&dir).unwrap();
    let file = dir.join("keep.txt");
    fs::write(&file, b"data").unwrap();
    // Must succeed (skip) without touching the directory contents.
    delete_path(&dir, DeleteOptions::default().only_empty().silent()).unwrap();
    assert!(dir.exists());
    assert!(file.exists());
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_top_level_file_skipped_when_filter_excludes_it() {
    let temp = create_test_dir();
    let file = temp.join("small.bin");
    fs::write(&file, vec![0u8; 10]).unwrap();
    let filter = FilterConfig {
        min_size: Some(100),
        ..Default::default()
    };
    delete_path(&file, DeleteOptions::default().with_filter(filter).silent()).unwrap();
    assert!(file.exists(), "file below --min-size must be kept");
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_shred_tree_handles_readonly_hardlinks_and_scrubs_names() {
    let temp = create_test_dir();
    let tree = temp.join("tree");
    let sub = tree.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let outside = temp.join("outside.txt");
    fs::write(&outside, b"IMPORTANT DATA").unwrap();
    fs::hard_link(&outside, sub.join("link.txt")).unwrap();
    let ro = sub.join("ro.txt");
    fs::write(&ro, b"secret").unwrap();
    let mut perms = fs::metadata(&ro).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&ro, perms).unwrap();
    for i in 0..300 {
        fs::write(tree.join(format!("f{i}.bin")), b"secret").unwrap();
    }

    delete_path(&tree, DeleteOptions::default().shred().silent()).unwrap();
    assert!(!tree.exists());
    assert_eq!(fs::read(&outside).unwrap(), b"IMPORTANT DATA");
    let leftovers: Vec<_> = fs::read_dir(&temp).unwrap().flatten().collect();
    assert_eq!(leftovers.len(), 1, "only outside.txt may remain");
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_filtered_delete_keeping_files_succeeds() {
    // Regression: kept files made their directories (and the root)
    // fail with "directory not empty", turning every filtered run into
    // an error even though exactly the right files were deleted.
    use glob::Pattern;
    let temp = create_test_dir();
    let sub = temp.join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(temp.join("a.log"), b"x").unwrap();
    fs::write(temp.join("keep.txt"), b"x").unwrap();
    fs::write(sub.join("b.log"), b"x").unwrap();
    fs::write(sub.join("keep2.txt"), b"x").unwrap();

    for filter in [
        FilterConfig {
            includes: vec![Pattern::new("*.log").unwrap()],
            ..Default::default()
        },
        FilterConfig {
            excludes: vec![Pattern::new("*.txt").unwrap()],
            ..Default::default()
        },
    ] {
        delete_path(&temp, DeleteOptions::default().with_filter(filter).silent()).unwrap();
        assert!(!temp.join("a.log").exists());
        assert!(!sub.join("b.log").exists());
        assert!(temp.join("keep.txt").exists());
        assert!(sub.join("keep2.txt").exists());
        fs::write(temp.join("a.log"), b"x").unwrap();
        fs::write(sub.join("b.log"), b"x").unwrap();
    }
    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_filtered_delete_removes_root_when_nothing_is_kept() {
    use glob::Pattern;
    let temp = create_test_dir();
    fs::create_dir(temp.join("sub")).unwrap();
    fs::write(temp.join("sub").join("x.tmp"), b"x").unwrap();
    let filter = FilterConfig {
        excludes: vec![Pattern::new("*.keep").unwrap()],
        ..Default::default()
    };
    delete_path(&temp, DeleteOptions::default().with_filter(filter).silent()).unwrap();
    assert!(!temp.exists());
}

#[test]
fn test_top_level_file_deleted_when_filter_matches() {
    let temp = create_test_dir();
    let file = temp.join("big.bin");
    fs::write(&file, vec![0u8; 200]).unwrap();
    let filter = FilterConfig {
        min_size: Some(100),
        ..Default::default()
    };
    delete_path(&file, DeleteOptions::default().with_filter(filter).silent()).unwrap();
    assert!(!file.exists(), "file matching --min-size must be deleted");
    let _ = fs::remove_dir_all(&temp);
}
