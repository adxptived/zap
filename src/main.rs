use indicatif::MultiProgress;
use owo_colors::{AnsiColors, OwoColorize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use zap::{batch, cli, delete, path_utils, protect, size, stop};

#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
static CONSOLE_ALLOCATED: AtomicBool = AtomicBool::new(false);

fn main() {
    stop::install_handler();
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

    #[cfg(windows)]
    if CONSOLE_ALLOCATED.load(Ordering::SeqCst) {
        free_console();
    }

    if !success {
        std::process::exit(1);
    }
}

const BATCH_INITIAL_QUIET_POLLS: usize = 10;
const BATCH_FINAL_QUIET_POLLS: usize = 30;
const BATCH_MAX_POLLS: usize = 300;

fn write_error_log(errors: &[(PathBuf, std::io::Error)]) {
    let log_path = std::env::temp_dir().join("zap-errors.log");
    if let Ok(file) = File::create(&log_path) {
        let mut w = BufWriter::new(file);
        for (path, err) in errors {
            let _ = writeln!(w, "{}: {}", path.display(), err);
        }
    }
}

fn elapsed_color(elapsed_secs: f32) -> AnsiColors {
    if elapsed_secs < 5.0 {
        AnsiColors::Green
    } else if elapsed_secs < 30.0 {
        AnsiColors::Yellow
    } else {
        AnsiColors::Red
    }
}

fn report_error(path: &Path, err: &std::io::Error) {
    eprintln!(
        "{} {}: {}",
        " ERROR ".on_color(AnsiColors::BrightRed).black(),
        path.display().bright_yellow(),
        err
    );
}

fn confirm_interactive(paths: &[PathBuf], dry_run: bool, recycle: bool) -> io::Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interactive confirmation requires a terminal (use --yes to skip)",
        ));
    }
    eprintln!();
    eprintln!(
        "{} {}",
        " PREVIEW ".on_color(AnsiColors::Cyan).black(),
        if dry_run {
            "the following will be deleted:"
        } else {
            "delete the following?"
        }
        .bright_white()
    );

    // Compute sizes for the previewed paths in parallel — sequential
    // dir_size_recursive calls can take seconds each on large trees.
    use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
    let preview: Vec<&PathBuf> = paths.iter().take(20).collect();
    let sizes: Vec<u64> = preview
        .par_iter()
        .map(|path| size::dir_size_recursive(path))
        .collect();

    for (path, total) in preview.iter().zip(sizes) {
        if total > 0 {
            eprintln!(
                "  {}  {}",
                path.display().bright_yellow(),
                size::format_size(total).dimmed()
            );
        } else {
            eprintln!("  {}", path.display().bright_yellow());
        }
    }
    if paths.len() > 20 {
        eprintln!("  ... {} more path(s)", paths.len() - 20);
    }

    if dry_run {
        return Ok(false);
    }

    if recycle {
        eprint!("\nMove to Recycle Bin? [y/N]: ");
    } else {
        eprint!("\nDelete permanently? [y/N]: ");
    }
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes") {
        Ok(true)
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "cancelled"))
    }
}

