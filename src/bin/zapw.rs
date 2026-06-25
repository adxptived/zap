#![cfg_attr(windows, windows_subsystem = "windows")]

use rayon::prelude::*;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use zap::{batch, cli, delete, path_utils};

/// Internal flag used to distinguish the detached background worker
/// from the initial Explorer-launched process.
const WORKER_FLAG: &str = "--zapw-worker";
const BATCH_WRITTEN_FLAG: &str = "--zapw-batch-written";
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);
const WORKER_QUIET_POLLS: usize = 20; // 500 ms after Explorer stops adding paths
const WORKER_MAX_POLLS: usize = 120; // 3 s cap if Explorer keeps launching items

fn main() {
    let raw_args: Vec<OsString> = std::env::args_os().skip(1).collect();

    // If --zapw-worker is present, we ARE the background worker -- do the real work.
    if raw_args.iter().any(|a| a == WORKER_FLAG) {
        let batch_already_written = raw_args.iter().any(|a| a == BATCH_WRITTEN_FLAG);
        let args: Vec<OsString> = raw_args.into_iter().filter(|a| a != WORKER_FLAG).collect();
        let args: Vec<OsString> = args
            .into_iter()
            .filter(|a| a != BATCH_WRITTEN_FLAG)
            .collect();
        worker_main(args, batch_already_written);
        return;
    }

    // Otherwise we are the Explorer-launched process. For batched Explorer
    // invokes, write this path before spawning the leader worker; this prevents
    // the worker from deleting early paths before Explorer has launched all verbs.
    if try_start_batch_leader(&raw_args) {
        return;
    }

    let _ = relaunch_detached(&raw_args, false);
}

/// Spawn ourselves with --zapw-worker as a fully detached process.
/// The original process exits in < 10 ms -- Explorer is unblocked immediately.
fn relaunch_detached(args: &[OsString], batch_already_written: bool) -> bool {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return false,
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(WORKER_FLAG);
    if batch_already_written {
        cmd.arg(BATCH_WRITTEN_FLAG);
    }
    cmd.args(args);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    cmd.spawn().is_ok()
}

fn try_start_batch_leader(raw_args: &[OsString]) -> bool {
    let action = match cli::parse_args(raw_args.iter().cloned()) {
        Ok(action) => action,
        Err(_) => return false,
    };
    let cli::CliAction::Run(options) = action else {
        return false;
    };
    if !options.batch {
        return false;
    }

    let paths_dir = batch::batch_paths_dir();
    let lock_file = batch::batch_lock_file();

    batch::cleanup_stale_batch(&paths_dir, &lock_file);
    if batch::write_batch_paths(&paths_dir, &options.paths).is_err() {
        return false;
    }

    match batch::try_acquire_lock(&lock_file) {
        Ok(lock) => {
            drop(lock);
            if relaunch_detached(raw_args, true) {
                true
            } else {
                let _ = fs::remove_file(lock_file);
                false
            }
        }
        Err(_) => true,
    }
}

/// The actual worker logic -- runs in a fully detached process.
fn worker_main(args: Vec<OsString>, batch_already_written: bool) {
    let action = match cli::parse_args(args) {
        Ok(action) => action,
        Err(_) => return,
    };

    let cli::CliAction::Run(mut options) = action else {
        return;
    };

    options.silent = true;

    let mut batch_run = None;

    if options.batch {
        let run = if batch_already_written {
            collect_existing_batch_paths()
        } else {
            collect_batch_paths(&options.paths)
        };
        match run {
            Some(mut run) => {
                options.paths = std::mem::take(&mut run.paths);
                options.batch = false;
                batch_run = Some(run);
            }
            None => return, // follower -- another worker is the leader
        }
    }

    let _ = run_silent_delete(&options);

    if let Some(run) = batch_run {
        drain_late_batch_paths(&options, run);
    }
}

/// Whether top-level paths should be deleted in parallel via `par_iter`.
/// Only when: all paths are non-directory files, more than one path, and
/// no explicit thread override. Directories already use internal Rayon
/// parallelism; parallelizing top-level directories would oversubscribe.
fn should_parallelize_top_level(options: &cli::CliOptions) -> bool {
    options.threads.is_none()
        && options.paths.len() > 1
        && options
            .paths
            .iter()
            .all(|path| fs::symlink_metadata(path).is_ok_and(|m| !m.is_dir()))
}

