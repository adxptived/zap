use super::*;

/// Preview a deletion without changing anything: what deleting `path`
/// would remove, with the same validation as a real run (filesystem roots,
/// protected paths) and the same filter semantics — filtered-out files are
/// kept, and so is every directory that still holds one. Links are counted
/// as links and never followed.
pub fn preview_path(
    path: &Path,
    filter: &FilterConfig,
    allow_dangerous: bool,
) -> io::Result<TreeStats> {
    let metadata = validate_delete_target(path, allow_dangerous)?.metadata;
    if metadata.file_type().is_symlink() {
        return Ok(TreeStats::link());
    }
    if metadata.is_file() {
        let base = path.parent().unwrap_or_else(|| Path::new(""));
        let kept = !filter.is_empty()
            && !filter.matches_relative(
                path,
                base,
                true,
                Some(metadata.len()),
                metadata.modified().ok(),
            );
        return Ok(if kept {
            TreeStats::default()
        } else {
            TreeStats::file(metadata.len())
        });
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported filesystem entry: {}", path.display()),
        ));
    }
    if filter.is_empty() {
        // Listing-based on Windows: no per-file open.
        return Ok(crate::size::tree_stats(path));
    }
    preview_filtered_dir(path, filter)
}

fn preview_filtered_dir(root: &Path, filter: &FilterConfig) -> io::Result<TreeStats> {
    /// Mark `start` and its ancestors up to `root` as kept. Stops at the
    /// first one already marked: its ancestors were marked with it.
    fn keep_ancestors(kept: &mut HashSet<PathBuf>, start: Option<&Path>, root: &Path) {
        let mut current = start;
        while let Some(dir) = current {
            if !kept.insert(dir.to_path_buf()) || dir == root {
                break;
            }
            current = dir.parent();
        }
    }

    let entries = scan_directory(root)?;
    let mut kept: HashSet<PathBuf> = HashSet::new();
    let mut candidate_dirs: Vec<&Path> = Vec::new();
    let mut stats = TreeStats::default();
    for entry in &entries {
        let is_file = matches!(entry.kind, EntryKind::File);
        let matches = filter.matches_relative(
            &entry.path,
            root,
            is_file,
            entry.file_size,
            entry.modified_time,
        );
        match entry.kind {
            // The root is removed only when nothing below it is kept.
            EntryKind::Dir { depth: 0 } => {}
            EntryKind::Dir { .. } if matches => candidate_dirs.push(&entry.path),
            EntryKind::Dir { .. } => keep_ancestors(&mut kept, Some(&entry.path), root),
            EntryKind::File if matches => stats += TreeStats::file(entry.file_size.unwrap_or(0)),
            EntryKind::Symlink if matches => stats += TreeStats::link(),
            EntryKind::File | EntryKind::Symlink => {
                keep_ancestors(&mut kept, entry.path.parent(), root)
            }
        }
    }
    for dir in candidate_dirs {
        if !kept.contains(dir) {
            stats += TreeStats::dir();
        }
    }
    if !kept.contains(root) {
        stats += TreeStats::dir();
    }
    Ok(stats)
}
