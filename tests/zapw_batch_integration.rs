#![cfg(windows)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn unique_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn zapw_batch_deletes_50_explorer_style_processes() {
    let data_root = unique_root("zapw-batch-data");
    let batch_root = unique_root("zapw-batch-state");
    fs::create_dir_all(&data_root).unwrap();
    fs::create_dir_all(&batch_root).unwrap();

    let mut files = Vec::new();
    for i in 0..50 {
        let path = data_root.join(format!("item-{i:02}.txt"));
        fs::write(&path, format!("item {i}")).unwrap();
        files.push(path);
    }

    let exe = env!("CARGO_BIN_EXE_zapw");
    let mut children = Vec::new();
    for path in &files {
        children.push(
            Command::new(exe)
                .env("ZAP_BATCH_ROOT", &batch_root)
                .args(["--batch", "--silent", "--yes"])
                .arg(path)
                .spawn()
                .unwrap(),
        );
    }

    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && files.iter().any(|p| p.exists()) {
        thread::sleep(Duration::from_millis(100));
    }

    let remaining: Vec<_> = files.iter().filter(|p| p.exists()).collect();
    assert!(remaining.is_empty(), "remaining files: {remaining:?}");

    let _ = fs::remove_dir_all(&data_root);
    let _ = fs::remove_dir_all(&batch_root);
}

#[test]
fn zapw_batch_deletes_50_processes_with_slow_explorer_launch() {
    let data_root = unique_root("zapw-batch-slow-data");
    let batch_root = unique_root("zapw-batch-slow-state");
    fs::create_dir_all(&data_root).unwrap();
    fs::create_dir_all(&batch_root).unwrap();

    let mut files = Vec::new();
    for i in 0..50 {
        let path = data_root.join(format!("slow-{i:02}.txt"));
        fs::write(&path, format!("slow item {i}")).unwrap();
        files.push(path);
    }

    let exe = env!("CARGO_BIN_EXE_zapw");
    for path in &files {
        let status = Command::new(exe)
            .env("ZAP_BATCH_ROOT", &batch_root)
            .args(["--batch", "--silent", "--yes"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
        thread::sleep(Duration::from_millis(20));
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && files.iter().any(|p| p.exists()) {
        thread::sleep(Duration::from_millis(100));
    }

    let remaining: Vec<_> = files.iter().filter(|p| p.exists()).collect();
    assert!(remaining.is_empty(), "remaining files: {remaining:?}");

    let _ = fs::remove_dir_all(&data_root);
    let _ = fs::remove_dir_all(&batch_root);
}
