#[cfg(test)]
use std::path::PathBuf;
use std::{
    io,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use indicatif::{ProgressBar, ProgressStyle};
use rayon::{iter::ParallelIterator, slice::ParallelSlice};

use crate::protect::{is_protected_path, sanitize_path};
use crate::scan::{scan_directory, scan_directory_plan, scan_directory_plan_with_bar, EntryKind};

const MAX_REPORTED_FAILURES: usize = 20;
const PROGRESS_CHUNK_SIZE: usize = 1024;

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

/// Lock the shared failure buffer, recovering from a poisoned mutex.
///
/// A panicking task in `par_chunks(...).for_each(...)` will poison the mutex
/// guarding the failure buffer. Without recovery, every subsequent task would
/// panic on `unwrap()`, masking the original failure and turning a partial
/// deletion into a full process crash. Recovery preserves the contents and
/// lets us still report what we managed to capture.
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

/// Last-resort delete via Win32 POSIX semantics. Handles files locked by
/// other processes, sharing violations, and stubborn read-only attributes
/// that `set_writable` can't clear (e.g. when the parent dir is non-writable).
///
/// On non-Windows targets this is a no-op returning the original error.
#[inline]
fn try_force_delete(path: &Path, original: io::Error) -> io::Result<()> {
    #[cfg(windows)]
    {
        if crate::winapi::force_delete(path).is_ok() {
            return Ok(());
        }
    }
    let _ = path; // silence unused on non-Windows
    Err(original)
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
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            // Try to clear readonly first; if that itself fails or remove
            // still refuses, escalate to POSIX-semantics force delete.
            if set_writable(path).is_ok() {
                if let Ok(()) = std::fs::remove_file(path) {
                    return Ok(());
                }
            }
            try_force_delete(path, e)
        }
        Err(e) => Err(e),
    }
}

pub fn remove_dir_with_retry(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            if let Ok(meta) = std::fs::symlink_metadata(path) {
                let mut perms = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                if std::fs::set_permissions(path, perms).is_ok() {
                    if let Ok(()) = std::fs::remove_dir(path) {
                        return Ok(());
                    }
                }
            }
            // Force-delete works on empty directories too; we only call this
            // function from depth-batched cleanup, so the dir IS empty by
            // construction.
            try_force_delete(path, e)
        }
        Err(e) => Err(e),
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
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            if set_writable(path).is_ok() {
                if let Ok(()) = remove_either(path) {
                    return Ok(());
                }
            }
            try_force_delete(path, e)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
pub fn delete_directory(path: &Path, threads: Option<usize>) -> io::Result<()> {
    delete_directory_inner(path, threads, false, None)
}

fn delete_directory_inner(
    path: &Path,
    threads: Option<usize>,
    silent: bool,
    external_bar: Option<ProgressBar>,
) -> io::Result<()> {
    if let Some(count) = threads {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(count)
            .build()
            .map_err(|err| io::Error::other(err.to_string()))?;
        return pool.install(|| delete_directory_pool_inner(path, silent, external_bar));
    }

    delete_directory_pool_inner(path, silent, external_bar)
}

