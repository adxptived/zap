#[cfg(test)]
use std::path::PathBuf;
use std::{
    io,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use indicatif::{ProgressBar, ProgressStyle};
use rayon::iter::{ParallelBridge, ParallelIterator};
use rayon::slice::ParallelSlice;

use crate::filter::FilterConfig;
use crate::protect::{is_protected_path, sanitize_path};
use crate::scan::{
    scan_directory, scan_directory_plan_for_filter, scan_directory_plan_with_bar_for_filter,
    scan_into_channel, EntryKind, ScannedEntry,
};

const MAX_REPORTED_FAILURES: usize = 20;

#[derive(Debug, Default)]
pub struct BulkDeleteSummary {
    pub deleted: u64,
    pub errors: Vec<(std::path::PathBuf, String)>,
}

/// Adaptive chunk size: keeps every Rayon thread busy with roughly 256
/// entries while clamped to [64, 65536] to avoid extremes.
#[inline]
fn chunk_size(total: usize) -> usize {
    let threads = rayon::current_num_threads().max(1);
    (total / (threads * 4)).clamp(64, 65536)
}

#[cfg(test)]
static TEST_REMOVE_FILE_FAILURE: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
pub fn set_test_remove_file_failure(path: Option<PathBuf>) {
    *TEST_REMOVE_FILE_FAILURE.lock().unwrap() = path;
}

#[derive(Debug)]
struct DeletionFailure {
    path: std::path::PathBuf,
    error: String,
}

fn lock_failures(
    failures: &Mutex<Vec<DeletionFailure>>,
) -> std::sync::MutexGuard<'_, Vec<DeletionFailure>> {
    failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn push_failure(failures: &Mutex<Vec<DeletionFailure>>, path: &Path, err: &io::Error) {
    let mut failures = lock_failures(failures);
    if failures.len() < MAX_REPORTED_FAILURES {
        failures.push(DeletionFailure {
            path: path.to_path_buf(),
            error: err.to_string(),
        });
    }
}

fn failure_summary(total_errors: u64, failures: &[DeletionFailure]) -> String {
    let mut message = format!("{total_errors} entries could not be deleted");
    if !failures.is_empty() {
        message.push_str(
            "\n  Hint: some files are locked or permission-protected. Close apps using these files (Python/Unity/MCP/etc.) or run as Administrator, then retry.",
        );
    }
    for failure in failures {
        message.push_str(&format!(
            "\n  {}: {}",
            failure.path.display(),
            failure.error
        ));
    }
    if total_errors as usize > failures.len() {
        message.push_str(&format!(
            "\n  ... {} more failure(s) omitted",
            total_errors as usize - failures.len()
        ));
    }
    message
}

pub fn set_writable(path: &Path) -> io::Result<()> {
    let mut perms = std::fs::symlink_metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms)
}

#[inline]
fn try_force_delete(path: &Path, original: io::Error) -> io::Result<()> {
    #[cfg(windows)]
    {
        if crate::winapi::force_delete(path).is_ok() {
            return Ok(());
        }
    }
    let _ = path;
    Err(original)
}

/// Shared recovery path for `PermissionDenied` failures: clear the
/// read-only attribute and retry `op`, then fall back to the Win32
/// force-delete. Any other error kind is returned unchanged.
fn retry_after_clearing_readonly(
    path: &Path,
    original: io::Error,
    op: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if original.kind() != io::ErrorKind::PermissionDenied {
        return Err(original);
    }
    let meta = std::fs::symlink_metadata(path).ok();
    if meta.as_ref().is_some_and(|m| m.permissions().readonly()) {
        let mut perms = meta.unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        if std::fs::set_permissions(path, perms).is_ok() && op(path).is_ok() {
            return Ok(());
        }
    }
    // Transient locks (antivirus scanners, indexers) usually clear within
    // milliseconds — retry briefly before the expensive force-delete fallback.
    for delay_ms in [10u64, 50] {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        if op(path).is_ok() {
            return Ok(());
        }
    }
    try_force_delete(path, original)
}

pub fn remove_file_with_retry(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if TEST_REMOVE_FILE_FAILURE
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|p| p == path)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected remove_file failure",
        ));
    }

    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) => retry_after_clearing_readonly(path, e, |p| std::fs::remove_file(p)),
    }
}

pub fn remove_dir_with_retry(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) => retry_after_clearing_readonly(path, e, |p| std::fs::remove_dir(p)),
    }
}

