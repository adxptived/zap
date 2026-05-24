#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use zap::{batch, cli, delete};

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
    let paths_dir = temp.join(batch::PATHS_DIR_NAME);
    let lock_file = temp.join(batch::LOCK_FILE_NAME);

    batch::cleanup_stale_batch(&paths_dir, &lock_file);
    if batch::write_batch_paths(&paths_dir, paths).is_err() {
        return Some(paths.to_vec());
    }

    match batch::try_acquire_lock(&lock_file) {
        Ok(lock) => {
            batch::wait_for_batch_quiet(
                &paths_dir,
                batch::BATCH_QUIET_POLLS,
                batch::BATCH_MAX_POLLS,
            );
            drop(lock);
            let mut collected = batch::read_batch_paths(&paths_dir);
            dedup_paths(&mut collected);
            let _ = fs::remove_dir_all(&paths_dir);
            let _ = fs::remove_file(&lock_file);
            Some(collected)
        }
        Err(_) => None,
    }
}

fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::with_capacity(paths.len());
    paths.retain(|path| seen.insert(path.clone()));
}
