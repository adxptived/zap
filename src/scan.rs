use std::{
    io,
    path::{Path, PathBuf},
};

const INITIAL_FILES_CAPACITY: usize = 8192;

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
    pub file_size: Option<u64>,
    pub modified_time: Option<std::time::SystemTime>,
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

    pub fn apply_filter(
        mut self,
        base_dir: &Path,
        filter: &crate::filter::FilterConfig,
    ) -> ScanPlan {
        if filter.is_empty() {
            return self;
        }

        self.files_and_links.retain(|entry| {
            let is_file = matches!(entry.kind, EntryKind::File);
            filter.matches_relative(
                &entry.path,
                base_dir,
                is_file,
                entry.file_size,
                entry.modified_time,
            )
        });

        self.dirs_by_depth = self
            .dirs_by_depth
            .into_iter()
            .map(|batch| DirBatch {
                entries: batch
                    .entries
                    .into_iter()
                    .filter(|entry| {
                        filter.matches_relative(
                            &entry.path,
                            base_dir,
                            false,
                            entry.file_size,
                            entry.modified_time,
                        )
                    })
                    .collect(),
                depth: batch.depth,
            })
            .filter(|batch| !batch.entries.is_empty())
            .collect();

        self.stats = ScanStats {
            files: self
                .files_and_links
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::File))
                .count(),
            symlinks: self
                .files_and_links
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::Symlink))
                .count(),
            dirs: self.dirs_by_depth.iter().map(|b| b.entries.len()).sum(),
        };

        self
    }
}

/// How many jwalk I/O threads to use when walking. Capped at 8 to avoid
/// overwhelming a single spinning disk; SSDs benefit from the full count.
fn scan_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8)
}

/// Progress-bar tick interval during scanning (entries between ticks).
/// 512 keeps the bar responsive without hammering the atomic inside indicatif.
const SCAN_PROGRESS_UPDATE_INTERVAL: u64 = 512;

// ---------------------------------------------------------------------------
// Public scan functions (unchanged API)
// ---------------------------------------------------------------------------

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

#[cfg(test)]
pub(crate) fn scan_directory_plan(path: &Path) -> io::Result<ScanPlan> {
    scan_directory_plan_inner(path, None, true, false)
}

pub(crate) fn scan_directory_plan_for_filter(
    path: &Path,
    filter: &crate::filter::FilterConfig,
) -> io::Result<ScanPlan> {
    scan_directory_plan_inner(path, None, filter.needs_metadata(), false)
}

pub(crate) fn scan_directory_plan_with_bar_for_filter(
    path: &Path,
    bar: &indicatif::ProgressBar,
    filter: &crate::filter::FilterConfig,
) -> io::Result<ScanPlan> {
    scan_directory_plan_inner(path, Some(bar), filter.needs_metadata(), true)
}

// ---------------------------------------------------------------------------
// Streaming / pipeline scan API
//
// `scan_into_channel` walks `path` and immediately sends every file/symlink
// entry to `file_tx` as it is discovered, without waiting for the full walk
// to complete.  Directory entries are accumulated internally and returned as
// a `ScanPlan` (with an empty `files_and_links`) once the walk finishes.
//
// This lets the caller start deleting files while the walk is still running,
// overlapping I/O-bound scan work with I/O-bound delete work.
// ---------------------------------------------------------------------------

/// Result of a streaming scan: directory depth-batches only.
/// Files were already sent to the caller via `file_tx`.
pub struct StreamScanResult {
    pub dirs_by_depth: Vec<DirBatch>,
    pub stats: ScanStats,
}

