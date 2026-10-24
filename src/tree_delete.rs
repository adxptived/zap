//! Windows fast path for deleting a whole directory tree (no filters).
//!
//! Each Rayon task enumerates one directory through a handle opened
//! relative to its parent's handle, deletes its files right away, recurses
//! into subdirectories in parallel and removes the directory as soon as its
//! subtree is empty. Compared with a scan-then-delete pipeline this needs no
//! channels, no per-depth barriers and no extra metadata syscalls:
//! attributes (directory / reparse point) arrive with the listing itself.
//!
//! Safety properties preserved from the generic path:
//! * reparse points (symlinks, junctions) are removed as links and never
//!   traversed;
//! * nothing below the root is ever addressed by path: directories are
//!   opened, and every entry is opened for deletion, relative to its
//!   parent's verified handle with `FILE_OPEN_REPARSE_POINT`. Swapping a
//!   path component for a junction mid-run therefore cannot redirect the
//!   traversal *or a single delete* into another tree (which matters most
//!   for elevated runs);
//! * an entry is opened as the type the listing reported, so a directory
//!   swapped in where a file was is never acted on;
//! * a directory is removed only when every child was removed, so a locked
//!   file produces one error instead of a cascade up to the root;
//! * stop/pause requests are honoured between directories and file chunks.
#![cfg(windows)]

use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rayon::prelude::*;

use crate::winapi::{
    self, DirHandle, ACCESS_DELETE, ACCESS_WRITE_ATTRIBUTES, ACCESS_WRITE_DATA,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
};

/// One child from a directory listing: its full path (for error reports
/// and progress), its name (to open it relative to the parent's handle)
/// and the attributes the listing reported.
struct Entry {
    path: PathBuf,
    name: Vec<u16>,
    attributes: u32,
}

impl Entry {
    fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    fn is_link(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

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
        match DirHandle::open(root) {
            Ok(handle) => self.clear_dir(&handle, root, 0),
            // A root that vanished meanwhile is already in the goal state.
            Err(err) if err.kind() == io::ErrorKind::NotFound => true,
            Err(err) => {
                self.sink.record(root, &err);
                false
            }
        }
    }

    fn clear_dir(&self, handle: &DirHandle, dir: &Path, depth: usize) -> bool {
        if depth >= MAX_PARALLEL_DEPTH {
            return self.clear_dir_iterative(handle, dir);
        }
        let Some((subdirs, mut clean)) = self.list_and_remove_leaves(handle, dir) else {
            return false;
        };

        let failed_subdirs = subdirs
            .par_iter()
            .filter(|sub| !self.clear_subdir(handle, sub, depth + 1))
            .count();
        clean &= failed_subdirs == 0;
        clean && !crate::stop::is_stop_requested()
    }