fn guard_self_deletion(options: &cli::CliOptions) -> io::Result<()> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| protect::sanitize_path(&p).ok());
    for path in &options.paths {
        if let Ok(canon) = protect::sanitize_path(path) {
            if let Some(ref exe_path) = exe {
                if canon == *exe_path {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to delete the running executable",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn collect_dangerous_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut dangerous = Vec::new();
    for path in paths {
        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        let canonical = match protect::sanitize_path(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if canonical.parent().is_none() {
            continue;
        }
        if protect::is_protected_path(&canonical) {
            dangerous.push(canonical);
        }
    }
    dangerous.sort();
    dangerous.dedup();
    dangerous
}

fn confirm_dangerous_deletion(options: &cli::CliOptions) -> io::Result<bool> {
    if options.dry_run {
        return Ok(false);
    }
    let dangerous = collect_dangerous_paths(&options.paths);
    if dangerous.is_empty() {
        return Ok(false);
    }
    if options.silent {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
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
    if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes") {
        Ok(true)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "dangerous deletion cancelled",
        ))
    }
}

fn run_delete(options: &cli::CliOptions, start: Instant) -> bool {
    if let Err(err) = guard_self_deletion(options) {
        cli::print_error(&err);
        return false;
    }

    // If no --yes/--force and no --dry-run: show preview with sizes, then
    // ask for interactive confirmation.
    if options.dry_run && !options.batch {
        match confirm_interactive(&options.paths, options.dry_run, options.recycle) {
            Ok(true) => {
                let mut real_options = options.clone();
                real_options.dry_run = false;
                real_options.batch = false;
                return run_delete(&real_options, start);
            }
            Ok(false) => return true,
            Err(err) => {
                cli::print_error(&err);
                return false;
            }
        }
    }

    let allow_dangerous = match confirm_dangerous_deletion(options) {
        Ok(a) => a,
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

    let elapsed = start.elapsed().as_secs_f32();

    if errors.is_empty() {
        if !options.silent {
            println!("{}", format!("{elapsed:.3}s").color(elapsed_color(elapsed)));
        }
        true
    } else {
        write_error_log(&errors);
        eprintln!(
            "\n{} {} path(s) failed out of {}",
            " SUMMARY ".on_color(AnsiColors::BrightRed).black(),
            errors.len(),
            options.paths.len()
        );
        eprintln!(
            "{}",
            format!(
                "  Errors written to {}",
                std::env::temp_dir().join("zap-errors.log").display()
            )
            .dimmed()
        );
        false
    }
}

fn run_delete_inner(
    options: &cli::CliOptions,
    use_current_pool: bool,
    allow_dangerous: bool,
) -> Vec<(PathBuf, std::io::Error)> {
    run_delete_parallel(options, use_current_pool, allow_dangerous)
}

fn run_delete_parallel(
    options: &cli::CliOptions,
    use_current_pool: bool,
    allow_dangerous: bool,
) -> Vec<(PathBuf, std::io::Error)> {
    // Recycle mode: one SHFileOperationW call for the whole selection — a
    // single shell roundtrip and a single Explorer "Undo" entry. Validation
    // (roots, protected paths, symlinks) happens per path inside.
    #[cfg(windows)]
    if options.recycle {
        let errors = delete::recycle_paths_validated(&options.paths, allow_dangerous);
        for (path, err) in &errors {
            report_error(path, err);
        }
        let _ = use_current_pool;
        return errors;
    }

    let errors: std::sync::Mutex<Vec<(PathBuf, std::io::Error)>> =
        std::sync::Mutex::new(Vec::with_capacity(options.paths.len()));

    if options.silent {
        rayon::scope(|s| {
            for path in &options.paths {
                if stop::is_stop_requested() {
                    break;
                }
                let opts = options
                    .to_delete_options(use_current_pool, allow_dangerous)
                    .silent();
                s.spawn(|_| {
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

    let mp = MultiProgress::new();
    mp.set_move_cursor(true);
    rayon::scope(|s| {
        for path in &options.paths {
            if stop::is_stop_requested() {
                break;
            }
            let bar = indicatif::ProgressBar::new_spinner();
            bar.set_prefix(path_utils::progress_name(path));
            let bar = mp.add(bar);
            let opts = options
                .to_delete_options(use_current_pool, allow_dangerous)
                .with_bar(bar);
            s.spawn(|_| {
                let result = delete::delete_path(path, opts);
                if let Err(err) = result {
                    report_error(path, &err);
                    errors.lock().unwrap().push((path.clone(), err));
                }
            });
        }
    });

    errors.into_inner().unwrap()
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

fn run_batch(options: &cli::CliOptions, start: Instant) -> bool {
    let paths_dir = batch::batch_paths_dir();
    let lock_file = batch::batch_lock_file();

    #[cfg(windows)]
    free_console();

    batch::cleanup_stale_batch(&paths_dir, &lock_file);

    if batch::write_batch_paths(&paths_dir, &options.paths).is_err() {
        #[cfg(windows)]
        alloc_console();
        return run_delete(options, start);
    }

    match batch::try_acquire_lock(&lock_file) {
        Ok(mut lock) => {
            batch::wait_for_batch_quiet(&paths_dir, BATCH_INITIAL_QUIET_POLLS, BATCH_MAX_POLLS);

            let all_paths = batch::read_batch_paths(&paths_dir);

            #[cfg(windows)]
            if !CONSOLE_ALLOCATED.load(Ordering::SeqCst) {
                alloc_console();
            }

            let mut merged = options.clone();
            merged.paths = all_paths;
            merged.batch = false;

            let success = run_delete(&merged, start);

            // Drain any late arrivals
            batch::touch_lock(&mut lock);
            batch::wait_for_batch_quiet(&paths_dir, BATCH_FINAL_QUIET_POLLS, BATCH_MAX_POLLS);
            let processed: HashSet<&PathBuf> = merged.paths.iter().collect();
            let late_paths = batch::read_batch_paths(&paths_dir);
            let pending: Vec<PathBuf> = late_paths
                .into_iter()
                .filter(|p| !processed.contains(p))
                .collect();
            if !pending.is_empty() {
                #[cfg(windows)]
                if !options.silent && !CONSOLE_ALLOCATED.load(Ordering::SeqCst) {
                    alloc_console();
                }
                let mut late_merged = options.clone();
                late_merged.paths = pending;
                late_merged.batch = false;
                let _ = run_delete(&late_merged, start);
            }

            drop(lock);
            let _ = fs::remove_dir_all(&paths_dir);
            let _ = fs::remove_file(&lock_file);

            success
        }
        Err(_) => {
            std::process::exit(0);
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

        batch::write_batch_paths(&dir, std::slice::from_ref(&first)).unwrap();
        batch::write_batch_paths(&dir, std::slice::from_ref(&second)).unwrap();

        let paths = batch::read_batch_paths(&dir);

        assert_eq!(paths, vec![first, second]);
        assert!(!paths
            .iter()
            .any(|path| { path.to_string_lossy().contains(r".tmpTsDO5HC:\Users\ilyam") }));

        let _ = fs::remove_dir_all(&dir);
    }
}