fn run_silent_delete(options: &cli::CliOptions) -> bool {
    // Recycle mode: one SHFileOperationW call for the whole batch — single
    // shell roundtrip and a single Explorer "Undo" entry.
    #[cfg(windows)]
    if options.recycle && !options.dry_run {
        return delete::recycle_paths_validated(&options.paths, false).is_empty();
    }

    let delete_one = |path: &Path| {
        if options.dry_run {
            delete::dry_run_path_silent(path)
        } else {
            // Build options via `to_delete_options` so recycle/shred/
            // only-empty/filters survive: constructing DeleteOptions
            // manually here used to drop them, which made the context-menu
            // "Move to Recycle Bin" entry delete permanently.
            delete::delete_path(path, options.to_delete_options(false, false).silent())
        }
    };

    if !should_parallelize_top_level(options) {
        return options.paths.iter().all(|path| delete_one(path).is_ok());
    }

    options
        .paths
        .par_iter()
        .map(|path| delete_one(path).is_ok())
        .reduce(|| true, |ok, item_ok| ok && item_ok)
}

struct BatchRun {
    paths: Vec<PathBuf>,
    paths_dir: PathBuf,
    lock_file: PathBuf,
    /// Active lock handle. The leader keeps this alive for the lifetime
    /// of deletion to prevent `cleanup_stale_batch` from clearing state
    /// from underneath a long-running batch. The file mtime is refreshed
    /// by `touch_lock` in the drain loop.
    lock: Option<File>,
}

/// Returns `Some(run)` for the leader, `None` for followers.
/// The lock handle must be kept alive until deletion is complete.
fn collect_batch_paths(paths: &[PathBuf]) -> Option<BatchRun> {
    let paths_dir = batch::batch_paths_dir();
    let lock_file = batch::batch_lock_file();

    batch::cleanup_stale_batch(&paths_dir, &lock_file);
    if batch::write_batch_paths(&paths_dir, paths).is_err() {
        // Fallback: can't write to batch dir, just delete our own paths
        return Some(BatchRun {
            paths: paths.to_vec(),
            paths_dir,
            lock_file,
            lock: None,
        });
    }

    match batch::try_acquire_lock(&lock_file) {
        Ok(lock) => {
            wait_for_batch_quiet_fast(&paths_dir);
            let mut collected = batch::read_batch_paths(&paths_dir);
            path_utils::dedup_paths(&mut collected);
            Some(BatchRun {
                paths: collected,
                paths_dir,
                lock_file,
                lock: Some(lock),
            })
        }
        Err(_) => None,
    }
}

/// Called by the detached worker (--zapw-batch-written path). The worker
/// adopts the lock that was created by the short-lived Explorer process,
/// refreshing the PID and mtime so `cleanup_stale_batch` doesn't kill an
/// active batch.
fn collect_existing_batch_paths() -> Option<BatchRun> {
    let paths_dir = batch::batch_paths_dir();
    let lock_file = batch::batch_lock_file();

    wait_for_batch_quiet_fast(&paths_dir);
    let mut collected = batch::read_batch_paths(&paths_dir);
    path_utils::dedup_paths(&mut collected);
    if collected.is_empty() {
        return None;
    }

    let lock = batch::refresh_lock_owner(&lock_file).ok();

    Some(BatchRun {
        paths: collected,
        paths_dir,
        lock_file,
        lock,
    })
}

fn drain_late_batch_paths(options: &cli::CliOptions, run: BatchRun) {
    let mut processed: HashSet<PathBuf> = options.paths.iter().cloned().collect();

    loop {
        wait_for_batch_quiet_fast(&run.paths_dir);
        let mut pending = batch::read_batch_paths(&run.paths_dir);
        path_utils::dedup_paths(&mut pending);
        pending.retain(|path| !processed.contains(path));

        if pending.is_empty() {
            break;
        }

        let mut late_options = options.clone();
        late_options.paths = pending.clone();
        late_options.batch = false;
        let _ = run_silent_delete(&late_options);
        processed.extend(pending);
    }

    drop(run.lock);
    let _ = fs::remove_dir_all(&run.paths_dir);
    let _ = fs::remove_file(&run.lock_file);
}

