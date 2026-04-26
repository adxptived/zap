#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zap::{cli, delete};

const BATCH_POLL_INTERVAL: Duration = Duration::from_millis(50);
const BATCH_QUIET_POLLS: usize = 10;
const BATCH_MAX_POLLS: usize = 120;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let action = match cli::parse_args(args) {
        Ok(action) => action,
        Err(_) => return,
    };

    let cli::CliAction::Run(mut options) = action else {
        return;
    };

    options.silent = true;
    if options.batch {
        options.paths = match collect_batch_paths(&options.paths) {
            Some(paths) => paths,
            None => return,
        };
        options.batch = false;
    }

    let _ = run_silent_delete(&options);
}

fn run_silent_delete(options: &cli::CliOptions) -> bool {
    let delete_one = |path: &Path| {
        if options.dry_run {
            delete::dry_run_path_silent(path)
        } else {
            delete::delete_path(
                path,
                delete::DeleteOptions::default()
                    .with_threads(options.threads)
                    .silent(),
            )
        }
    };

    let mut ok = true;
    for path in &options.paths {
        ok = delete_one(path).is_ok() && ok;
    }
    ok
}

fn collect_batch_paths(paths: &[PathBuf]) -> Option<Vec<PathBuf>> {
    let temp = std::env::temp_dir();
    let paths_dir = temp.join("zapw-batch-paths.d");
    let lock_file = temp.join("zapw-batch.lock");

    cleanup_stale_batch(&paths_dir, &lock_file);
    if write_batch_paths(&paths_dir, paths).is_err() {
        return Some(paths.to_vec());
    }

    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_file)
    {
        Ok(lock) => {
            wait_for_batch_quiet(&paths_dir);
            drop(lock);
            let mut collected = read_batch_paths(&paths_dir);
            dedup_paths(&mut collected);
            let _ = fs::remove_dir_all(&paths_dir);
            let _ = fs::remove_file(&lock_file);
            Some(collected)
        }
        Err(_) => None,
    }
}

fn cleanup_stale_batch(paths_dir: &Path, lock_file: &Path) {
    if let Ok(meta) = fs::metadata(lock_file) {
        if let Ok(modified) = meta.modified() {
            if modified.elapsed().unwrap_or_default() > Duration::from_secs(10) {
                let _ = fs::remove_dir_all(paths_dir);
                let _ = fs::remove_file(lock_file);
            }
        }
    }
}

fn write_batch_paths(paths_dir: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
    fs::create_dir_all(paths_dir)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();

    for attempt in 0..100_u32 {
        let file = paths_dir.join(format!("{nanos}-{pid}-{attempt}.txt"));
        match OpenOptions::new().create_new(true).write(true).open(file) {
            Ok(mut file) => {
                for path in paths {
                    writeln!(file, "{}", path.display())?;
                }
                return file.flush();
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a unique batch path file",
    ))
}

fn wait_for_batch_quiet(paths_dir: &Path) {
    let mut last_state = batch_state(paths_dir);
    let mut quiet_polls = 0;

    for _ in 0..BATCH_MAX_POLLS {
        thread::sleep(BATCH_POLL_INTERVAL);
        let state = batch_state(paths_dir);
        if state == last_state {
            quiet_polls += 1;
            if quiet_polls >= BATCH_QUIET_POLLS {
                break;
            }
        } else {
            quiet_polls = 0;
            last_state = state;
        }
    }
}

fn batch_state(paths_dir: &Path) -> (usize, u64) {
    fs::read_dir(paths_dir)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then_some(meta.len())
        })
        .fold((0, 0), |(count, bytes), len| (count + 1, bytes + len))
}

fn read_batch_paths(paths_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(paths_dir)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| entry.path())
        })
        .collect();
    files.sort();

    files
        .into_iter()
        .flat_map(|file| {
            fs::read_to_string(file)
                .unwrap_or_default()
                .lines()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::with_capacity(paths.len());
    paths.retain(|path| seen.insert(path.clone()));
}
