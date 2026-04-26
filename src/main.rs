/*
  Copyright 2022 Tejas Ravishankar

  Licensed under the Apache License, Version 2.0 (the "License");
  you may not use this file except in compliance with the License.
  You may obtain a copy of the License at

      http://www.apache.org/licenses/LICENSE-2.0

  Unless required by applicable law or agreed to in writing, software
  distributed under the License is distributed on an "AS IS" BASIS,
  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
  See the License for the specific language governing permissions and
  limitations under the License.
*/

use indicatif::MultiProgress;
use owo_colors::{AnsiColors, OwoColorize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zap::{cli, delete, protect};

static CONSOLE_ALLOCATED: AtomicBool = AtomicBool::new(false);

fn main() {
    let start = Instant::now();

    let action = cli::parse_args(std::env::args_os().skip(1)).unwrap_or_else(|err| {
        cli::print_error(&err);
        cli::print_help();
        std::process::exit(1);
    });

    let success = match action {
        cli::CliAction::PrintHelp => {
            cli::print_help();
            true
        }
        cli::CliAction::PrintVersion => {
            cli::print_version();
            true
        }
        cli::CliAction::Run(options) => {
            if options.batch {
                run_batch(&options, start)
            } else {
                run_delete(&options, start)
            }
        }
    };

    // If we allocated a console (batch leader / late follower), close it
    // so the window disappears automatically after deletion.
    #[cfg(windows)]
    if CONSOLE_ALLOCATED.load(Ordering::SeqCst) {
        free_console();
    }

    if !success {
        std::process::exit(1);
    }
}

const MAX_PARALLEL_DELETES: usize = 5;
const BATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);
const BATCH_INITIAL_QUIET_POLLS: usize = 10;
const BATCH_FINAL_QUIET_POLLS: usize = 30;
const BATCH_MAX_POLLS: usize = 300;

fn report_error(path: &Path, err: &std::io::Error) {
    eprintln!(
        "{} {}: {}",
        " ERROR ".on_color(AnsiColors::BrightRed).black(),
        path.display().bright_yellow(),
        err
    );
}

fn collect_dangerous_paths(paths: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut dangerous = Vec::new();

    for path in paths {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }

        let canonical = protect::sanitize_path(path)?;
        if canonical.parent().is_none() {
            continue;
        }
        if protect::is_protected_path(&canonical) {
            dangerous.push(canonical);
        }
    }

    dangerous.sort();
    dangerous.dedup();
    Ok(dangerous)
}

fn confirm_dangerous_deletion(options: &cli::CliOptions) -> std::io::Result<bool> {
    if options.dry_run {
        return Ok(false);
    }

    let dangerous = collect_dangerous_paths(&options.paths)?;
    if dangerous.is_empty() {
        return Ok(false);
    }

    if options.silent {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "dangerous system path deletion requires interactive confirmation",
        ));
    }

    eprintln!();
    eprintln!(
        "{} dangerous system path deletion requires extra confirmation",
        " WARNING ".on_color(AnsiColors::Yellow).black()
    );
    for path in dangerous.iter().take(20) {
        eprintln!("  {}", path.display().bright_yellow());
    }
    if dangerous.len() > 20 {
        eprintln!("  ... {} more path(s)", dangerous.len() - 20);
    }
    eprint!("Continue? [y/N]: ");
    std::io::stderr().flush()?;

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
        Ok(true)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "dangerous deletion cancelled",
        ))
    }
}

fn run_delete(options: &cli::CliOptions, start: Instant) -> bool {
    let allow_dangerous = match confirm_dangerous_deletion(options) {
        Ok(allow_dangerous) => allow_dangerous,
        Err(err) => {
            cli::print_error(&err);
            return false;
        }
    };

    let errors = if let Some(count) = options.threads.filter(|_| !options.dry_run) {
        match rayon::ThreadPoolBuilder::new().num_threads(count).build() {
            Ok(pool) => pool.install(|| run_delete_inner(options, true, allow_dangerous)),
            Err(err) => {
                eprintln!(
                    "{} failed to configure thread pool: {}",
                    " ERROR ".on_color(AnsiColors::BrightRed).black(),
                    err
                );
                return false;
            }
        }
    } else {
        run_delete_inner(options, false, allow_dangerous)
    };

    if errors.is_empty() {
        let elapsed = start.elapsed().as_secs_f32();
        println!("{elapsed:.3}s");
        true
    } else {
        eprintln!(
            "\n{} {} path(s) failed out of {}",
            " SUMMARY ".on_color(AnsiColors::BrightRed).black(),
            errors.len(),
            options.paths.len()
        );
        false
    }
}

fn run_delete_inner(
    options: &cli::CliOptions,
    use_current_pool: bool,
    allow_dangerous: bool,
) -> Vec<(PathBuf, std::io::Error)> {
    if options.dry_run || options.paths.len() <= 1 {
        run_delete_sequential(options, use_current_pool, allow_dangerous)
    } else {
        run_delete_parallel(options, use_current_pool, allow_dangerous)
    }
}

