//! Deletion engine. `delete_path` is the entry point for one selected
//! path; the submodules hold the pieces:
//!
//! * `directory` - whole-tree deletion: the Windows handle-based fast path,
//!   the portable scan/delete pipeline and filtered runs;
//! * `bulk` - many top-level files in one Rayon pass;
//! * `preview` - what a run would remove, without changing anything;
//! * `retry` - single-entry removal with read-only and lock recovery;
//! * `report` - the capped failure list and its summary text.

use std::collections::HashSet;
use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use indicatif::{ProgressBar, ProgressStyle};
use rayon::iter::{ParallelBridge, ParallelIterator};
use rayon::slice::ParallelSlice;

use crate::filter::FilterConfig;
use crate::protect::validate_delete_target;
use crate::scan::{
    scan_directory, scan_directory_plan_for_filter, scan_directory_plan_with_bar_for_filter,
    scan_into_channel, EntryKind, ScannedEntry,
};
use crate::size::TreeStats;

mod bulk;
mod directory;
mod preview;
mod report;
mod retry;
#[cfg(test)]
mod tests;

pub use bulk::{delete_file_roots_bulk, BulkDeleteSummary};
#[cfg(test)]
pub use directory::delete_directory;
use directory::delete_directory_inner;
pub use preview::preview_path;
pub use retry::{delete_symlink, remove_dir_with_retry, remove_file_with_retry, set_writable};

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

#[cfg(test)]
pub(crate) fn is_injected_failure(path: &Path) -> bool {
    TEST_REMOVE_FILE_FAILURE
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|p| p == path)
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
/// Valid paths are then recycled in one shell operation so Explorer shows a
/// single "Undo" entry; the shell reports each item's result separately.
#[cfg(windows)]
pub fn recycle_paths_validated(
    paths: &[std::path::PathBuf],
    allow_dangerous: bool,
) -> Vec<(std::path::PathBuf, io::Error)> {
    let mut errors: Vec<(std::path::PathBuf, io::Error)> = Vec::new();
    let mut to_recycle: Vec<&std::path::PathBuf> = Vec::with_capacity(paths.len());

    for path in paths {
        let target = match validate_delete_target(path, allow_dangerous) {
            Ok(target) => target,
            Err(err) => {
                errors.push((path.clone(), err));
                continue;
            }
        };
        if target.metadata.file_type().is_symlink() {
            if let Err(err) = delete_symlink(path) {
                errors.push((path.clone(), err));
            }
            continue;
        }
        to_recycle.push(path);
    }

    if to_recycle.is_empty() {
        return errors;
    }

    match crate::recycle::recycle_paths(&to_recycle, None) {
        Ok(failures) => errors.extend(failures),
        // The operation could not even start: every path failed with it.
        Err(err) => {
            for path in to_recycle {
                errors.push((path.clone(), io::Error::new(err.kind(), err.to_string())));
            }
        }
    }

    errors
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
        let target = validate_delete_target(path, opts.allow_dangerous)?;
        if target.metadata.file_type().is_symlink() {
            return delete_symlink(path);
        }
        let bar = opts.bar.clone();
        let progress = move |done: u64, total: u64| {
            if let Some(ref b) = bar {
                b.set_length(total);
                b.set_position(done);
            }
        };
        return crate::recycle::recycle_path(path, Some(&progress));
    }

    delete_path_inner(path, opts)
}

fn delete_path_inner(path: &Path, opts: DeleteOptions) -> io::Result<()> {
    let target = validate_delete_target(path, opts.allow_dangerous)?;
    let metadata = target.metadata;

    if metadata.file_type().is_symlink() {
        return delete_symlink(path);
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
