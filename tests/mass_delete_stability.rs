use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use zap::delete::{delete_path, DeleteOptions};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn create_test_dir() -> PathBuf {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("zap-mass-delete-stability-{pid}-{id}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn deletes_thousands_of_files_stably() {
    let root = create_test_dir();

    // Keep this large enough to catch regressions for Explorer multi-select
    // deletes and small enough to stay reliable on slower CI runners.
    const DIRS: usize = 30;
    const FILES_PER_DIR: usize = 100;

    for dir_idx in 0..DIRS {
        let dir = root.join(format!("bucket-{dir_idx:03}"));
        fs::create_dir(&dir).unwrap();

        for file_idx in 0..FILES_PER_DIR {
            fs::write(dir.join(format!("file-{file_idx:03}.tmp")), b"zap").unwrap();
        }
    }

    let started = Instant::now();
    delete_path(&root, DeleteOptions::default().silent()).unwrap();
    let elapsed = started.elapsed();

    assert!(!root.exists(), "mass-delete root should be removed");

    // This is deliberately generous; it is a regression guard, not a benchmark.
    assert!(
        elapsed.as_secs() < 20,
        "deleting {DIRS}x{FILES_PER_DIR} files took too long: {elapsed:?}"
    );
}
