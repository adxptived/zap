use super::report::{failure_summary, lock_failures, push_failure, DeletionFailure};
use super::*;

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

/// `keep_non_empty`: a filtered run deliberately leaves files behind, so a
/// directory that still holds them is kept, not reported as a failure.
fn delete_dir_batches(
    batches: &[crate::scan::DirBatch],
    error_count: &AtomicU64,
    failures: &Mutex<Vec<DeletionFailure>>,
    bar: Option<&ProgressBar>,
    keep_non_empty: bool,
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
                match remove_dir_with_retry(&entry.path) {
                    Ok(()) => {}
                    Err(err)
                        if keep_non_empty && err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                    Err(err) => {
                        error_count.fetch_add(1, Ordering::Relaxed);
                        push_failure(failures, &entry.path, &err);
                    }
                }
            }
            if let Some(b) = bar {
                b.inc(chunk.len() as u64);
            }
        });
    }
}

/// `keep_non_empty_root`: see [`delete_dir_batches`]; the root of a filtered
/// run is removed only when nothing was kept inside it.
fn finalize(
    root: &Path,
    total_errors: u64,
    failures: &Mutex<Vec<DeletionFailure>>,
    bar: Option<ProgressBar>,
    keep_non_empty_root: bool,
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
        Err(e) if keep_non_empty_root && e.kind() == io::ErrorKind::DirectoryNotEmpty => {}
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
// Windows fast tree delete (no filter active)
// ---------------------------------------------------------------------------

#[cfg(windows)]
struct CountingSink<'a> {
    errors: &'a AtomicU64,
    failures: &'a Mutex<Vec<DeletionFailure>>,
}

#[cfg(windows)]
impl crate::tree_delete::FailureSink for CountingSink<'_> {
    fn record(&self, path: &Path, err: &io::Error) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        push_failure(self.failures, path, err);
    }
}

/// Recursive parallel delete: see `tree_delete` for the design. The bar
/// position counts removed entries; callers that know the tree size (the
/// GUI's background size pass) turn it into a real percentage.
#[cfg(windows)]
fn delete_directory_fast(path: &Path, bar: Option<ProgressBar>, shred: bool) -> io::Result<()> {
    // Absolute paths let the Win32 layer add the `\\?\` long-path prefix.
    let root = std::path::absolute(path)?;
    if let Some(ref b) = bar {
        b.set_style(
            ProgressStyle::default_spinner()
                .template("{prefix} {spinner} {msg} {pos} entries ({per_sec})")
                .unwrap(),
        );
        b.set_message("Deleting");
        b.enable_steady_tick(std::time::Duration::from_millis(120));
    }

    let errors = AtomicU64::new(0);
    let failures = Mutex::new(Vec::<DeletionFailure>::new());
    let sink = CountingSink {
        errors: &errors,
        failures: &failures,
    };
    let progress_bar = bar.clone();
    let on_progress = move |removed: u64| {
        if let Some(ref b) = progress_bar {
            b.set_position(removed);
        }
    };
    let deleter = crate::tree_delete::TreeDeleter::new(shred, &sink, Some(&on_progress));
    deleter.clear(&root);
    if let Some(ref b) = bar {
        b.set_position(deleter.removed.load(Ordering::Relaxed));
    }

    finalize(path, errors.load(Ordering::Relaxed), &failures, bar, false)
}

// ---------------------------------------------------------------------------
// Pipeline delete (no filter active, non-Windows)
// ---------------------------------------------------------------------------

#[cfg_attr(windows, allow(dead_code))]
fn delete_directory_pipeline(path: &Path, bar: Option<ProgressBar>, shred: bool) -> io::Result<()> {
    // Both queues derive from the common worker budget. This keeps memory
    // bounded even when a machine exposes hundreds of logical CPUs.
    let workers = rayon::current_num_threads().min(crate::parallelism::MAX_WORKERS);
    let queue_cap = crate::parallelism::queue_capacity(workers);
    let (scan_tx, scan_rx) = crossbeam_channel::bounded::<ScannedEntry>(queue_cap);

    // Keep only a small number of ready batches ahead of Rayon. An unbounded
    // queue could retain the whole tree when scanning outran a slow disk or
    // locked-file retries, defeating the bounded scan channel above.
    let ready_batches = workers.saturating_mul(2).clamp(4, 32);
    let (batch_tx, batch_rx) = crossbeam_channel::bounded::<Vec<ScannedEntry>>(ready_batches);

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
        false,
    );

    finalize(
        path,
        file_errors.load(Ordering::Relaxed) + dir_errors.load(Ordering::Relaxed),
        &failures,
        bar,
        false,
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
    delete_dir_batches(
        &plan.dirs_by_depth,
        &dir_errors,
        &failures,
        bar.as_ref(),
        true,
    );

    finalize(
        path,
        file_errors.load(Ordering::Relaxed) + dir_errors.load(Ordering::Relaxed),
        &failures,
        bar,
        true,
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
        #[cfg(windows)]
        return delete_directory_fast(path, bar, shred);
        #[cfg(not(windows))]
        delete_directory_pipeline(path, bar, shred)
    } else {
        delete_directory_filtered(path, bar, filter, shred)
    }
}

pub(super) fn delete_directory_inner(
    path: &Path,
    threads: Option<usize>,
    silent: bool,
    external_bar: Option<ProgressBar>,
    filter: &FilterConfig,
    shred: bool,
) -> io::Result<()> {
    if let Some(count) = threads {
        let total = crate::parallelism::worker_count(Some(count));
        // The Windows tree delete scans inside the pool itself; only the
        // portable pipeline runs a separate scanner that needs a share.
        let workers = if cfg!(windows) && filter.is_empty() {
            total
        } else {
            crate::parallelism::delete_worker_count(total)
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
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
