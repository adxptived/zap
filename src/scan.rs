use std::{
    io,
    path::{Path, PathBuf},
};

const INITIAL_FILES_CAPACITY: usize = 1024;

#[derive(Debug)]
pub enum EntryKind {
    File,
    Dir { depth: usize },
    Symlink,
}

#[derive(Debug)]
pub struct ScannedEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
}

impl ScanStats {
    pub fn total_deletable(&self) -> usize {
        self.files + self.dirs + self.symlinks
    }
}

#[derive(Debug)]
pub struct DirBatch {
    pub depth: usize,
    pub entries: Vec<ScannedEntry>,
}

#[derive(Debug)]
pub struct ScanPlan {
    pub files_and_links: Vec<ScannedEntry>,
    pub dirs_by_depth: Vec<DirBatch>,
    pub stats: ScanStats,
}

impl ScanPlan {
    #[cfg(test)]
    pub fn dir_depths_desc(&self) -> Vec<usize> {
        self.dirs_by_depth.iter().map(|batch| batch.depth).collect()
    }
}

const SCAN_PROGRESS_UPDATE_INTERVAL: u64 = 32;

pub fn scan_directory(path: &Path) -> io::Result<Vec<ScannedEntry>> {
    walk_entries(path, None)
}

#[cfg(test)]
fn scan_directory_with_bar(
    path: &Path,
    bar: &indicatif::ProgressBar,
) -> io::Result<Vec<ScannedEntry>> {
    walk_entries(path, Some(bar))
}

pub(crate) fn scan_directory_plan(path: &Path) -> io::Result<ScanPlan> {
    scan_directory_plan_inner(path, None)
}

pub(crate) fn scan_directory_plan_with_bar(
    path: &Path,
    bar: &indicatif::ProgressBar,
) -> io::Result<ScanPlan> {
    scan_directory_plan_inner(path, Some(bar))
}

fn scan_directory_plan_inner(
    path: &Path,
    external_bar: Option<&indicatif::ProgressBar>,
) -> io::Result<ScanPlan> {
    let mut files_and_links: Vec<ScannedEntry> = Vec::with_capacity(INITIAL_FILES_CAPACITY);
    let mut depth_buckets: Vec<Vec<ScannedEntry>> = Vec::new();
    let mut stats = ScanStats::default();

    walk_entries_with(path, external_bar, |entry| match &entry.kind {
        EntryKind::File => {
            stats.files += 1;
            files_and_links.push(entry);
        }
        EntryKind::Symlink => {
            stats.symlinks += 1;
            files_and_links.push(entry);
        }
        EntryKind::Dir { depth: 0 } => {}
        EntryKind::Dir { depth } => {
            stats.dirs += 1;
            if *depth >= depth_buckets.len() {
                depth_buckets.resize_with(*depth + 1, Vec::new);
            }
            depth_buckets[*depth].push(entry);
        }
    })?;

    let dirs_by_depth: Vec<DirBatch> = depth_buckets
        .into_iter()
        .enumerate()
        .rev()
        .filter(|(_, entries)| !entries.is_empty())
        .map(|(depth, entries)| DirBatch { depth, entries })
        .collect();

    Ok(ScanPlan {
        files_and_links,
        dirs_by_depth,
        stats,
    })
}

fn walk_entries(
    path: &Path,
    external_bar: Option<&indicatif::ProgressBar>,
) -> io::Result<Vec<ScannedEntry>> {
    let mut entries = Vec::new();
    walk_entries_with(path, external_bar, |entry| entries.push(entry))?;
    Ok(entries)
}

