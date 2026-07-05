//! Shared directory-size helpers used by dry-run, GUI, and batch output.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayon::iter::{ParallelBridge, ParallelIterator};

pub fn dir_size_recursive(path: &Path) -> u64 {
    // Fast path: plain files and symlinks need one metadata call, not a
    // jwalk thread-pool spin-up. This matters when callers sum sizes over
    // thousands of file roots (bulk Explorer selections).
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.is_dir() {
            return meta.len();
        }
    }
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

/// Iteratively collect all entries with sizes for treemap visualization
/// using `jwalk`. Returns (path, size) pairs for all files and directories.
/// Does not use recursion, so deep directory trees won't blow the stack.
pub fn dir_size_tree(root: &Path) -> std::io::Result<Vec<(PathBuf, u64)>> {
    let mut result = Vec::new();
    let mut dir_sizes: HashMap<PathBuf, u64> = HashMap::new();

    for entry in jwalk::WalkDir::new(root)
        .follow_links(false)
        .skip_hidden(false)
        .sort(false)
        .into_iter()
    {
        let entry = entry.map_err(|err| std::io::Error::other(err.to_string()))?;
        let path = entry.path();
        let file_type = entry.file_type();

        if file_type.is_file() {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            // Credit the size to the immediate parent only; totals are
            // propagated to all ancestors in one bottom-up pass below.
            // The previous per-file ancestor walk allocated O(depth)
            // PathBufs per file, which dominated large-tree scans.
            if let Some(parent) = path.parent() {
                if parent.starts_with(root) {
                    *dir_sizes.entry(parent.to_path_buf()).or_insert(0) += size;
                }
            }
            result.push((path, size));
        } else if file_type.is_symlink() {
            result.push((path, 0));
        } else if file_type.is_dir() {
            dir_sizes.entry(path).or_insert(0);
        }
    }

    // Propagate directory totals bottom-up: children are deeper than their
    // parents, so one deepest-first pass rolls every subtree into its root.
    let mut order: Vec<PathBuf> = dir_sizes.keys().cloned().collect();
    order.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in &order {
        if path == root {
            continue;
        }
        if let Some(parent) = path.parent() {
            if parent.starts_with(root) {
                let subtotal = dir_sizes.get(path).copied().unwrap_or(0);
                *dir_sizes.entry(parent.to_path_buf()).or_insert(0) += subtotal;
            }
        }
    }

    // Directories stay deepest-first for treemap layout stability.
    result.extend(order.into_iter().map(|path| {
        let size = dir_sizes.get(&path).copied().unwrap_or(0);
        (path, size)
    }));
    Ok(result)
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
    fn test_dir_size_tree_handles_deep_tree_iteratively() {
        let root = temp_dir();
        let mut current = root.clone();
        for i in 0..128 {
            current = current.join(format!("level-{i}"));
            fs::create_dir(&current).unwrap();
        }
        fs::write(current.join("leaf.bin"), vec![1u8; 32]).unwrap();

        let entries = dir_size_tree(&root).unwrap();
        assert!(entries
            .iter()
            .any(|(path, size)| path.ends_with("leaf.bin") && *size == 32));
        assert!(entries
            .iter()
            .any(|(path, size)| path == &root && *size >= 32));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_dir_size_tree_handles_empty_dir() {
        let root = temp_dir();
        let sub_dir = root.join("empty");
        fs::create_dir(&sub_dir).unwrap();

        let entries = dir_size_tree(&root).unwrap();
        assert!(entries
            .iter()
            .any(|(path, size)| path == &sub_dir && *size == 0));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_dir_size_tree_handles_flat_files() {
        let root = temp_dir();
        fs::write(root.join("a.bin"), vec![1u8; 10]).unwrap();
        fs::write(root.join("b.bin"), vec![2u8; 20]).unwrap();

        let entries = dir_size_tree(&root).unwrap();
        assert!(entries
            .iter()
            .any(|(path, size)| path.ends_with("a.bin") && *size == 10));
        assert!(entries
            .iter()
            .any(|(path, size)| path.ends_with("b.bin") && *size == 20));
        assert!(entries
            .iter()
            .any(|(path, size)| path == &root && *size == 30));

        let _ = fs::remove_dir_all(&root);
    }
}
