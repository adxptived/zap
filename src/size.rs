//! Shared directory-size helpers used by dry-run, GUI, and batch output.

use std::path::{Path, PathBuf};

/// Total bytes and entry count of a tree. `entries` counts every file,
/// link and directory including the root itself — the same unit the
/// deletion progress counter uses, so `removed / entries` is a percentage.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TreeStats {
    pub bytes: u64,
    pub entries: u64,
}

impl std::ops::Add for TreeStats {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self {
            bytes: self.bytes + other.bytes,
            entries: self.entries + other.entries,
        }
    }
}

/// Size and entry count of `path`. On Windows sizes come from the directory
/// listing itself, so no file is opened — `metadata()` per file costs about
/// as much kernel time as deleting it.
pub fn tree_stats(path: &Path) -> TreeStats {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return TreeStats::default();
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return TreeStats {
            bytes: if meta.is_file() { meta.len() } else { 0 },
            entries: 1,
        };
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
            stats.entries += 1;
            if entry.file_type().is_file() {
                stats.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        stats
    }
}

#[cfg(windows)]
fn tree_stats_dir(dir: &Path, depth: usize) -> TreeStats {
    use rayon::prelude::*;
    let mut stats = TreeStats {
        bytes: 0,
        entries: 1,
    };
    let mut subdirs = Vec::new();
    let _ = crate::winapi::for_each_dir_entry(dir, |entry| {
        if entry.is_dir() && !entry.is_reparse_point() {
            subdirs.push(crate::winapi::join_wide(dir, entry.name));
        } else {
            stats.entries += 1;
            if !entry.is_reparse_point() {
                stats.bytes += entry.size;
            }
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
        stats.entries += 1;
        let _ = crate::winapi::for_each_dir_entry(&dir, |entry| {
            if entry.is_dir() && !entry.is_reparse_point() {
                pending.push(crate::winapi::join_wide(&dir, entry.name));
            } else {
                stats.entries += 1;
                if !entry.is_reparse_point() {
                    stats.bytes += entry.size;
                }
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

/// Immediate children of `root` with their aggregated sizes, for the
/// treemap: the rectangles partition the root without double counting.
/// Child subtrees are sized in parallel with [`tree_stats`].
pub fn child_sizes(root: &Path) -> std::io::Result<Vec<(PathBuf, u64)>> {
    use rayon::prelude::*;
    let children: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    Ok(children
        .into_par_iter()
        .map(|path| {
            let bytes = tree_stats(&path).bytes;
            (path, bytes)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
                entries: 4
            }
        );
        assert_eq!(dir_size_recursive(&root), 30);
        assert_eq!(
            tree_stats(&root.join("a.bin")),
            TreeStats {
                bytes: 10,
                entries: 1
            }
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_child_sizes_partition_root() {
        let root = temp_dir();
        let sub = root.join("sub");
        fs::create_dir_all(sub.join("deeper")).unwrap();
        fs::write(root.join("top.bin"), vec![0u8; 10]).unwrap();
        fs::write(sub.join("deeper").join("nested.bin"), vec![0u8; 30]).unwrap();

        let mut entries = child_sizes(&root).unwrap();
        entries.sort();
        assert_eq!(entries, vec![(sub, 30), (root.join("top.bin"), 10)]);

        let _ = fs::remove_dir_all(&root);
    }
}