fn wait_for_batch_quiet_fast(paths_dir: &Path) {
    let mut last_state = batch::batch_state(paths_dir);
    let mut quiet_polls = 0;

    for _ in 0..WORKER_MAX_POLLS {
        thread::sleep(WORKER_POLL_INTERVAL);
        let current_state = batch::batch_state(paths_dir);
        if current_state == last_state {
            quiet_polls += 1;
            if quiet_polls >= WORKER_QUIET_POLLS {
                break;
            }
        } else {
            quiet_polls = 0;
            last_state = current_state;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zap::filter::FilterConfig;

    fn test_options(paths: Vec<PathBuf>) -> cli::CliOptions {
        cli::CliOptions {
            dry_run: false,
            threads: None,
            paths,
            batch: false,
            silent: true,
            filter: FilterConfig::default(),
            shred: false,
            only_empty: false,
            recycle: false,
            no_size_preview: false,
        }
    }

    #[test]
    fn test_batch_wait_constants_keep_context_menu_latency_low() {
        assert_eq!(WORKER_POLL_INTERVAL, Duration::from_millis(25));
        assert_eq!(WORKER_QUIET_POLLS, 20);
        assert!(WORKER_POLL_INTERVAL * WORKER_QUIET_POLLS as u32 <= Duration::from_millis(500));
        assert!(WORKER_POLL_INTERVAL * WORKER_MAX_POLLS as u32 <= Duration::from_secs(3));
    }

    #[test]
    fn test_should_parallelize_top_level_only_for_multiple_files_without_thread_override() {
        let root = std::env::temp_dir().join(format!(
            "zapw-classify-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let first_file = root.join("first.txt");
        let second_file = root.join("second.txt");
        let dir = root.join("dir");
        fs::write(&first_file, b"first").unwrap();
        fs::write(&second_file, b"second").unwrap();
        fs::create_dir(&dir).unwrap();

        let mut file_options = test_options(vec![first_file.clone(), second_file.clone()]);
        assert!(should_parallelize_top_level(&file_options));

        let mixed_options = test_options(vec![first_file, dir]);
        assert!(!should_parallelize_top_level(&mixed_options));

        file_options.threads = Some(2);
        assert!(!should_parallelize_top_level(&file_options));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_run_silent_delete_honors_filters() {
        let root = std::env::temp_dir().join(format!(
            "zapw-filter-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("keep.txt"), b"keep").unwrap();
        fs::write(root.join("drop.log"), b"drop").unwrap();

        let mut options = test_options(vec![root.clone()]);
        options.filter = FilterConfig {
            includes: vec![glob::Pattern::new("*.log").unwrap()],
            ..FilterConfig::default()
        };
        let _ = run_silent_delete(&options);

        // Regression: zapw used to drop the filter (and recycle/shred flags)
        // by building DeleteOptions manually instead of via to_delete_options.
        assert!(
            root.join("keep.txt").exists(),
            "filtered-out file must survive"
        );
        assert!(!root.join("drop.log").exists(), "matching file is deleted");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_run_silent_delete_handles_multiple_top_level_files() {
        let root = std::env::temp_dir().join(format!(
            "zapw-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let first = root.join("first.txt");
        let second = root.join("second.txt");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();

        assert!(run_silent_delete(&test_options(vec![
            first.clone(),
            second.clone()
        ])));
        assert!(!first.exists());
        assert!(!second.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_drain_late_batch_paths_processes_pending_without_lock_handle() {
        let root = std::env::temp_dir().join(format!(
            "zapw-drain-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths_dir = root.join("paths");
        let lock_file = root.join("lock");
        fs::create_dir_all(&paths_dir).unwrap();
        File::create(&lock_file).unwrap();

        let initial = root.join("initial.txt");
        let late = root.join("late.txt");
        fs::write(&initial, b"initial").unwrap();
        fs::write(&late, b"late").unwrap();
        batch::write_batch_paths(&paths_dir, &[initial.clone(), late.clone()]).unwrap();

        let options = test_options(vec![initial.clone()]);
        drain_late_batch_paths(
            &options,
            BatchRun {
                paths: Vec::new(),
                paths_dir: paths_dir.clone(),
                lock_file: lock_file.clone(),
                lock: None,
            },
        );

        assert!(initial.exists());
        assert!(!late.exists());
        assert!(!paths_dir.exists());
        assert!(!lock_file.exists());

        let _ = fs::remove_dir_all(&root);
    }
}