pub fn delete_symlink(path: &Path) -> io::Result<()> {
    fn remove_either(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path).or_else(|file_err| {
            std::fs::remove_dir(path).map_err(|dir_err| {
                io::Error::new(
                    dir_err.kind(),
                    format!(
                        "failed to remove symlink {}: {}; {}",
                        path.display(),
                        file_err,
                        dir_err
                    ),
                )
            })
        })
    }

    match remove_either(path) {
        Ok(()) => Ok(()),
        Err(e) => retry_after_clearing_readonly(path, e, remove_either),
    }
}

/// Dispatch a single file/symlink entry to the appropriate delete fn.
#[inline]
fn delete_entry(entry: &ScannedEntry, shred: bool) -> io::Result<()> {
    match &entry.kind {
        EntryKind::Symlink => delete_symlink(&entry.path),
        EntryKind::File => {
            if shred {
                crate::shred::shred_file(&entry.path, 3)
            } else {
                remove_file_with_retry(&entry.path)
            }
        }
        EntryKind::Dir { .. } => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers used by both pipeline and filtered paths
// ---------------------------------------------------------------------------

#[inline]
fn process_file_batch(
    chunk: &[ScannedEntry],
    shred: bool,
    error_count: &AtomicU64,
    failures: &Mutex<Vec<DeletionFailure>>,
    bar: Option<&ProgressBar>,
    processed_count: Option<&AtomicU64>,
) {
    // Honour pause and Ctrl+C between batches so huge deletions pause/stop
    // promptly. `wait_if_paused` is a single atomic load when not paused.
    crate::stop::wait_if_paused();
    if crate::stop::is_stop_requested() {
        return;
    }
    for entry in chunk {
        if let Err(err) = delete_entry(entry, shred) {
            error_count.fetch_add(1, Ordering::Relaxed);
            push_failure(failures, &entry.path, &err);
        }
    }
    let processed = chunk.len() as u64;
    if let Some(b) = bar {
        b.inc(processed);
    }
    if let Some(count) = processed_count {
        count.fetch_add(processed, Ordering::Relaxed);
    }
}

fn delete_dir_batches(
    batches: &[crate::scan::DirBatch],
    error_count: &AtomicU64,
    failures: &Mutex<Vec<DeletionFailure>>,
    bar: Option<&ProgressBar>,
) {
    for batch in batches {
        if crate::stop::is_stop_requested() {
            return;
        }
        let cs = chunk_size(batch.entries.len().max(1));
        batch.entries.par_chunks(cs).for_each(|chunk| {
            crate::stop::wait_if_paused();
            if crate::stop::is_stop_requested() {
                return;
            }
            for entry in chunk {
                if let Err(err) = remove_dir_with_retry(&entry.path) {
                    error_count.fetch_add(1, Ordering::Relaxed);
                    push_failure(failures, &entry.path, &err);
                }
            }
            if let Some(b) = bar {
                b.inc(chunk.len() as u64);
            }
        });
    }
}

fn finalize(
    root: &Path,
    total_errors: u64,
    failures: &Mutex<Vec<DeletionFailure>>,
    bar: Option<ProgressBar>,
) -> io::Result<()> {
    if total_errors > 0 {
        if let Some(ref b) = bar {
            b.finish_with_message(format!("Failed ({total_errors} entries)"));
        }
        let failures = lock_failures(failures);
        return Err(io::Error::other(failure_summary(total_errors, &failures)));
    }

    if crate::stop::is_stop_requested() {
        if let Some(ref b) = bar {
            b.finish_with_message("Cancelled");
        }
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "cancelled by user",
        ));
    }

    match remove_dir_with_retry(root) {
        Ok(()) => {
            if let Some(ref b) = bar {
                b.inc(1);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            if let Some(ref b) = bar {
                b.finish_and_clear();
            }
            return Err(e);
        }
    }

    if let Some(ref b) = bar {
        b.finish_with_message("Deleted");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pipeline delete (no filter active)
// ---------------------------------------------------------------------------

fn delete_directory_pipeline(path: &Path, bar: Option<ProgressBar>, shred: bool) -> io::Result<()> {
    // Bounded channel from scan-thread to consumer-thread.
    // Capacity = num_threads * 4 so backpressure kicks in before memory bloats.
    let queue_cap = (rayon::current_num_threads() * 4).max(64);
    let (scan_tx, scan_rx) = crossbeam_channel::bounded::<ScannedEntry>(queue_cap);

    // Channel from consumer-thread to rayon workers: unbounded batches.
    let (batch_tx, batch_rx) = crossbeam_channel::unbounded::<Vec<ScannedEntry>>();

    // ── thread A: jwalk scan ──────────────────────────────────────────────
    let scan_path = path.to_path_buf();
    let scan_bar = bar.clone();
    let scan_thread = std::thread::spawn(move || -> io::Result<crate::scan::StreamScanResult> {
        scan_into_channel(&scan_path, &scan_tx, scan_bar.as_ref(), false)
        // scan_tx drops here → scan_rx will drain and close
    });

    // ── thread B: consumer — collects entries and pushes batches ─────────
    // This thread does NO blocking rayon work; it just accumulates and sends.
    let batch_size = chunk_size(queue_cap * 4).max(256);
    let consumer_thread = std::thread::spawn(move || {
        let mut batch: Vec<ScannedEntry> = Vec::with_capacity(batch_size);
        for entry in &scan_rx {
            batch.push(entry);
            if batch.len() >= batch_size {
                let ready = std::mem::replace(&mut batch, Vec::with_capacity(batch_size));
                let _ = batch_tx.send(ready);
            }
        }
        if !batch.is_empty() {
            let _ = batch_tx.send(batch);
        }
        // batch_tx drops here → batch_rx closes
    });

    // ── rayon pool: drain batch_rx in parallel ────────────────────────────
    let file_errors = AtomicU64::new(0);
    let failures = Mutex::new(Vec::<DeletionFailure>::new());
    let file_done = Arc::new(AtomicU64::new(0));
    let monitor_stop = Arc::new(AtomicBool::new(false));
    let progress_monitor = bar.as_ref().map(|b| {
        let bar = b.clone();
        let file_done = Arc::clone(&file_done);
        let monitor_stop = Arc::clone(&monitor_stop);
        std::thread::spawn(move || {
            while !monitor_stop.load(Ordering::Acquire) {
                let deleted = file_done.load(Ordering::Relaxed);
                if deleted > 0 {
                    bar.set_message(format!("Scanning / deleting files ({deleted} deleted)"));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        })
    });

    // Stream batches into the rayon pool as they arrive so file deletion
    // overlaps with the scan (true pipelining). `par_bridge` keeps all
    // workers busy while the scan thread is still producing entries.
    let file_done_for_workers = Arc::clone(&file_done);
    batch_rx.into_iter().par_bridge().for_each(|batch| {
        // Files are deleted while the scan bar is still active, so don't
        // touch the bar here — its position tracks scanned entries.
        process_file_batch(
            &batch,
            shred,
            &file_errors,
            &failures,
            None,
            Some(file_done_for_workers.as_ref()),
        );
    });
    monitor_stop.store(true, Ordering::Release);
    if let Some(handle) = progress_monitor {
        let _ = handle.join();
    }
    consumer_thread.join().ok();

    let scan_result = scan_thread
        .join()
        .map_err(|_| io::Error::other("scan thread panicked"))??;

    if let Some(ref b) = bar {
        b.set_message(format!(
            "Deleted files ({})",
            file_done.load(Ordering::Relaxed)
        ));
        b.set_style(
            ProgressStyle::default_bar()
                .template("{prefix} [{bar:40}] {pos}/{len} {per_sec} ETA {eta} {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );
        b.set_length(scan_result.stats.dirs as u64 + 1);
        b.set_position(0);
        b.set_message(format!("Deleting dirs ({})", scan_result.stats.dirs));
    }

    let dir_errors = AtomicU64::new(0);
    delete_dir_batches(
        &scan_result.dirs_by_depth,
        &dir_errors,
        &failures,
        bar.as_ref(),
    );

    finalize(
        path,
        file_errors.load(Ordering::Relaxed) + dir_errors.load(Ordering::Relaxed),
        &failures,
        bar,
    )
}

// ---------------------------------------------------------------------------
// Filtered path (collect full plan first)
// ---------------------------------------------------------------------------

fn delete_directory_filtered(
    path: &Path,
    bar: Option<ProgressBar>,
    filter: &FilterConfig,
    shred: bool,
) -> io::Result<()> {
    let plan = if let Some(ref b) = bar {
        scan_directory_plan_with_bar_for_filter(path, b, filter)?
    } else {
        scan_directory_plan_for_filter(path, filter)?
    };
    let plan = plan.apply_filter(path, filter);

    if let Some(ref b) = bar {
        b.set_style(
            ProgressStyle::default_bar()
                .template("{prefix} [{bar:40}] {pos}/{len} {per_sec} ETA {eta} {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );
        b.set_length((plan.stats.total_deletable() + 1) as u64);
        b.set_position(0);
        b.set_message(format!("Deleting files ({})", plan.files_and_links.len()));
    }

    let file_errors = AtomicU64::new(0);
    let failures = Mutex::new(Vec::<DeletionFailure>::new());

    let cs = chunk_size(plan.files_and_links.len().max(1));
    plan.files_and_links.par_chunks(cs).for_each(|chunk| {
        process_file_batch(chunk, shred, &file_errors, &failures, bar.as_ref(), None);
    });

    if let Some(ref b) = bar {
        b.set_message(format!("Deleting dirs ({})", plan.stats.dirs));
    }

    let dir_errors = AtomicU64::new(0);
    delete_dir_batches(&plan.dirs_by_depth, &dir_errors, &failures, bar.as_ref());

    finalize(
        path,
        file_errors.load(Ordering::Relaxed) + dir_errors.load(Ordering::Relaxed),
        &failures,
        bar,
    )
}

// ---------------------------------------------------------------------------
// Pool dispatcher
// ---------------------------------------------------------------------------

fn delete_directory_pool_inner(
    path: &Path,
    silent: bool,
    external_bar: Option<ProgressBar>,
    filter: &FilterConfig,
    shred: bool,
) -> io::Result<()> {
    let bar: Option<ProgressBar> = if silent {
        None
    } else {
        let b = external_bar.unwrap_or_else(ProgressBar::new_spinner);
        b.set_style(
            ProgressStyle::default_spinner()
                .template("{prefix} {msg} {pos} entries")
                .unwrap(),
        );
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        b.set_prefix(name);
        Some(b)
    };

    if let Some(ref b) = bar {
        b.set_message("Scanning");
    }

    if filter.is_empty() {
        delete_directory_pipeline(path, bar, shred)
    } else {
        delete_directory_filtered(path, bar, filter, shred)
    }
}

fn delete_directory_inner(
    path: &Path,
    threads: Option<usize>,
    silent: bool,
    external_bar: Option<ProgressBar>,
    filter: &FilterConfig,
    shred: bool,
) -> io::Result<()> {
    if let Some(count) = threads {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(count)
            .build()
            .map_err(|err| io::Error::other(err.to_string()))?;
        return pool
            .install(|| delete_directory_pool_inner(path, silent, external_bar, filter, shred));
    }
    delete_directory_pool_inner(path, silent, external_bar, filter, shred)
}

#[cfg(test)]
pub fn delete_directory(path: &Path, threads: Option<usize>) -> io::Result<()> {
    delete_directory_inner(path, threads, false, None, &FilterConfig::default(), false)
}

// ---------------------------------------------------------------------------
// DeleteOptions builder
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct DeleteOptions {
    pub threads: Option<usize>,
    pub silent: bool,
    pub bar: Option<ProgressBar>,
    pub allow_dangerous: bool,
    pub filter: FilterConfig,
    pub shred: bool,
    pub only_empty: bool,
    pub recycle: bool,
}

impl DeleteOptions {
    pub fn with_threads(mut self, threads: Option<usize>) -> Self {
        self.threads = threads;
        self
    }
    pub fn silent(mut self) -> Self {
        self.silent = true;
        self
    }
    pub fn with_bar(mut self, bar: ProgressBar) -> Self {
        self.bar = Some(bar);
        self
    }
    pub fn allow_dangerous(mut self) -> Self {
        self.allow_dangerous = true;
        self
    }
    pub fn with_filter(mut self, f: FilterConfig) -> Self {
        self.filter = f;
        self
    }
    pub fn shred(mut self) -> Self {
        self.shred = true;
        self
    }
    pub fn only_empty(mut self) -> Self {
        self.only_empty = true;
        self
    }
    pub fn recycle(mut self) -> Self {
        self.recycle = true;
        self
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Move several top-level paths to the Recycle Bin in a single shell call.
///
/// Each path goes through the same validation as `delete_path` (symlinks are
/// unlinked directly, filesystem roots and protected paths are refused).
/// Valid paths are then recycled with one `SHFileOperationW` call so Explorer
/// shows a single "Undo" entry. If the batched call fails, falls back to
/// per-path recycling so errors are attributed to the right path.
#[cfg(windows)]
pub fn recycle_paths_validated(
    paths: &[std::path::PathBuf],
    allow_dangerous: bool,
) -> Vec<(std::path::PathBuf, io::Error)> {
    let mut errors: Vec<(std::path::PathBuf, io::Error)> = Vec::new();
    let mut to_recycle: Vec<&std::path::PathBuf> = Vec::with_capacity(paths.len());

    for path in paths {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(err) => {
                errors.push((path.clone(), err));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            if let Err(err) = delete_symlink(path) {
                errors.push((path.clone(), err));
            }
            continue;
        }
        let canonical = match sanitize_path(path) {
            Ok(c) => c,
            Err(err) => {
                errors.push((path.clone(), err));
                continue;
            }
        };
        if canonical.parent().is_none() {
            errors.push((
                path.clone(),
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("refusing to delete filesystem root: {}", path.display()),
                ),
            ));
            continue;
        }
        if is_protected_path(&canonical) && !allow_dangerous {
            errors.push((
                path.clone(),
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing to delete dangerous path without extra confirmation: {}",
                        path.display()
                    ),
                ),
            ));
            continue;
        }
        to_recycle.push(path);
    }

    if to_recycle.is_empty() {
        return errors;
    }

    if crate::recycle::recycle_paths(&to_recycle).is_err() {
        // Batched call failed: retry one-by-one so each error lands on the
        // correct path instead of blaming the whole batch.
        for path in to_recycle {
            if let Err(err) = crate::recycle::recycle_path(path) {
                errors.push((path.clone(), err));
            }
        }
    }

    errors
}

/// Delete many already-selected top-level files/symlinks with one Rayon pass.
///
/// This avoids spawning one task/progress bar per file when Explorer or the CLI
/// passes thousands of individual files. Callers still perform path-level
/// validation before choosing this fast path.
pub fn delete_file_roots_bulk(
    paths: &[std::path::PathBuf],
    shred: bool,
    bar: Option<&ProgressBar>,
) -> BulkDeleteSummary {
    let deleted = AtomicU64::new(0);
    let processed = AtomicU64::new(0);
    let errors = Mutex::new(Vec::<(std::path::PathBuf, String)>::new());
    let cs = chunk_size(paths.len().max(1));

    // Record a "cancelled" error for every path skipped after a stop
    // request. Silence here would let callers (GUI items, the operation
    // journal) report never-deleted paths as successes — the journal must
    // reflect what actually happened on disk.
    let mark_cancelled = |skipped: &[std::path::PathBuf]| {
        if let Ok(mut guard) = errors.lock() {
            for path in skipped {
                guard.push((path.clone(), "cancelled by user".to_owned()));
            }
        }
    };

    paths.par_chunks(cs).for_each(|chunk| {
        if crate::stop::is_stop_requested() {
            mark_cancelled(chunk);
            return;
        }
        for (i, path) in chunk.iter().enumerate() {
            crate::stop::wait_if_paused();
            if crate::stop::is_stop_requested() {
                mark_cancelled(&chunk[i..]);
                break;
            }
            let result = match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => delete_symlink(path),
                Ok(_) if shred => crate::shred::shred_file(path, 3),
                Ok(_) => remove_file_with_retry(path),
                Err(err) => Err(err),
            };
            match result {
                Ok(()) => {
                    deleted.fetch_add(1, Ordering::Relaxed);
                }
                Err(err) => {
                    if let Ok(mut guard) = errors.lock() {
                        guard.push((path.clone(), err.to_string()));
                    }
                }
            }
            processed.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(b) = bar {
            b.set_position(processed.load(Ordering::Relaxed));
        }
    });

    BulkDeleteSummary {
        deleted: deleted.load(Ordering::Relaxed),
        errors: errors.into_inner().unwrap_or_default(),
    }
}

pub fn delete_path(path: &Path, opts: DeleteOptions) -> io::Result<()> {
    if opts.only_empty && !path.is_dir() {
        if !opts.silent {
            eprintln!("Skipping non-directory (--only-empty): {}", path.display());
        }
        return Ok(());
    }

    #[cfg(windows)]
    if opts.recycle {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return delete_symlink(path);
        }
        let canonical = sanitize_path(path)?;
        if canonical.parent().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to delete filesystem root: {}", path.display()),
            ));
        }
        if is_protected_path(&canonical) && !opts.allow_dangerous {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to delete dangerous path without extra confirmation: {}",
                    path.display()
                ),
            ));
        }
        return crate::recycle::recycle_path(path);
    }

    delete_path_inner(path, opts)
}

fn delete_path_inner(path: &Path, opts: DeleteOptions) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;

    if metadata.file_type().is_symlink() {
        return delete_symlink(path);
    }

    let canonical = sanitize_path(path)?;
    if canonical.parent().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to delete filesystem root: {}", path.display()),
        ));
    }

    if is_protected_path(&canonical) && !opts.allow_dangerous {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to delete dangerous path without extra confirmation: {}",
                path.display()
            ),
        ));
    }

    if metadata.is_dir() {
        if opts.only_empty {
            // Use `remove_dir` semantics: it only succeeds on empty
            // directories, atomically. Checking emptiness separately and
            // then deleting would race with concurrent file creation.
            return match remove_dir_with_retry(path) {
                Ok(()) => {
                    if let Some(ref b) = opts.bar {
                        b.finish_with_message("Deleted");
                    }
                    Ok(())
                }
                Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => {
                    if let Some(ref b) = opts.bar {
                        b.finish_with_message("Skipped - directory is not empty");
                    } else if !opts.silent {
                        eprintln!("Skipping non-empty directory: {}", path.display());
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            };
        }
        delete_directory_inner(
            path,
            opts.threads,
            opts.silent,
            opts.bar,
            &opts.filter,
            opts.shred,
        )
    } else if metadata.is_file() {
        // Top-level file roots must respect --include/--exclude/--min-size
        // filters just like files found inside directory roots.
        if !opts.filter.is_empty() {
            let base = path.parent().unwrap_or_else(|| Path::new(""));
            let matches = opts.filter.matches_relative(
                path,
                base,
                true,
                Some(metadata.len()),
                metadata.modified().ok(),
            );
            if !matches {
                if let Some(ref b) = opts.bar {
                    b.finish_with_message("Skipped - excluded by filter");
                } else if !opts.silent {
                    eprintln!("Skipping (excluded by filter): {}", path.display());
                }
                return Ok(());
            }
        }
        if opts.shred {
            crate::shred::shred_file(path, 3)
        } else {
            remove_file_with_retry(path)
        }
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported filesystem entry: {}", path.display()),
        ))
    }
}