fn run_delete_sequential(
    options: &cli::CliOptions,
    use_current_pool: bool,
    allow_dangerous: bool,
) -> Vec<(PathBuf, std::io::Error)> {
    let mut errors = Vec::new();
    for path in &options.paths {
        let result = if options.dry_run {
            if options.silent {
                delete::dry_run_path_silent(path)
            } else {
                delete::dry_run_path(path)
            }
        } else if use_current_pool {
            // Inside a pool.install(...) scope: thread count is None so we
            // reuse the surrounding pool instead of nesting a new one.
            let opts = delete::DeleteOptions::default();
            let opts = if options.silent { opts.silent() } else { opts };
            let opts = if allow_dangerous {
                opts.allow_dangerous()
            } else {
                opts
            };
            delete::delete_path(path, opts)
        } else {
            let opts = delete::DeleteOptions::default().with_threads(options.threads);
            let opts = if options.silent { opts.silent() } else { opts };
            let opts = if allow_dangerous {
                opts.allow_dangerous()
            } else {
                opts
            };
            delete::delete_path(path, opts)
        };
        if let Err(err) = result {
            report_error(path, &err);
            errors.push((path.clone(), err));
        }
    }
    errors
}

fn run_delete_parallel(
    options: &cli::CliOptions,
    use_current_pool: bool,
    allow_dangerous: bool,
) -> Vec<(PathBuf, std::io::Error)> {
    let errors: std::sync::Mutex<Vec<(PathBuf, std::io::Error)>> =
        std::sync::Mutex::new(Vec::new());

    if options.silent {
        // Silent mode: no progress bars, no MultiProgress
        rayon::scope(|s| {
            for path in &options.paths {
                s.spawn(|_| {
                    let opts = delete::DeleteOptions::default().silent();
                    let opts = if use_current_pool {
                        opts
                    } else {
                        opts.with_threads(options.threads)
                    };
                    let opts = if allow_dangerous {
                        opts.allow_dangerous()
                    } else {
                        opts
                    };
                    let result = delete::delete_path(path, opts);
                    if let Err(err) = result {
                        report_error(path, &err);
                        errors.lock().unwrap().push((path.clone(), err));
                    }
                });
            }
        });
        return errors.into_inner().unwrap();
    }

    for chunk in options.paths.chunks(MAX_PARALLEL_DELETES) {
        let mp = MultiProgress::new();
        mp.set_move_cursor(true);
        rayon::scope(|s| {
            let bars: Vec<_> = chunk
                .iter()
                .map(|path| {
                    let bar = indicatif::ProgressBar::new_spinner();
                    bar.set_prefix(progress_name(path));
                    mp.add(bar)
                })
                .collect();

            for (path, bar) in chunk.iter().zip(bars) {
                s.spawn(|_| {
                    let opts = delete::DeleteOptions::default().with_bar(bar);
                    let opts = if use_current_pool {
                        opts
                    } else {
                        opts.with_threads(options.threads)
                    };
                    let opts = if allow_dangerous {
                        opts.allow_dangerous()
                    } else {
                        opts
                    };
                    let result = delete::delete_path(path, opts);
                    if let Err(err) = result {
                        report_error(path, &err);
                        errors.lock().unwrap().push((path.clone(), err));
                    }
                });
            }
        });
    }

    errors.into_inner().unwrap()
}

fn progress_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

#[cfg(windows)]
fn free_console() {
    extern "system" {
        fn FreeConsole() -> i32;
    }
    unsafe {
        FreeConsole();
    }
}

#[cfg(windows)]
fn alloc_console() {
    extern "system" {
        fn AllocConsole() -> i32;
    }
    unsafe {
        AllocConsole();
    }
    CONSOLE_ALLOCATED.store(true, Ordering::SeqCst);
}

fn batch_state(paths_dir: &Path) -> (usize, u64) {
    let mut file_count = 0;
    let mut total_len = 0;

    if let Ok(entries) = fs::read_dir(paths_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    file_count += 1;
                    total_len += metadata.len();
                }
            }
        }
    }

    (file_count, total_len)
}

fn wait_for_batch_quiet(paths_dir: &Path, required_quiet_polls: usize, max_polls: usize) {
    let mut last_state = batch_state(paths_dir);
    let mut quiet_polls = 0;

    for _ in 0..max_polls {
        thread::sleep(BATCH_POLL_INTERVAL);
        let current_state = batch_state(paths_dir);
        if current_state == last_state {
            quiet_polls += 1;
            if quiet_polls >= required_quiet_polls {
                break;
            }
        } else {
            quiet_polls = 0;
            last_state = current_state;
        }
    }
}