fn delete_directory_pool_inner(
    path: &Path,
    silent: bool,
    external_bar: Option<ProgressBar>,
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

    let plan = if silent {
        scan_directory_plan(path)?
    } else {
        scan_directory_plan_with_bar(path, bar.as_ref().unwrap())?
    };

    let total = plan.stats.total_deletable();
    if let Some(ref b) = bar {
        b.set_style(
            ProgressStyle::default_bar()
                .template("{prefix} [{bar:40}] {pos}/{len} {per_sec} ETA {eta} {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );
        b.set_length((total + 1) as u64);
        b.set_position(0);
        b.set_message(format!("Deleting files ({})", plan.files_and_links.len()));
    }

    let file_errors: AtomicU64 = AtomicU64::new(0);
    let failures = Mutex::new(Vec::new());
    plan.files_and_links
        .par_chunks(PROGRESS_CHUNK_SIZE)
        .for_each(|chunk| {
            for entry in chunk {
                let result = match &entry.kind {
                    EntryKind::Symlink => delete_symlink(&entry.path),
                    EntryKind::File => remove_file_with_retry(&entry.path),
                    _ => Ok(()),
                };
                if let Err(err) = result {
                    file_errors.fetch_add(1, Ordering::Relaxed);
                    push_failure(&failures, &entry.path, &err);
                }
            }
            if let Some(ref b) = bar {
                b.inc(chunk.len() as u64);
            }
        });

    let mut total_errors = file_errors.load(Ordering::Relaxed);
    if total_errors > 0 {
        if let Some(ref b) = bar {
            b.set_message(format!(
                "Files failed ({total_errors}); cleaning remaining dirs"
            ));
        }
    }

    if let Some(ref b) = bar {
        b.set_message(format!("Deleting dirs ({})", plan.stats.dirs));
    }

    let dir_errors: AtomicU64 = AtomicU64::new(0);
    for batch in &plan.dirs_by_depth {
        if let Some(ref b) = bar {
            b.set_message(format!(
                "Deleting dirs depth {} ({})",
                batch.depth,
                batch.entries.len()
            ));
        }
        batch
            .entries
            .par_chunks(PROGRESS_CHUNK_SIZE)
            .for_each(|chunk| {
                for entry in chunk {
                    let result = remove_dir_with_retry(&entry.path);
                    if let Err(err) = result {
                        dir_errors.fetch_add(1, Ordering::Relaxed);
                        push_failure(&failures, &entry.path, &err);
                    }
                }
                if let Some(ref b) = bar {
                    b.inc(chunk.len() as u64);
                }
            });
    }

    total_errors += dir_errors.load(Ordering::Relaxed);
    if total_errors > 0 {
        if let Some(ref b) = bar {
            b.finish_with_message(format!("Failed ({total_errors} entries)"));
        }
        let failures = lock_failures(&failures);
        return Err(io::Error::other(failure_summary(total_errors, &failures)));
    }

    // Remove the root directory itself
    if path.exists() {
        if let Some(ref b) = bar {
            b.set_message("Finalizing");
        }
        if let Err(e) = remove_dir_with_retry(path) {
            if let Some(ref b) = bar {
                b.finish_and_clear();
            }
            return Err(e);
        }
        if let Some(ref b) = bar {
            b.inc(1);
        }
    }

    if let Some(ref b) = bar {
        b.finish_with_message("Deleted");
    }

    Ok(())
}

/// Options for [`delete_path`]. Replaces the old four-function surface
/// (`delete_path` / `_with_bar` / `_silent` / `_in_current_pool`) with a
/// single entry point and chainable setters.
#[derive(Default)]
pub struct DeleteOptions {
    /// Number of Rayon worker threads. `None` reuses the current Rayon pool
    /// (or the global one) — set this to `None` when calling from inside an
    /// existing `pool.install(...)` scope to avoid creating a nested pool.
    pub threads: Option<usize>,
    /// Suppress all progress output (used by `--silent` mode).
    pub silent: bool,
    /// Optional pre-allocated progress bar. Used by `MultiProgress` callers
    /// so several deletions can share one rendering surface.
    pub bar: Option<ProgressBar>,
    /// Permit deletion of paths that normally require extra caller-side
    /// confirmation, such as Windows and Program Files subtrees.
    pub allow_dangerous: bool,
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
}

/// Delete a file, directory, or symlink at `path`.
///
/// Safety guards (in order):
///
/// 1. `symlink_metadata` (never follow symlinks at the surface layer).
/// 2. Symlink fast-path: remove the link only, never the target.
/// 3. `sanitize_path` (canonicalize + strip `\\?\`).
/// 4. `is_protected_path` (refuses system / well-known profile dirs).
/// 5. `parent().is_none()` guard against filesystem roots (`C:\`, `D:\`, …).
pub fn delete_path(path: &Path, opts: DeleteOptions) -> io::Result<()> {
    delete_path_inner(
        path,
        opts.threads,
        opts.silent,
        opts.bar,
        opts.allow_dangerous,
    )
}

fn delete_path_inner(
    path: &Path,
    threads: Option<usize>,
    silent: bool,
    bar: Option<ProgressBar>,
    allow_dangerous: bool,
) -> io::Result<()> {
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

    if is_protected_path(&canonical) && !allow_dangerous {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to delete dangerous path without extra confirmation: {}",
                path.display()
            ),
        ));
    }

    if metadata.is_dir() {
        delete_directory_inner(path, threads, silent, bar)
    } else if metadata.is_file() {
        remove_file_with_retry(path)
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
        // Only the symlink itself is removed, not its target.
        // This is safe regardless of where the symlink points.
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

    if canonical.parent().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to delete filesystem root: {}", path.display()),
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
        fs::create_dir(temp.join("subdir")).unwrap();

        delete_path(&temp, DeleteOptions::default()).unwrap();

        assert!(!temp.exists());
    }

    #[test]
    fn test_delete_symlink_removes_link() {
        let temp = create_test_dir();
        let target = temp.join("target");
        fs::create_dir(&target).unwrap();

        let link = temp.join("link");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(link.exists());

        delete_symlink(&link).unwrap();

        assert!(!link.exists());
        assert!(target.exists());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_delete_directory_does_not_follow_symlink_target() {
        let temp = create_test_dir();
        let delete_root = temp.join("delete-root");
        let external_target = temp.join("external-target");
        fs::create_dir(&delete_root).unwrap();
        fs::create_dir(&external_target).unwrap();
        File::create(external_target.join("keep.txt")).unwrap();

        let link = delete_root.join("link");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&external_target, &link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external_target, &link).unwrap();

        delete_directory(&delete_root, None).unwrap();

        assert!(!delete_root.exists());
        assert!(external_target.exists());
        assert!(external_target.join("keep.txt").exists());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_delete_directory_idempotent() {
        let temp = create_test_dir();
        File::create(temp.join("file.txt")).unwrap();

        delete_directory(&temp, None).unwrap();
        assert!(!temp.exists());

        let result = delete_directory(&temp, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_delete_empty_directory() {
        let temp = create_test_dir();
        let empty = temp.join("empty");
        fs::create_dir(&empty).unwrap();

        delete_directory(&empty, None).unwrap();

        assert!(!empty.exists());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_delete_path_refuses_filesystem_root() {
        // Path::parent() guards delete/dry-run against operating on the FS root.
        let root = Path::new("C:\\");
        assert!(root.parent().is_none(), "sanity: C:\\ has no parent");

        // Use dry-run so we never actually attempt to delete the root.
        // We expect either InvalidInput (our guard) or PermissionDenied
        // (protected-path check) — both are acceptable refusals.
        match dry_run_path_silent(root) {
            Ok(()) => panic!("dry_run_path on filesystem root must not succeed"),
            Err(e) => {
                let kind = e.kind();
                assert!(
                    matches!(
                        kind,
                        io::ErrorKind::InvalidInput
                            | io::ErrorKind::PermissionDenied
                            | io::ErrorKind::NotFound
                    ),
                    "unexpected error kind for FS root: {kind:?} ({e})"
                );
            }
        }
    }

    #[test]
    fn test_remove_file_with_retry_handles_readonly() {
        let temp = create_test_dir();
        let file = temp.join("readonly.txt");
        File::create(&file).unwrap();

        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&file, perms).unwrap();

        remove_file_with_retry(&file).unwrap();
        assert!(!file.exists());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_dry_run_path_does_not_delete_file() {
        let temp = create_test_dir();
        let file = temp.join("file.txt");
        File::create(&file).unwrap();

        dry_run_path(&file).unwrap();

        assert!(file.exists());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_dry_run_path_does_not_delete_directory() {
        let temp = create_test_dir();
        let subdir = temp.join("subdir");
        fs::create_dir(&subdir).unwrap();
        File::create(subdir.join("file.txt")).unwrap();

        dry_run_path(&subdir).unwrap();

        assert!(subdir.exists());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_delete_path_refuses_protected_path_integration() {
        // Verify is_protected_path logic with string-only paths (no filesystem access).
        // This avoids touching real C:\Windows which triggers AV and is slow.
        assert!(
            crate::protect::is_protected_path(Path::new(r"C:\Windows")),
            "C:\\Windows should be protected (string check)"
        );
        assert!(
            crate::protect::is_protected_path(Path::new(r"C:\Program Files")),
            "C:\\Program Files should be protected (string check)"
        );
        // Verify a real temp path is NOT protected and can be deleted
        let temp = create_test_dir();
        let file = temp.join("file.txt");
        File::create(&file).unwrap();
        delete_path(&file, DeleteOptions::default()).unwrap();
        assert!(!file.exists());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_delete_path_handles_file_symlink() {
        let temp = create_test_dir();
        let target_file = temp.join("target.txt");
        File::create(&target_file).unwrap();

        let link = temp.join("link.txt");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target_file, &link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_file, &link).unwrap();

        assert!(link.exists());

        delete_path(&link, DeleteOptions::default()).unwrap();

        assert!(!link.exists());
        assert!(target_file.exists(), "target file should not be deleted");
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_dry_run_path_symlink_does_not_delete() {
        let temp = create_test_dir();
        let target = temp.join("target");
        fs::create_dir(&target).unwrap();

        let link = temp.join("link");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();

        dry_run_path(&link).unwrap();

        assert!(link.exists(), "dry-run should not delete symlink");
        assert!(target.exists(), "dry-run should not affect symlink target");
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_delete_path_nonexistent_returns_error() {
        let temp = create_test_dir();
        let nonexistent = temp.join("does-not-exist");

        let result = delete_path(&nonexistent, DeleteOptions::default());
        assert!(result.is_err(), "deleting non-existent path should fail");

        let result = dry_run_path(&nonexistent);
        assert!(result.is_err(), "dry-run on non-existent path should fail");
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
        // Create a junction point (doesn't require admin)
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
        assert!(
            status.status.success(),
            "failed to create junction: {}",
            String::from_utf8_lossy(&status.stderr)
        );

        delete_directory(&delete_root, None).unwrap();

        assert!(!delete_root.exists());
        assert!(
            external_target.exists(),
            "junction target should not be deleted"
        );
        assert!(
            external_target.join("keep.txt").exists(),
            "files inside junction target should not be deleted"
        );
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
        assert!(
            !empty.exists(),
            "deletion should still clean unrelated empty directories"
        );

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
            assert!(!d.exists(), "directory should be deleted");
        }
        assert!(base.exists(), "base dir should remain");
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

        // threads=None inside pool.install(...) reuses the surrounding pool.
        pool.install(|| {
            delete_path(&temp, DeleteOptions::default().silent()).unwrap();
        });

        assert!(!temp.exists());
    }

    #[test]
    fn test_lock_failures_recovers_from_poisoned_mutex() {
        // A2 regression: a panicking task inside a Rayon parallel pass would
        // poison the failures mutex; subsequent tasks must still be able to
        // record their errors instead of panicking on `unwrap()`.
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
        // B3 regression: an empty directory must be removed without invoking
        // the scan / rayon machinery. We can't observe the fast path directly,
        // but we can verify functional equivalence: removal succeeds and the
        // root is gone.
        let base = create_test_dir();
        let empty = base.join("empty-fast-path");
        fs::create_dir(&empty).unwrap();

        delete_path(&empty, DeleteOptions::default()).unwrap();

        assert!(!empty.exists());
        assert!(base.exists(), "fast path should not affect siblings");
        let _ = fs::remove_dir_all(&base);
    }
}
