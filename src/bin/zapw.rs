#![cfg_attr(windows, windows_subsystem = "windows")]

use rayon::prelude::*;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use zap::journal::{self, JournalAction, PathOutcome};
use zap::settings::Language;
use zap::{batch, cli, delete, path_utils};

/// Internal flag used to distinguish the detached background worker
/// from the initial Explorer-launched process.
const WORKER_FLAG: &str = "--zapw-worker";
const BATCH_WRITTEN_FLAG: &str = "--zapw-batch-written";
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);
const WORKER_QUIET_POLLS: usize = 20; // 500 ms after Explorer stops adding paths
const WORKER_MAX_POLLS: usize = 120; // 3 s cap if Explorer keeps launching items

fn main() {
    zap::parallelism::init_global_pool();
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

    let scope = options.batch_scope("zapw");
    let paths_dir = batch::batch_paths_dir(&scope);
    let lock_file = batch::batch_lock_file(&scope);

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

    // Before the adjustments below: must match the launcher's scope.
    let scope = options.batch_scope("zapw");
    options.silent = true;

    // A windowless worker cannot show a confirmation prompt. Without an
    // explicit --yes the safest behavior is a silent dry-run no-op.
    if options.needs_confirm {
        options.dry_run = true;
    }

    let settings = zap::settings::Settings::load();
    if !settings.journal {
        options.no_journal = true;
    }
    if options.threads.is_none() {
        options.threads = settings.threads;
    }

    let mut batch_run = None;

    if options.batch {
        let run = if batch_already_written {
            collect_existing_batch_paths(&scope)
        } else {
            collect_batch_paths(&scope, &options.paths)
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

    let mut failures = run_silent_delete_collect(&options);

    if let Some(run) = batch_run {
        failures.extend(drain_late_batch_paths(&options, run));
    }

    if settings.notify_failures && !options.dry_run && !failures.is_empty() {
        notify_failures(&failures, settings.language.resolve(), options.recycle);
    }
}

/// Windowless runs have no other way to report errors: without this a
/// locked file made "Delete permanently" silently do nothing.
fn notify_failures(failures: &[(PathBuf, String)], lang: Language, recycle: bool) {
    const SHOWN: usize = 8;
    let ru = lang == Language::Russian;
    let mut text = if ru {
        format!(
            "Не удалось {} ({}):\n\n",
            if recycle {
                "переместить в корзину"
            } else {
                "удалить"
            },
            failures.len()
        )
    } else {
        format!(
            "Zap could not {} {} item(s):\n\n",
            if recycle { "recycle" } else { "delete" },
            failures.len()
        )
    };
    for (path, err) in failures.iter().take(SHOWN) {
        // The first line of the error is enough; details are in the journal.
        let first_line = err.lines().next().unwrap_or_default();
        text.push_str(&format!(
            "• {}\n   {}\n",
            path.display(),
            zap::messages::localize(first_line, lang)
        ));
    }
    if failures.len() > SHOWN {
        let more = failures.len() - SHOWN;
        text.push_str(&if ru {
            format!("… и ещё {more}\n")
        } else {
            format!("… and {more} more\n")
        });
    }
    text.push_str(if ru {
        "\nЗакройте программы, использующие эти файлы, и повторите."
    } else {
        "\nClose programs that use these files, then try again."
    });
    show_message_box(&text, "Zap");
}

#[cfg(windows)]
fn show_message_box(text: &str, caption: &str) {
    extern "system" {
        fn MessageBoxW(
            hwnd: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            kind: u32,
        ) -> i32;
    }
    const MB_ICONWARNING: u32 = 0x30;
    const MB_SETFOREGROUND: u32 = 0x1_0000;
    const MB_TOPMOST: u32 = 0x4_0000;
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (text, caption) = (wide(text), wide(caption));
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

#[cfg(not(windows))]
fn show_message_box(text: &str, _caption: &str) {
    eprintln!("{text}");
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

#[cfg(test)]
fn run_silent_delete(options: &cli::CliOptions) -> bool {
    run_silent_delete_collect(options).is_empty()
}

/// Delete, journal, and return the failed paths with their errors.
fn run_silent_delete_collect(options: &cli::CliOptions) -> Vec<(PathBuf, String)> {
    let outcomes = run_silent_delete_outcomes(options);
    record_journal(options, &outcomes);
    outcomes
        .into_iter()
        .filter_map(|(path, err)| err.map(|err| (path, err)))
        .collect()
}

/// Delete all paths, returning a per-path outcome (`None` = success) so the
/// operation journal can record exactly what happened. This is the only
/// deletion entry point in zapw — context-menu deletions were previously
/// the one surface that never journaled.
fn run_silent_delete_outcomes(options: &cli::CliOptions) -> Vec<PathOutcome> {
    // Recycle mode: one SHFileOperationW call for the whole batch — single
    // shell roundtrip and a single Explorer "Undo" entry.
    #[cfg(windows)]
    if options.recycle && !options.dry_run {
        let errors = delete::recycle_paths_validated(&options.paths, false);
        let error_index: std::collections::HashMap<&Path, String> = errors
            .iter()
            .map(|(path, err)| (path.as_path(), err.to_string()))
            .collect();
        return options
            .paths
            .iter()
            .map(|path| (path.clone(), error_index.get(path.as_path()).cloned()))
            .collect();
    }

    let delete_one = |path: &Path| -> PathOutcome {
        let result = if options.dry_run {
            delete::preview_path(path, &options.filter, false).map(|_| ())
        } else {
            // Build options via `to_delete_options` so recycle/shred/
            // only-empty/filters survive: constructing DeleteOptions
            // manually here used to drop them, which made the context-menu
            // "Move to Recycle Bin" entry delete permanently.
            delete::delete_path(path, options.to_delete_options(false, false).silent())
        };
        (path.to_path_buf(), result.err().map(|err| err.to_string()))
    };

    if !should_parallelize_top_level(options) {
        return options.paths.iter().map(|p| delete_one(p)).collect();
    }

    options.paths.par_iter().map(|p| delete_one(p)).collect()
}

/// Mirror of the CLI journaling: best-effort, skipped for dry runs and when
/// disabled by flag or environment.
fn record_journal(options: &cli::CliOptions, outcomes: &[PathOutcome]) {
    if options.dry_run || options.no_journal || journal::is_disabled_by_env() {
        return;
    }
    let action = if options.recycle {
        JournalAction::Recycle
    } else if options.shred {
        JournalAction::Shred
    } else {
        JournalAction::Delete
    };
    let _ = journal::record(action, outcomes);
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
    /// The scope no longer belongs to this run (never owned, or already
    /// released): the lock file and directory may be a new leader's now
    /// and must not be touched.
    released: bool,
}

/// Returns `Some(run)` for the leader, `None` for followers.
/// The lock handle must be kept alive until deletion is complete.
fn collect_batch_paths(scope: &str, paths: &[PathBuf]) -> Option<BatchRun> {
    let paths_dir = batch::batch_paths_dir(scope);
    let lock_file = batch::batch_lock_file(scope);

    batch::cleanup_stale_batch(&paths_dir, &lock_file);
    if batch::write_batch_paths(&paths_dir, paths).is_err() {
        // Fallback: can't write to batch dir, just delete our own paths
        return Some(BatchRun {
            paths: paths.to_vec(),
            paths_dir,
            lock_file,
            lock: None,
            released: true,
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
                released: false,
            })
        }
        Err(_) => None,
    }
}

/// Called by the detached worker (--zapw-batch-written path). The worker
/// adopts the lock that was created by the short-lived Explorer process,
/// refreshing the PID and mtime so `cleanup_stale_batch` doesn't kill an
/// active batch.
fn collect_existing_batch_paths(scope: &str) -> Option<BatchRun> {
    let paths_dir = batch::batch_paths_dir(scope);
    let lock_file = batch::batch_lock_file(scope);

    wait_for_batch_quiet_fast(&paths_dir);
    let mut collected = batch::read_batch_paths(&paths_dir);
    path_utils::dedup_paths(&mut collected);
    if collected.is_empty() {
        // An older leader already claimed the launcher's paths. Release the
        // lock the launcher created — left behind, it would make every
        // launch in the next 30 s hand its paths to nobody.
        let mut rest = batch::release_and_collect(&paths_dir, &lock_file, None);
        path_utils::dedup_paths(&mut rest);
        return (!rest.is_empty()).then_some(BatchRun {
            paths: rest,
            paths_dir,
            lock_file,
            lock: None,
            released: true,
        });
    }

    let lock = batch::refresh_lock_owner(&lock_file).ok();

    Some(BatchRun {
        paths: collected,
        paths_dir,
        lock_file,
        lock,
        released: false,
    })
}

fn drain_late_batch_paths(options: &cli::CliOptions, mut run: BatchRun) -> Vec<(PathBuf, String)> {
    let mut processed: HashSet<PathBuf> = options.paths.iter().cloned().collect();
    let mut failures = Vec::new();

    while !run.released {
        let mut pending = {
            if let Some(lock) = run.lock.as_mut() {
                batch::touch_lock(lock);
            }
            wait_for_batch_quiet_fast(&run.paths_dir);
            batch::read_batch_paths(&run.paths_dir)
        };
        if pending.is_empty() {
            // Quiet: hand the scope over. Paths committed before the lock
            // disappears are still ours; later launches lead themselves.
            pending = batch::release_and_collect(&run.paths_dir, &run.lock_file, run.lock.take());
            run.released = true;
        }
        path_utils::dedup_paths(&mut pending);
        pending.retain(|path| processed.insert(path.clone()));
        if pending.is_empty() {
            continue;
        }

        let mut late_options = options.clone();
        late_options.paths = pending;
        late_options.batch = false;
        failures.extend(run_silent_delete_collect(&late_options));
    }
    failures
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
            needs_confirm: false,
            threads: None,
            paths,
            batch: false,
            silent: true,
            filter: FilterConfig::default(),
            shred: false,
            only_empty: false,
            recycle: false,
            no_size_preview: false,
            no_journal: true,
            newer_than_spec: None,
            older_than_spec: None,
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
        // Keeping filtered-out files is the point, not a failure.
        assert!(run_silent_delete(&options));

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
                released: false,
            },
        );

        assert!(initial.exists());
        assert!(!late.exists());
        assert!(!paths_dir.exists());
        assert!(!lock_file.exists());

        let _ = fs::remove_dir_all(&root);
    }
}