fn write_batch_paths(paths_dir: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
    fs::create_dir_all(paths_dir)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();

    for attempt in 0..100_u32 {
        let path = paths_dir.join(format!("{timestamp}-{pid}-{attempt}.txt"));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut file) => {
                for p in paths {
                    writeln!(file, "{}", p.display())?;
                }
                file.flush()?;
                return Ok(());
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

fn run_batch(options: &cli::CliOptions, start: Instant) -> bool {
    let temp = std::env::temp_dir();
    let paths_dir = temp.join("zap-batch-paths.d");
    let old_paths_file = temp.join("zap-batch-paths.txt");
    let lock_file = temp.join("zap-batch.lock");

    // Destroy our console immediately. When launched via zapw.exe
    // (CREATE_NO_WINDOW) this is a no-op. When launched directly,
    // this closes the brief console window as fast as possible.
    #[cfg(windows)]
    free_console();

    // Clean up stale files from a previous crashed run (older than 10s)
    if let Ok(meta) = fs::metadata(&lock_file) {
        if let Ok(modified) = meta.modified() {
            if modified.elapsed().unwrap_or_default() > Duration::from_secs(10) {
                let _ = fs::remove_file(&lock_file);
                let _ = fs::remove_file(&old_paths_file);
                let _ = fs::remove_dir_all(&paths_dir);
            }
        }
    }

    // Each process writes its own file. This avoids interleaved appends from
    // concurrent Explorer launches joining two paths into one invalid path.
    if write_batch_paths(&paths_dir, &options.paths).is_err() {
        // Can't batch, fall back to normal mode
        #[cfg(windows)]
        alloc_console();
        return run_delete(options, start);
    }

    // Try to become the leader (atomic file creation)
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_file)
    {
        Ok(lock) => {
            // Leader: Explorer can launch one process per selected item, and
            // those launches may arrive while deletion is already running.
            // Keep draining newly appended paths so late followers do not
            // allocate their own consoles or lose their targets.
            wait_for_batch_quiet(&paths_dir, BATCH_INITIAL_QUIET_POLLS, BATCH_MAX_POLLS);

            let mut processed_paths = 0;
            let mut success = true;

            loop {
                let all_paths = read_batch_paths(&paths_dir);
                let pending_paths: Vec<PathBuf> =
                    all_paths.iter().skip(processed_paths).cloned().collect();

                if pending_paths.is_empty() {
                    break;
                }

                #[cfg(windows)]
                if !options.silent && !CONSOLE_ALLOCATED.load(Ordering::SeqCst) {
                    alloc_console();
                }

                let merged = cli::CliOptions {
                    paths: pending_paths,
                    dry_run: options.dry_run,
                    threads: options.threads,
                    batch: false,
                    silent: options.silent,
                };
                success = run_delete(&merged, start) && success;
                processed_paths = all_paths.len();

                wait_for_batch_quiet(&paths_dir, BATCH_FINAL_QUIET_POLLS, BATCH_MAX_POLLS);
            }

            // Race-safe cleanup: rename paths_dir first so any late
            // follower still under the lock cannot drop a file into the
            // directory we are about to scan-and-delete. Drain the
            // renamed directory once more for any path that landed
            // between the last read and the rename.
            let drained_dir = paths_dir.with_extension("draining");
            let _ = fs::remove_dir_all(&drained_dir);
            let final_dir = match fs::rename(&paths_dir, &drained_dir) {
                Ok(()) => drained_dir.clone(),
                Err(_) => paths_dir.clone(),
            };

            let leftover_paths: Vec<PathBuf> = read_batch_paths(&final_dir)
                .into_iter()
                .skip(processed_paths)
                .collect();
            if !leftover_paths.is_empty() {
                #[cfg(windows)]
                if !options.silent && !CONSOLE_ALLOCATED.load(Ordering::SeqCst) {
                    alloc_console();
                }
                let merged = cli::CliOptions {
                    paths: leftover_paths,
                    dry_run: options.dry_run,
                    threads: options.threads,
                    batch: false,
                    silent: options.silent,
                };
                success = run_delete(&merged, start) && success;
            }

            drop(lock);
            let _ = fs::remove_file(&old_paths_file);
            let _ = fs::remove_dir_all(&final_dir);
            let _ = fs::remove_dir_all(&paths_dir);
            let _ = fs::remove_file(&lock_file);

            success
        }
        Err(_) => {
            // Follower: path was appended above; the leader owns all output and
            // will pick up late arrivals before releasing the lock.
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_batch_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("zap-batch-test-{pid}-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_batch_paths_are_read_from_separate_files() {
        let dir = test_batch_dir();
        let first = PathBuf::from(r"C:\Users\ilyam\AppData\Local\Temp\.tmpTsDO5HC");
        let second = PathBuf::from(r"C:\Users\ilyam\AppData\Local\Temp\.tmptNZVas");

        write_batch_paths(&dir, std::slice::from_ref(&first)).unwrap();
        write_batch_paths(&dir, std::slice::from_ref(&second)).unwrap();

        let paths = read_batch_paths(&dir);

        assert_eq!(paths, vec![first, second]);
        assert!(!paths
            .iter()
            .any(|path| { path.to_string_lossy().contains(r".tmpTsDO5HC:\Users\ilyam") }));

        let _ = fs::remove_dir_all(&dir);
    }
}