pub fn dry_run_path(path: &Path) -> io::Result<()> {
    dry_run_path_inner(path, false)
}

pub fn dry_run_path_silent(path: &Path) -> io::Result<()> {
    dry_run_path_inner(path, true)
}

fn dry_run_path_inner(path: &Path, silent: bool) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;

    if metadata.file_type().is_symlink() {
        if !silent {
            println!("Would delete symlink: {}", path.display());
        }
        return Ok(());
    }

    let canonical = sanitize_path(path)?;
    if is_protected_path(&canonical) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to delete protected path: {}", path.display()),
        ));
    }

    if metadata.is_dir() {
        let entries = scan_directory(path)?;
        let file_count = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::File))
            .count();
        let dir_count = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Dir { .. }))
            .count();
        let link_count = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Symlink))
            .count();
        if !silent {
            println!(
                "Would delete directory: {} ({} files, {} dirs, {} symlinks)",
                path.display(),
                file_count,
                dir_count,
                link_count
            );
        }
    } else if !silent {
        if metadata.is_file() {
            println!("Would delete file: {}", path.display());
        } else {
            println!("Would delete entry: {}", path.display());
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

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

        let summary = delete_file_roots_bulk(&files, false, None);
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
        set_test_remove_file_failure(Some(blocked.clone()));

        let summary = delete_file_roots_bulk(&[ok.clone(), blocked.clone()], false, None);
        set_test_remove_file_failure(None);

        assert_eq!(summary.deleted, 1);
        assert_eq!(summary.errors.len(), 1);
        assert_eq!(summary.errors[0].0, blocked);
        assert!(!ok.exists());
        assert!(blocked.exists());
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
        let summary = delete_file_roots_bulk(&files, false, None);
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
    fn test_dry_run_does_not_delete() {
        let temp = create_test_dir();
        let file = temp.join("keep.txt");
        File::create(&file).unwrap();
        dry_run_path(&temp).unwrap();
        assert!(file.exists());
        let _ = fs::remove_dir_all(&temp);
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
        assert!(dry_run_path(&nonexistent).is_err());
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
    fn test_delete_directory_reports_failed_entries_and_keeps_root() {
        let temp = create_test_dir();
        let file = temp.join("blocked.txt");
        let empty = temp.join("empty");
        File::create(&file).unwrap();
        fs::create_dir(&empty).unwrap();
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
}
