//! Windows fast path for deleting a whole directory tree (no filters).
//!
//! Each Rayon task enumerates one directory with `FindFirstFileExW`,
//! deletes its files right away, recurses into subdirectories in parallel
//! and removes the directory as soon as its subtree is empty. Compared with
//! a scan-then-delete pipeline this needs no channels, no per-depth barriers
//! and no extra metadata syscalls: attributes (directory / reparse point)
//! arrive with the directory listing itself.
//!
//! Safety properties preserved from the generic path:
//! * reparse points (symlinks, junctions) are removed as links and never
//!   traversed — `FILE_FLAG_OPEN_REPARSE_POINT` on the delete handle too;
//! * a directory is removed only when every child was removed, so a locked
//!   file produces one error instead of a cascade up to the root;
//! * stop/pause requests are honoured between directories and file chunks.
#![cfg(windows)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rayon::prelude::*;

use crate::winapi::{self, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};

/// Below this many files a directory is cleared by the visiting worker
/// alone; above it, its files are split across the pool.
const PARALLEL_FILES_THRESHOLD: usize = 256;
const FILE_CHUNK: usize = 128;

/// Parallel recursion depth after which a subtree is finished iteratively.
/// Rayon frames are deep; this keeps pathological trees (thousands of
/// nested levels) from overflowing worker stacks.
const MAX_PARALLEL_DEPTH: usize = 48;

/// Receives every failed entry. Implemented by `delete.rs` so failures land
/// in the same capped report as the generic pipeline.
pub(crate) trait FailureSink: Sync {
    fn record(&self, path: &Path, err: &io::Error);
}

pub(crate) struct TreeDeleter<'a> {
    shred: bool,
    sink: &'a dyn FailureSink,
    /// Entries removed so far (files, links and directories).
    pub removed: AtomicU64,
    on_progress: Option<&'a (dyn Fn(u64) + Sync)>,
    posix_supported: AtomicBool,
}

impl<'a> TreeDeleter<'a> {
    pub fn new(
        shred: bool,
        sink: &'a dyn FailureSink,
        on_progress: Option<&'a (dyn Fn(u64) + Sync)>,
    ) -> Self {
        Self {
            shred,
            sink,
            removed: AtomicU64::new(0),
            on_progress,
            posix_supported: AtomicBool::new(true),
        }
    }

    /// Delete everything below `root` (the root itself is kept). Returns
    /// `true` when the subtree is empty afterwards.
    pub fn clear(&self, root: &Path) -> bool {
        self.clear_dir(root, 0)
    }

    fn clear_dir(&self, dir: &Path, depth: usize) -> bool {
        if depth >= MAX_PARALLEL_DEPTH {
            return self.clear_dir_iterative(dir);
        }
        let Some((subdirs, mut clean)) = self.list_and_remove_leaves(dir) else {
            return false;
        };

        let failed_subdirs = subdirs
            .par_iter()
            .filter(|sub| {
                !(self.clear_dir(sub, depth + 1) && self.remove(sub, FILE_ATTRIBUTE_DIRECTORY))
            })
            .count();
        clean &= failed_subdirs == 0;
        clean && !crate::stop::is_stop_requested()
    }

    /// Stack-based post-order traversal for very deep subtrees.
    fn clear_dir_iterative(&self, root: &Path) -> bool {
        struct Frame {
            path: PathBuf,
            pending: Vec<PathBuf>,
            clean: bool,
        }
        let open = |path: PathBuf| -> Frame {
            match self.list_and_remove_leaves(&path) {
                Some((subdirs, clean)) => Frame {
                    path,
                    pending: subdirs,
                    clean,
                },
                None => Frame {
                    path,
                    pending: Vec::new(),
                    clean: false,
                },
            }
        };

        let mut stack = vec![open(root.to_path_buf())];
        loop {
            let top = stack.last_mut().expect("stack holds the root until return");
            if let Some(next) = top.pending.pop() {
                if crate::stop::is_stop_requested() {
                    top.clean = false;
                    top.pending.clear();
                    continue;
                }
                stack.push(open(next));
                continue;
            }
            let done = stack.pop().expect("non-empty");
            match stack.last_mut() {
                None => return done.clean,
                Some(parent) => {
                    let ok = done.clean && self.remove(&done.path, FILE_ATTRIBUTE_DIRECTORY);
                    parent.clean &= ok;
                }
            }
        }
    }

    /// List `dir`, delete its files and links, and return the real
    /// subdirectories still to visit. `None` when the listing failed.
    fn list_and_remove_leaves(&self, dir: &Path) -> Option<(Vec<PathBuf>, bool)> {
        crate::stop::wait_if_paused();
        if crate::stop::is_stop_requested() {
            return None;
        }

        let mut leaves: Vec<(PathBuf, u32)> = Vec::new();
        let mut subdirs: Vec<PathBuf> = Vec::new();
        let listed = winapi::for_each_dir_entry(dir, |entry| {
            let path = winapi::join_wide(dir, entry.name);
            if entry.is_dir() && !entry.is_reparse_point() {
                subdirs.push(path);
            } else {
                leaves.push((path, entry.attributes));
            }
        });
        if let Err(err) = listed {
            // A directory that vanished meanwhile is already in the goal state.
            if err.kind() == io::ErrorKind::NotFound {
                return Some((Vec::new(), true));
            }
            self.sink.record(dir, &err);
            return None;
        }

        let failed = if leaves.len() >= PARALLEL_FILES_THRESHOLD {
            leaves
                .par_chunks(FILE_CHUNK)
                .map(|chunk| self.remove_leaves(chunk))
                .sum()
        } else {
            self.remove_leaves(&leaves)
        };
        Some((subdirs, failed == 0))
    }

    fn remove_leaves(&self, chunk: &[(PathBuf, u32)]) -> usize {
        crate::stop::wait_if_paused();
        if crate::stop::is_stop_requested() {
            return chunk.len();
        }
        chunk
            .iter()
            .filter(|(path, attrs)| !self.remove(path, *attrs))
            .count()
    }

    /// Remove one entry, recording any failure. Returns `true` on success.
    fn remove(&self, path: &Path, attributes: u32) -> bool {
        match self.remove_entry(path, attributes) {
            Ok(()) => {
                let n = self.removed.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(cb) = self.on_progress {
                    // Batch notifications: indicatif and the GUI poll anyway.
                    if n.is_multiple_of(64) {
                        cb(n);
                    }
                }
                true
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => true,
            Err(err) => {
                self.sink.record(path, &err);
                false
            }
        }
    }

    fn remove_entry(&self, path: &Path, attributes: u32) -> io::Result<()> {
        #[cfg(test)]
        if crate::delete::is_injected_failure(path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected remove_file failure",
            ));
        }

        let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        let is_link = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        if self.shred && !is_dir && !is_link {
            return crate::shred::shred_file(path, 3);
        }

        if self.posix_supported.load(Ordering::Relaxed) {
            match winapi::force_delete(path) {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(err),
                Err(err) if winapi::is_posix_delete_unsupported(&err) => {
                    self.posix_supported.store(false, Ordering::Relaxed);
                }
                // Locked / denied: fall through to the retrying std path.
                Err(_) => {}
            }
        }

        if is_link {
            crate::delete::delete_symlink(path)
        } else if is_dir {
            crate::delete::remove_dir_with_retry(path)
        } else {
            crate::delete::remove_file_with_retry(path)
        }
    }
}
