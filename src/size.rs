//! Shared directory-size helpers used by dry-run, GUI, and batch output.

use std::path::Path;

/// Size and composition of a tree. `entries` counts every file, link and
/// directory including the root itself — the same unit the deletion
/// progress counter uses, so `removed / entries` is a percentage — and is
/// always `files + dirs + links`. Links (symlinks, junctions) are counted,
/// never followed, and add no bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TreeStats {
    pub bytes: u64,
    pub entries: u64,
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
}

impl TreeStats {
    pub fn file(bytes: u64) -> Self {
        Self {
            bytes,
            entries: 1,
            files: 1,
            ..Self::default()
        }
    }

    pub fn dir() -> Self {
        Self {
            entries: 1,
            dirs: 1,
            ..Self::default()
        }
    }

    pub fn link() -> Self {
        Self {
            entries: 1,
            links: 1,
            ..Self::default()
        }
    }
}

impl std::ops::Add for TreeStats {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self {
            bytes: self.bytes + other.bytes,
            entries: self.entries + other.entries,
            files: self.files + other.files,
            dirs: self.dirs + other.dirs,
            links: self.links + other.links,
        }
    }
}

impl std::ops::AddAssign for TreeStats {
    fn add_assign(&mut self, other: Self) {
        *self = *self + other;
    }
}

/// Size and entry count of `path`. On Windows sizes come from the directory
/// listing itself, so no file is opened — `metadata()` per file costs about
/// as much kernel time as deleting it.
pub fn tree_stats(path: &Path) -> TreeStats {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return TreeStats::default();
    };
    if meta.file_type().is_symlink() {
        return TreeStats::link();
    }
    if !meta.is_dir() {
        return TreeStats::file(if meta.is_file() { meta.len() } else { 0 });
    }
    #[cfg(windows)]
    {
        tree_stats_dir(path, 0)
    }
    #[cfg(not(windows))]
    {
        let mut stats = TreeStats::default();
        for entry in jwalk::WalkDir::new(path)
            .follow_links(false)
            .skip_hidden(false)
            .sort(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let ft = entry.file_type();
            stats += if ft.is_symlink() {
                TreeStats::link()
            } else if ft.is_dir() {
                TreeStats::dir()
            } else {
                TreeStats::file(entry.metadata().map(|m| m.len()).unwrap_or(0))
            };
        }
        stats
    }
}

/// Stats contribution of one non-directory listing entry.
#[cfg(windows)]
fn leaf_stats(entry: &crate::winapi::RawDirEntry<'_>) -> TreeStats {
    if entry.is_reparse_point() {
        TreeStats::link()
    } else {
        TreeStats::file(entry.size)
    }
}

#[cfg(windows)]
fn tree_stats_dir(dir: &Path, depth: usize) -> TreeStats {
    use rayon::prelude::*;
    let mut stats = TreeStats::dir();
    let mut subdirs = Vec::new();
    let _ = crate::winapi::for_each_dir_entry(dir, |entry| {
        if entry.is_dir() && !entry.is_reparse_point() {
            subdirs.push(crate::winapi::join_wide(dir, entry.name));
        } else {
            stats += leaf_stats(&entry);
        }
    });
    // Same depth guard as tree_delete: past it, recurse sequentially on
    // this worker instead of stacking more Rayon frames.
    let children = if depth < 48 {
        subdirs
            .par_iter()
            .map(|sub| tree_stats_dir(sub, depth + 1))
            .reduce(TreeStats::default, |a, b| a + b)
    } else {
        tree_stats_iterative(subdirs)
    };
    stats + children
}

#[cfg(windows)]
fn tree_stats_iterative(mut pending: Vec<std::path::PathBuf>) -> TreeStats {
    let mut stats = TreeStats::default();
    while let Some(dir) = pending.pop() {
        stats += TreeStats::dir();
        let _ = crate::winapi::for_each_dir_entry(&dir, |entry| {
            if entry.is_dir() && !entry.is_reparse_point() {
                pending.push(crate::winapi::join_wide(&dir, entry.name));
            } else {
                stats += leaf_stats(&entry);
            }
        });
    }
    stats
}

pub fn dir_size_recursive(path: &Path) -> u64 {
    #[cfg(windows)]
    {
        tree_stats(path).bytes
    }
    #[cfg(not(windows))]
    dir_size_recursive_jwalk(path)
}

#[cfg(not(windows))]
fn dir_size_recursive_jwalk(path: &Path) -> u64 {
    // Fast path: plain files and symlinks need one metadata call, not a
    // jwalk thread-pool spin-up. This matters when callers sum sizes over
    // thousands of file roots (bulk Explorer selections).
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.is_dir() {
            return meta.len();
        }
    }
    use rayon::iter::{ParallelBridge, ParallelIterator};
    // The per-entry metadata syscalls dominate this walk, and jwalk only
    // parallelizes traversal — bridge the iterator onto rayon so metadata
    // reads run across the thread pool too.
    jwalk::WalkDir::new(path)
        .follow_links(false)
        .skip_hidden(false)
        .sort(false)
        .into_iter()
        .par_bridge()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zap-size-test-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_tree_stats_counts_bytes_and_entries() {
        let root = temp_dir();
        fs::write(root.join("a.bin"), vec![1u8; 10]).unwrap();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("b.bin"), vec![2u8; 20]).unwrap();

        let stats = tree_stats(&root);
        // root + a.bin + sub + b.bin
        assert_eq!(
            stats,
            TreeStats {
                bytes: 30,
                entries: 4,
                files: 2,
                dirs: 2,
                links: 0,
            }
        );
        assert_eq!(dir_size_recursive(&root), 30);
        assert_eq!(tree_stats(&root.join("a.bin")), TreeStats::file(10));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(windows)]
    fn test_tree_stats_counts_junctions_as_links_without_following() {
        let root = temp_dir();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("big.bin"), vec![0u8; 100]).unwrap();
        let tree = root.join("tree");
        fs::create_dir(&tree).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(tree.join("link"))
            .arg(&target)
            .output()
            .unwrap();
        assert!(status.status.success());

        let stats = tree_stats(&tree);
        assert_eq!(
            (stats.dirs, stats.links, stats.files, stats.bytes),
            (1, 1, 0, 0)
        );

        let _ = fs::remove_dir_all(&root);
    }
}