    /// Open `sub` relative to its parent's handle, empty it, then remove it.
    fn clear_subdir(&self, parent: &DirHandle, sub: &Entry, depth: usize) -> bool {
        let cleared = match parent.open_child(&sub.name) {
            Ok(handle) => {
                let cleared = self.clear_dir(&handle, &sub.path, depth);
                // Close before removing the directory itself.
                drop(handle);
                cleared
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return true,
            Err(err) => {
                self.sink.record(&sub.path, &err);
                return false;
            }
        };
        cleared && self.remove(parent, sub)
    }

    /// Stack-based post-order traversal for very deep subtrees.
    fn clear_dir_iterative(&self, root_handle: &DirHandle, root: &Path) -> bool {
        struct Frame {
            /// `None` for the root frame: the caller owns and removes it.
            entry: Option<Entry>,
            /// `None` for the root frame, whose handle the caller owns.
            handle: Option<DirHandle>,
            pending: Vec<Entry>,
            clean: bool,
        }
        let list = |handle: &DirHandle, path: &Path| {
            self.list_and_remove_leaves(handle, path)
                .unwrap_or((Vec::new(), false))
        };

        let (pending, clean) = list(root_handle, root);
        let mut stack = vec![Frame {
            entry: None,
            handle: None,
            pending,
            clean,
        }];
        loop {
            let top = stack.last_mut().expect("stack holds the root until return");
            if let Some(next) = top.pending.pop() {
                if crate::stop::is_stop_requested() {
                    top.clean = false;
                    top.pending.clear();
                    continue;
                }
                let parent = top.handle.as_ref().unwrap_or(root_handle);
                match parent.open_child(&next.name) {
                    Ok(handle) => {
                        let (pending, clean) = list(&handle, &next.path);
                        stack.push(Frame {
                            entry: Some(next),
                            handle: Some(handle),
                            pending,
                            clean,
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => {
                        self.sink.record(&next.path, &err);
                        top.clean = false;
                    }
                }
                continue;
            }
            let done = stack.pop().expect("non-empty");
            match stack.last_mut() {
                None => return done.clean,
                Some(parent) => {
                    let Frame {
                        entry,
                        handle,
                        clean,
                        ..
                    } = done;
                    // Close before removing the directory itself.
                    drop(handle);
                    let entry = entry.expect("only the root frame has no entry");
                    let ok = {
                        let parent_handle = parent.handle.as_ref().unwrap_or(root_handle);
                        clean && self.remove(parent_handle, &entry)
                    };
                    parent.clean &= ok;
                }
            }
        }
    }

    /// List `dir` through its handle, delete its files and links, and
    /// return the real subdirectories still to visit. `None` when the
    /// listing failed.
    fn list_and_remove_leaves(&self, handle: &DirHandle, dir: &Path) -> Option<(Vec<Entry>, bool)> {
        crate::stop::wait_if_paused();
        if crate::stop::is_stop_requested() {
            return None;
        }

        let mut leaves: Vec<Entry> = Vec::new();
        let mut subdirs: Vec<Entry> = Vec::new();
        let listed = handle.for_each_entry(|raw| {
            let entry = Entry {
                path: winapi::join_wide(dir, raw.name),
                name: raw.name.to_vec(),
                attributes: raw.attributes,
            };
            if raw.is_dir() && !raw.is_reparse_point() {
                subdirs.push(entry);
            } else {
                leaves.push(entry);
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
                .map(|chunk| self.remove_leaves(handle, chunk))
                .sum()
        } else {
            self.remove_leaves(handle, &leaves)
        };
        Some((subdirs, failed == 0))
    }

    fn remove_leaves(&self, parent: &DirHandle, chunk: &[Entry]) -> usize {
        crate::stop::wait_if_paused();
        if crate::stop::is_stop_requested() {
            return chunk.len();
        }
        chunk
            .iter()
            .filter(|entry| !self.remove(parent, entry))
            .count()
    }

    /// Remove one entry of `parent`, recording any failure. Returns `true`
    /// on success.
    fn remove(&self, parent: &DirHandle, entry: &Entry) -> bool {
        match self.remove_entry(parent, entry) {
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
                self.sink.record(&entry.path, &err);
                false
            }
        }
    }

    fn remove_entry(&self, parent: &DirHandle, entry: &Entry) -> io::Result<()> {
        #[cfg(test)]
        if crate::delete::is_injected_failure(&entry.path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected remove_file failure",
            ));
        }

        if self.shred && !entry.is_dir() && !entry.is_link() {
            return with_retries(|| self.shred_entry(parent, &entry.name));
        }
        with_retries(|| self.unlink(parent, &entry.name, entry.is_dir()))
    }

    /// Delete the entry `name` of `parent` (a link is removed as a link).
    fn unlink(&self, parent: &DirHandle, name: &[u16], is_dir: bool) -> io::Result<()> {
        let handle = parent.open_entry(name, ACCESS_DELETE, is_dir)?;
        match self.delete_open(&handle) {
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                drop(handle);
                // A read-only entry on a volume without POSIX delete: the
                // attribute has to go first, which needs a handle with
                // FILE_WRITE_ATTRIBUTES.
                let Ok(handle) =
                    parent.open_entry(name, ACCESS_DELETE | ACCESS_WRITE_ATTRIBUTES, is_dir)
                else {
                    return Err(err);
                };
                if !winapi::clear_readonly(&handle).unwrap_or(false) {
                    return Err(err);
                }
                self.delete_open(&handle)
            }
            other => other,
        }
    }

    /// Overwrite and delete the file `name` of `parent` through a single
    /// handle opened relative to it.
    fn shred_entry(&self, parent: &DirHandle, name: &[u16]) -> io::Result<()> {
        let access = ACCESS_DELETE | ACCESS_WRITE_DATA;
        let handle = match parent.open_entry(name, access, false) {
            // Read-only files refuse write access: clear the attribute first.
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                let attrs = parent.open_entry(name, ACCESS_WRITE_ATTRIBUTES, false)?;
                if !winapi::clear_readonly(&attrs)? {
                    return Err(err);
                }
                drop(attrs);
                parent.open_entry(name, access, false)?
            }
            other => other?,
        };
        let mut file = std::fs::File::from(handle);
        let outcome = crate::shred::overwrite(&mut file, 3)?;
        let handle = OwnedHandle::from(file);
        if matches!(outcome, crate::shred::Overwrite::Done) {
            // Best effort: the deleted entry must not reveal the old name.
            let scrubbed: Vec<u16> = crate::shred::random_name().encode_utf16().collect();
            let _ = winapi::rename_in_place(&handle, &scrubbed);
        }
        self.delete_open(&handle)
    }

    /// Delete an open entry: POSIX semantics where the volume supports
    /// them, the classic disposition otherwise.
    fn delete_open(&self, handle: &OwnedHandle) -> io::Result<()> {
        if self.posix_supported.load(Ordering::Relaxed) {
            match winapi::delete_posix(handle) {
                Err(err) if winapi::is_posix_delete_unsupported(&err) => {
                    self.posix_supported.store(false, Ordering::Relaxed);
                }
                other => return other,
            }
        }
        winapi::delete_legacy(handle)
    }
}

/// Retry `op` briefly on transient failures: antivirus scanners and
/// indexers usually release a file within milliseconds.
fn with_retries(op: impl Fn() -> io::Result<()>) -> io::Result<()> {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    let is_transient = |err: &io::Error| {
        err.kind() == io::ErrorKind::PermissionDenied
            || err.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
    };
    let mut result = op();
    for delay_ms in [10u64, 50] {
        match &result {
            Err(err) if is_transient(err) => {}
            _ => return result,
        }
        if crate::stop::is_stop_requested() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "deletion cancelled",
            ));
        }
        std::thread::park_timeout(std::time::Duration::from_millis(delay_ms));
        result = op();
    }
    result
}
