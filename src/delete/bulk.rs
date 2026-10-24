use super::*;

#[derive(Debug, Default)]
pub struct BulkDeleteSummary {
    pub deleted: u64,
    pub errors: Vec<(std::path::PathBuf, String)>,
}

/// Delete many already-selected top-level files/symlinks with one Rayon pass.
///
/// This avoids spawning one task/progress bar per file when Explorer or the CLI
/// passes thousands of individual files. Callers still perform path-level
/// validation before choosing this fast path.
pub fn delete_file_roots_bulk(
    paths: &[std::path::PathBuf],
    shred: bool,
    allow_dangerous: bool,
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
            // Revalidate at the last possible moment. Besides enforcing the
            // central safety policy, this rejects a directory swapped in after
            // the caller selected the bulk file fast path (TOCTOU hardening).
            let result = validate_delete_target(path, allow_dangerous).and_then(|target| {
                if target.metadata.file_type().is_symlink() {
                    delete_symlink(path)
                } else if !target.metadata.is_file() {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "bulk deletion only accepts files or symlinks: {}",
                            path.display()
                        ),
                    ))
                } else if shred {
                    crate::shred::shred_file(path, 3)
                } else {
                    remove_file_with_retry(path)
                }
            });
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
