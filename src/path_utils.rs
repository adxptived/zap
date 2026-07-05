//! Path formatting helpers shared across binaries.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Extract just the filename (last component) from a path for progress display.
pub fn progress_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Deduplicate paths in-place, preserving first-occurrence order.
pub fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::with_capacity(paths.len());
    paths.retain(|path| seen.insert(path.clone()));
}

/// Truncate a path string to `max_len` chars, prepending "..." if cut.
pub fn compact_path(path: &Path, max_len: usize) -> String {
    let text = path.display().to_string();
    let char_count = text.chars().count();
    if char_count <= max_len {
        return text;
    }
    if max_len < 4 {
        return text.chars().take(max_len).collect();
    }
    // Slice the tail at a char boundary directly — avoids the previous
    // reverse-collect-reverse triple pass and its intermediate Vec.
    let keep = max_len - 3;
    let start = text
        .char_indices()
        .nth(char_count - keep)
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("...{}", &text[start..])
}