/// Walk `path` and stream every `File`/`Symlink` entry into `file_tx`.
/// Directory entries are collected and returned in `StreamScanResult`.
///
/// `bar` is ticked every `SCAN_PROGRESS_UPDATE_INTERVAL` entries when set.
pub(crate) fn scan_into_channel(
    path: &Path,
    file_tx: &crossbeam_channel::Sender<ScannedEntry>,
    bar: Option<&indicatif::ProgressBar>,
    collect_metadata: bool,
) -> io::Result<StreamScanResult> {
    let mut depth_buckets: Vec<Vec<ScannedEntry>> = Vec::new();
    let mut stats = ScanStats::default();
    let mut count: u64 = 0;

    for res in jwalk::WalkDir::new(path)
        .follow_links(false)
        .skip_hidden(false)
        .sort(false)
        .parallelism(jwalk::Parallelism::RayonNewPool(scan_parallelism()))
        .into_iter()
    {
        // Pause checkpoint: blocks the scan (and via channel backpressure the
        // deletion workers) while the GUI Pause button is active.
        crate::stop::wait_if_paused();
        if crate::stop::is_stop_requested() {
            if let Some(b) = bar {
                b.finish_with_message("Cancelled");
            }
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cancelled by user",
            ));
        }
        // An unreadable entry (permissions, vanished mid-walk) must not abort
        // the whole run: skip it. If it matters, the parent directory removal
        // will fail later and be reported in the failure summary.
        let entry: jwalk::DirEntry<((), ())> = match res {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let p = entry.path();
        let ft = entry.file_type();
        let cached_meta = collect_metadata.then(|| entry.metadata().ok()).flatten();

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
            EntryKind::Dir { depth: entry.depth }
        } else {
            EntryKind::File
        };

        let (file_size, modified_time) = if let Some(m) = cached_meta {
            {
                let meta: std::fs::Metadata = m;
                (Some(meta.len()), meta.modified().ok())
            }
        } else {
            (None, None)
        };

        let scanned = ScannedEntry {
            path: p,
            kind,
            file_size,
            modified_time,
        };

        match &scanned.kind {
            EntryKind::File => {
                stats.files += 1;
                let _ = file_tx.send(scanned);
            }
            EntryKind::Symlink => {
                stats.symlinks += 1;
                let _ = file_tx.send(scanned);
            }
            EntryKind::Dir { depth: 0 } => {}
            EntryKind::Dir { depth } => {
                stats.dirs += 1;
                if *depth >= depth_buckets.len() {
                    depth_buckets.resize_with(*depth + 1, Vec::new);
                }
                depth_buckets[*depth].push(scanned);
            }
        }

        count += 1;
        if let Some(b) = bar.filter(|_| count.is_multiple_of(SCAN_PROGRESS_UPDATE_INTERVAL)) {
            b.set_position(count);
        }
    }

    let dirs_by_depth: Vec<DirBatch> = depth_buckets
        .into_iter()
        .enumerate()
        .rev()
        .filter(|(_, entries)| !entries.is_empty())
        .map(|(depth, entries)| DirBatch { depth, entries })
        .collect();

    Ok(StreamScanResult {
        dirs_by_depth,
        stats,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers (batch / non-streaming path)
// ---------------------------------------------------------------------------

fn scan_directory_plan_inner(
    path: &Path,
    external_bar: Option<&indicatif::ProgressBar>,
    collect_metadata: bool,
    show_progress: bool,
) -> io::Result<ScanPlan> {
    let mut files_and_links: Vec<ScannedEntry> = Vec::with_capacity(INITIAL_FILES_CAPACITY);
    let mut depth_buckets: Vec<Vec<ScannedEntry>> = Vec::new();
    let mut stats = ScanStats::default();

    walk_entries_with(
        path,
        external_bar,
        collect_metadata,
        show_progress,
        |entry| match &entry.kind {
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
        },
    )?;

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
    walk_entries_with(path, external_bar, true, true, |entry| entries.push(entry))?;
    Ok(entries)
}

fn walk_entries_with(
    path: &Path,
    external_bar: Option<&indicatif::ProgressBar>,
    collect_metadata: bool,
    show_progress: bool,
    mut on_entry: impl FnMut(ScannedEntry),
) -> io::Result<()> {
    let local_bar;
    let bar = if let Some(ext) = external_bar {
        Some(ext)
    } else if !show_progress {
        None
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
        Some(&local_bar)
    };

    let mut count: u64 = 0;
    for res in jwalk::WalkDir::new(path)
        .follow_links(false)
        .skip_hidden(false)
        .sort(false)
        .parallelism(jwalk::Parallelism::RayonNewPool(scan_parallelism()))
        .into_iter()
    {
        if crate::stop::is_stop_requested() {
            if let Some(bar) = bar {
                bar.finish_with_message("Cancelled");
            }
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cancelled by user",
            ));
        }
        // Skip unreadable entries instead of aborting the scan (see
        // scan_into_channel for rationale).
        let entry: jwalk::DirEntry<((), ())> = match res {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let p = entry.path();
        let ft = entry.file_type();
        let cached_meta = collect_metadata.then(|| entry.metadata().ok()).flatten();

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
            EntryKind::Dir { depth: entry.depth }
        } else {
            EntryKind::File
        };

        let (file_size, modified_time) = if let Some(m) = cached_meta {
            {
                let meta: std::fs::Metadata = m;
                (Some(meta.len()), meta.modified().ok())
            }
        } else {
            (None, None)
        };

        on_entry(ScannedEntry {
            path: p,
            kind,
            file_size,
            modified_time,
        });

        count += 1;
        if let Some(bar) = bar.filter(|_| count.is_multiple_of(SCAN_PROGRESS_UPDATE_INTERVAL)) {
            bar.set_position(count);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn create_test_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("zap-scan-test-{pid}-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_scan_directory_returns_all_entries() {
        let temp = create_test_dir();
        File::create(temp.join("a.txt")).unwrap();
        File::create(temp.join("b.txt")).unwrap();
        let sub = temp.join("sub");
        fs::create_dir(&sub).unwrap();
        File::create(sub.join("c.txt")).unwrap();

        let entries = scan_directory(&temp).unwrap();
        assert_eq!(entries.len(), 5); // 3 files + 1 subdir + root
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_scan_plan_classifies_files_and_dirs() {
        let temp = create_test_dir();
        File::create(temp.join("file.txt")).unwrap();
        let sub = temp.join("subdir");
        fs::create_dir(&sub).unwrap();

        let plan = scan_directory_plan(&temp).unwrap();
        assert_eq!(plan.stats.files, 1);
        assert_eq!(plan.stats.dirs, 1);
        assert_eq!(plan.stats.symlinks, 0);
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_scan_plan_root_depth0_not_in_dirs() {
        let temp = create_test_dir();
        let plan = scan_directory_plan(&temp).unwrap();
        assert!(plan.dirs_by_depth.is_empty() || plan.dirs_by_depth.iter().all(|b| b.depth > 0));
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_scan_plan_dirs_sorted_deepest_first() {
        let temp = create_test_dir();
        let d1 = temp.join("d1");
        let d2 = d1.join("d2");
        let d3 = d2.join("d3");
        fs::create_dir_all(&d3).unwrap();

        let plan = scan_directory_plan(&temp).unwrap();
        let depths = plan.dir_depths_desc();
        for w in depths.windows(2) {
            assert!(w[0] >= w[1], "dirs should be deepest-first");
        }
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    #[cfg(windows)]
    fn test_scan_directory_junction_classified_as_symlink() {
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
    fn test_scan_plan_filter_removes_excluded_files() {
        use crate::filter::FilterConfig;
        use glob::Pattern;

        let dir = create_test_dir();
        fs::write(dir.join("keep.txt"), b"keep").unwrap();
        fs::write(dir.join("junk.tmp"), b"junk").unwrap();
        fs::create_dir(dir.join("subdir")).unwrap();
        fs::write(dir.join("subdir").join("nested.tmp"), b"nested").unwrap();

        let plan = scan_directory_plan(&dir).unwrap();
        assert_eq!(plan.stats.files, 3);

        let cfg = FilterConfig {
            excludes: vec![Pattern::new("*.tmp").unwrap()],
            ..Default::default()
        };
        let filtered = plan.apply_filter(&dir, &cfg);
        assert_eq!(filtered.stats.files, 1);
        assert_eq!(filtered.stats.dirs, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_plan_filter_drops_empty_dirs_after_file_filtering() {
        use crate::filter::FilterConfig;
        use glob::Pattern;

        let dir = create_test_dir();
        let subdir = dir.join("sub");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("data.tmp"), b"x").unwrap();

        let plan = scan_directory_plan(&dir).unwrap();
        let cfg = FilterConfig {
            excludes: vec![Pattern::new("**/*.tmp").unwrap()],
            ..Default::default()
        };
        let filtered = plan.apply_filter(&dir, &cfg);
        assert_eq!(filtered.stats.files, 0);
        assert_eq!(filtered.stats.dirs, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_plan_for_name_filters_skips_metadata_collection() {
        use crate::filter::FilterConfig;
        use glob::Pattern;

        let dir = create_test_dir();
        fs::write(dir.join("keep.txt"), b"keep").unwrap();
        let cfg = FilterConfig {
            includes: vec![Pattern::new("*.txt").unwrap()],
            ..Default::default()
        };

        let plan = scan_directory_plan_for_filter(&dir, &cfg).unwrap();
        let file = plan
            .files_and_links
            .iter()
            .find(|entry| entry.path.ends_with("keep.txt"))
            .unwrap();
        assert_eq!(file.file_size, None);
        assert_eq!(file.modified_time, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_plan_for_size_filter_collects_metadata() {
        use crate::filter::FilterConfig;

        let dir = create_test_dir();
        fs::write(dir.join("keep.txt"), b"keep").unwrap();
        let cfg = FilterConfig {
            min_size: Some(1),
            ..Default::default()
        };

        let plan = scan_directory_plan_for_filter(&dir, &cfg).unwrap();
        let file = plan
            .files_and_links
            .iter()
            .find(|entry| entry.path.ends_with("keep.txt"))
            .unwrap();
        assert_eq!(file.file_size, Some(4));
        assert!(file.modified_time.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_into_channel_streams_files_immediately() {
        let dir = create_test_dir();
        for i in 0..10 {
            fs::write(dir.join(format!("file{i}.txt")), b"x").unwrap();
        }
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("nested.txt"), b"y").unwrap();

        let (tx, rx) = crossbeam_channel::unbounded();
        let result = scan_into_channel(&dir, &tx, None, false).unwrap();
        drop(tx);

        let received: Vec<_> = rx.iter().collect();
        assert_eq!(received.len(), 11); // 10 + 1 nested
        assert_eq!(result.stats.files, 11);
        assert_eq!(result.stats.dirs, 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