fn walk_entries_with(
    path: &Path,
    external_bar: Option<&indicatif::ProgressBar>,
    mut on_entry: impl FnMut(ScannedEntry),
) -> io::Result<()> {
    let local_bar;
    let bar = if let Some(ext) = external_bar {
        ext
    } else {
        local_bar = indicatif::ProgressBar::new_spinner();
        local_bar.set_style(
            indicatif::ProgressStyle::default_spinner()
                .template("{prefix} Scanning {pos} entries")
                .unwrap(),
        );
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        local_bar.set_prefix(name);
        &local_bar
    };

    let mut count: u64 = 0;
    for res in jwalk::WalkDir::new(path)
        .follow_links(false)
        .skip_hidden(false)
        .sort(false)
        .into_iter()
    {
        let entry = res.map_err(|err| io::Error::other(err.to_string()))?;
        let p = entry.path();
        let ft = entry.file_type();

        let kind = if ft.is_symlink() {
            EntryKind::Symlink
        } else if ft.is_dir() {
            #[cfg(windows)]
            {
                match crate::winapi::is_reparse_point(&p) {
                    Ok(true) => EntryKind::Symlink,
                    Ok(false) => EntryKind::Dir { depth: entry.depth },
                    Err(err) => {
                        return Err(io::Error::new(
                            err.kind(),
                            format!("failed to inspect {}: {}", p.display(), err),
                        ));
                    }
                }
            }
            #[cfg(not(windows))]
            {
                EntryKind::Dir { depth: entry.depth }
            }
        } else {
            EntryKind::File
        };

        count += 1;
        if count.is_multiple_of(SCAN_PROGRESS_UPDATE_INTERVAL) {
            bar.set_position(count);
            bar.tick();
        }
        on_entry(ScannedEntry { path: p, kind });
    }

    bar.set_position(count);
    if external_bar.is_none() {
        bar.finish_and_clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn create_test_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = std::env::temp_dir().join(format!("zap-scan-test-{}", id));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        temp
    }

    #[test]
    fn test_scan_directory_categorizes_entries() {
        let temp = create_test_dir();
        File::create(temp.join("file.txt")).unwrap();
        fs::create_dir(temp.join("subdir")).unwrap();
        let entries = scan_directory(&temp).unwrap();
        let files = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::File))
            .count();
        let dirs = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Dir { .. }))
            .count();
        assert!(files >= 1);
        assert!(dirs >= 1);
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_scan_directory_includes_root_at_depth_0() {
        let temp = create_test_dir();
        File::create(temp.join("file.txt")).unwrap();
        fs::create_dir(temp.join("subdir")).unwrap();
        let entries = scan_directory(&temp).unwrap();
        let root_entries: Vec<_> = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Dir { depth: 0 }))
            .collect();
        assert_eq!(root_entries.len(), 1);
        assert!(root_entries[0].path == temp);
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_scan_directory_classifies_symlink_as_symlink() {
        let temp = create_test_dir();
        let target = temp.join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.join("link");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let entries = scan_directory(&temp).unwrap();
        let link_entry = entries.iter().find(|e| e.path == link);
        assert!(link_entry.is_some());
        assert!(matches!(link_entry.unwrap().kind, EntryKind::Symlink));
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    #[cfg(windows)]
    fn test_scan_directory_classifies_junction_as_symlink() {
        let temp = create_test_dir();
        let target = temp.join("target");
        fs::create_dir(&target).unwrap();
        let junction = temp.join("junction");
        let status = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                junction.to_str().unwrap(),
                target.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(status.status.success());
        let entries = scan_directory(&temp).unwrap();
        let junction_entry = entries.iter().find(|e| e.path == junction);
        assert!(junction_entry.is_some());
        assert!(matches!(junction_entry.unwrap().kind, EntryKind::Symlink));
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_scan_directory_file_symlink_classified_as_symlink() {
        let temp = create_test_dir();
        let target_file = temp.join("target.txt");
        File::create(&target_file).unwrap();
        let link = temp.join("link.txt");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target_file, &link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_file, &link).unwrap();
        let entries = scan_directory(&temp).unwrap();
        let link_entry = entries.iter().find(|e| e.path == link);
        assert!(link_entry.is_some());
        assert!(matches!(link_entry.unwrap().kind, EntryKind::Symlink));
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_scan_directory_with_bar_returns_same_entries() {
        let temp = create_test_dir();
        File::create(temp.join("a.txt")).unwrap();
        fs::create_dir(temp.join("sub")).unwrap();
        File::create(temp.join("sub/b.txt")).unwrap();
        let plain = scan_directory(&temp).unwrap();
        let bar = indicatif::ProgressBar::new_spinner();
        let with_bar = scan_directory_with_bar(&temp, &bar).unwrap();
        assert_eq!(plain.len(), with_bar.len());
        for (a, b) in plain.iter().zip(with_bar.iter()) {
            assert_eq!(a.path, b.path);
            assert!(std::mem::discriminant(&a.kind) == std::mem::discriminant(&b.kind));
        }
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_scan_directory_plan_groups_entries_and_counts_rootless_dirs() {
        let temp = create_test_dir();
        let a = temp.join("a");
        let b = a.join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        File::create(temp.join("root.txt")).unwrap();
        File::create(b.join("nested.txt")).unwrap();
        let plan = scan_directory_plan(&temp).unwrap();
        assert_eq!(plan.stats.files, 2);
        assert_eq!(plan.stats.dirs, 2);
        assert_eq!(plan.stats.symlinks, 0);
        assert_eq!(plan.stats.total_deletable(), 4);
        assert_eq!(plan.files_and_links.len(), 2);
        assert_eq!(plan.dir_depths_desc(), vec![2, 1]);
        assert!(plan.dirs_by_depth[0]
            .entries
            .iter()
            .any(|entry| entry.path == b));
        assert!(plan.dirs_by_depth[1]
            .entries
            .iter()
            .any(|entry| entry.path == a));
        let _ = fs::remove_dir_all(&temp);
    }
}
