//! Application state and background-worker logic for the zapg dialog.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use zap::delete::DeleteOptions;
use zap::journal::{self, JournalAction, PathOutcome};
use zap::{batch, delete, path_utils, protect, size, stop};

use crate::MAX_THREADS;

pub type BatchCollectionResult = Option<(Vec<PathBuf>, Option<BatchSession>)>;
pub type BatchReceiver = Receiver<BatchCollectionResult>;

pub const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct BatchSession {
    pub paths_dir: PathBuf,
    pub lock_file: PathBuf,
    pub lock: Option<File>,
    pub last_path_count: usize,
}

impl Drop for BatchSession {
    fn drop(&mut self) {
        drop(self.lock.take());
        let _ = fs::remove_dir_all(&self.paths_dir);
        let _ = fs::remove_file(&self.lock_file);
    }
}

#[derive(Clone)]
pub struct DeleteItem {
    pub path: PathBuf,
    pub state: ItemState,
    pub progress: Option<ProgressSnapshot>,
}

#[derive(Clone)]
pub enum ItemState {
    Pending,
    Running,
    Done,
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub position: u64,
    pub length: Option<u64>,
    pub message: String,
    /// Set for bulk file-root runs where `position`/`length` already track
    /// the *whole selection*, not a single item. The aggregate progress bar
    /// must use this fraction directly instead of weighting it per item.
    pub bulk: bool,
}

#[derive(Default)]
pub struct StatusCounts {
    pub pending: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
}

pub enum WorkerEvent {
    Started(PathBuf),
    Progress(PathBuf, ProgressSnapshot),
    Done(PathBuf, Result<(), String>),
    Finished(Duration),
}

/// Treemap payload prepared once by the collection thread: entries are
/// pre-sorted by size descending and truncated to the layout cap, and the
/// total is pre-computed — so per-frame rendering never sorts or sums the
/// (potentially huge) raw entry list.
pub struct TreemapSnapshot {
    pub entries: Vec<(PathBuf, u64)>,
    pub total: u64,
}

/// Treemap data shared with the collection thread.
pub type TreemapData = Arc<Mutex<Option<TreemapSnapshot>>>;

/// Per-root byte sizes computed by the background size thread. Used both for
/// the size badge (sum) and for byte-weighted progress/ETA: on mixed
/// selections (a few huge folders + many small files) weighting by bytes is
/// far more accurate than weighting every item equally.
///
/// The map and total are built once by the size thread so the per-frame
/// progress code does O(1) lookups instead of rebuilding an index and
/// re-summing thousands of entries on every repaint.
pub struct SizeSnapshot {
    pub by_path: std::collections::HashMap<PathBuf, u64>,
    pub total: u64,
}

pub type ItemSizes = Arc<Mutex<Option<SizeSnapshot>>>;

pub struct ZapApp {
    pub items: Vec<DeleteItem>,
    pub threads_enabled: bool,
    pub threads_text: String,
    pub dry_run: bool,
    pub receiver: Option<Receiver<WorkerEvent>>,
    pub started_at: Option<Instant>,
    pub finished: Option<Duration>,
    pub total_size_calculating: bool,
    pub total_size: Arc<Mutex<Option<u64>>>,
    /// Generation guard prevents an obsolete background traversal from
    /// publishing results after the selection changed.
    pub size_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Per-root sizes from the same background pass as `total_size`.
    pub item_sizes: ItemSizes,
    /// Time the current pause began (None while running normally).
    pub paused_at: Option<Instant>,
    /// Total time spent paused in this run — subtracted from elapsed so the
    /// ETA and the timer only count active work.
    pub paused_total: Duration,
    pub batch_session: Option<BatchSession>,
    pub last_batch_poll: Instant,
    pub has_dangerous_paths: bool,
    pub danger_confirmed: bool,
    pub recycle: bool,
    pub shred: bool,
    pub no_journal: bool,
    /// Taskbar progress mirror (Windows only). Lazily created on the UI
    /// thread once the window exists; `None` + `taskbar_unavailable` set
    /// means COM failed and we stop retrying.
    #[cfg(windows)]
    pub taskbar: Option<crate::taskbar::TaskbarProgress>,
    #[cfg(windows)]
    pub taskbar_unavailable: bool,
    pub show_treemap: bool,
    pub treemap_data: TreemapData,
    pub treemap_collecting: bool,
    /// Batch result receiver — window opens instantly, batch paths arrive
    /// asynchronously from the background collection thread.
    pub batch_collecting: Option<BatchReceiver>,
}

impl ZapApp {
    pub fn new(
        paths: Vec<PathBuf>,
        threads: Option<usize>,
        batch_session: Option<BatchSession>,
    ) -> Self {
        let has_dangerous = paths.iter().any(|p| protect::is_dangerous_target(p));

        let total_size = Arc::new(Mutex::new(None));
        let item_sizes: ItemSizes = Arc::new(Mutex::new(None));
        let size_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
        spawn_size_calculation(
            Arc::clone(&total_size),
            Arc::clone(&item_sizes),
            Arc::clone(&size_generation),
            1,
            paths.clone(),
        );

        Self {
            items: paths.into_iter().map(DeleteItem::pending).collect(),
            threads_enabled: threads.is_some(),
            threads_text: threads
                .unwrap_or_else(|| zap::parallelism::worker_count(None))
                .to_string(),
            dry_run: false,
            recycle: false,
            shred: false,
            no_journal: false,
            #[cfg(windows)]
            taskbar: None,
            #[cfg(windows)]
            taskbar_unavailable: false,
            receiver: None,
            started_at: None,
            finished: None,
            total_size_calculating: true,
            total_size,
            size_generation,
            item_sizes,
            paused_at: None,
            paused_total: Duration::ZERO,
            batch_session,
            last_batch_poll: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
            has_dangerous_paths: has_dangerous,
            danger_confirmed: false,
            show_treemap: false,
            treemap_data: Arc::new(Mutex::new(None)),
            treemap_collecting: false,
            batch_collecting: None,
        }
    }

    /// Create app with batch results arriving asynchronously — window opens
    /// instantly while batch paths are still being collected.
    pub fn new_collecting(
        initial_paths: Vec<PathBuf>,
        threads: Option<usize>,
        batch_session: Option<BatchSession>,
        batch_rx: BatchReceiver,
    ) -> Self {
        let mut app = Self::new(initial_paths, threads, batch_session);
        app.batch_collecting = Some(batch_rx);
        app
    }

    /// Poll the batch background thread for results. Called every frame until
    /// the batch session is established or fails.
    pub fn poll_batch_collection(&mut self) {
        let batch_rx = match self.batch_collecting.take() {
            Some(rx) => rx,
            None => return,
        };
        match batch_rx.try_recv() {
            Ok(Some((paths, session))) => {
                self.items = paths.into_iter().map(DeleteItem::pending).collect();
                // Update dangerous-paths check with collected paths
                self.has_dangerous_paths = self
                    .items
                    .iter()
                    .any(|item| protect::is_dangerous_target(&item.path));
                self.batch_session = session;
                self.recalculate_total_size();
            }
            Ok(None) => {
                // Batch collection failed — keep initial paths
            }
            Err(mpsc::TryRecvError::Empty) => {
                // Not ready yet — put the receiver back
                self.batch_collecting = Some(batch_rx);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                // Thread crashed — keep initial paths
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.receiver.is_some()
    }

    pub fn is_paused(&self) -> bool {
        self.is_running() && stop::is_paused()
    }

    /// Toggle pause/resume for the active run, tracking paused time so the
    /// elapsed timer and ETA only count active work.
    pub fn toggle_pause(&mut self) {
        if !self.is_running() {
            return;
        }
        if stop::is_paused() {
            stop::request_resume();
            if let Some(at) = self.paused_at.take() {
                self.paused_total += at.elapsed();
            }
        } else {
            stop::request_pause();
            self.paused_at = Some(Instant::now());
        }
    }

    /// Elapsed active time of the current/last run, excluding pauses.
    pub fn active_elapsed(&self) -> Option<Duration> {
        if let Some(done) = self.finished {
            return Some(done);
        }
        let raw = self.started_at?.elapsed();
        let paused = self.paused_total + self.paused_at.map(|at| at.elapsed()).unwrap_or_default();
        Some(raw.saturating_sub(paused))
    }

    pub fn thread_error(&self) -> Option<&'static str> {
        if !self.threads_enabled {
            return None;
        }
        match self.threads_text.parse::<usize>() {
            Ok(v) if (1..=MAX_THREADS).contains(&v) => None,
            _ => Some("Thread limit must be between 1 and 64."),
        }
    }

    pub fn parsed_threads(&self) -> Option<usize> {
        if self.thread_error().is_some() || !self.threads_enabled {
            None
        } else {
            self.threads_text.parse::<usize>().ok()
        }
    }

    pub fn start(&mut self) {
        // Clear any stop/pause request left over from a previous run.
        stop::reset();
        self.paused_at = None;
        self.paused_total = Duration::ZERO;
        self.finalize_batch_session();
        let paths: Vec<PathBuf> = self.items.iter().map(|item| item.path.clone()).collect();
        let threads = self.parsed_threads();
        let dry_run = self.dry_run;
        let recycle = self.recycle;
        let shred = self.shred;
        let allow_dangerous = self.danger_confirmed;
        let no_journal = self.no_journal;
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.started_at = Some(Instant::now());
        self.finished = None;
        for item in &mut self.items {
            item.state = ItemState::Pending;
            item.progress = None;
        }
        thread::spawn(move || {
            let start = Instant::now();
            let mut outcomes: Vec<PathOutcome> = Vec::with_capacity(paths.len());
            if should_bulk_delete_file_roots(&paths, dry_run, recycle) {
                outcomes = run_bulk_file_roots_gui(
                    paths,
                    threads,
                    shred,
                    allow_dangerous,
                    sender.clone(),
                );
            } else {
                for path in paths {
                    // Stop button: skip everything not yet started. The item
                    // currently being deleted aborts via the same flag inside
                    // delete_path.
                    if stop::is_stop_requested() {
                        let _ = sender.send(WorkerEvent::Done(
                            path.clone(),
                            Err("cancelled by user".to_owned()),
                        ));
                        outcomes.push((path, Some("cancelled by user".to_owned())));
                        continue;
                    }
                    let _ = sender.send(WorkerEvent::Started(path.clone()));
                    let result = run_delete_path(
                        &path,
                        threads,
                        dry_run,
                        recycle,
                        shred,
                        allow_dangerous,
                        &sender,
                    );
                    let result = normalize_worker_result(result, dry_run);
                    outcomes.push((path.clone(), result.as_ref().err().cloned()));
                    let _ = sender.send(WorkerEvent::Done(path, result));
                }
            }
            // Record the run in the operation journal (audit trail). A dry
            // run changes nothing on disk, so it is not journaled.
            if !dry_run && !no_journal && !journal::is_disabled_by_env() {
                let action = if recycle {
                    JournalAction::Recycle
                } else if shred {
                    JournalAction::Shred
                } else {
                    JournalAction::Delete
                };
                let _ = journal::record(action, &outcomes);
            }
            let _ = sender.send(WorkerEvent::Finished(start.elapsed()));
        });
    }

    pub fn poll_events(&mut self) {
        let mut events = Vec::new();
        let mut clear_receiver = false;
        let mut worker_died = false;
        if let Some(receiver) = &self.receiver {
            // Bound work per frame so a burst from bulk deletion cannot freeze
            // rendering for seconds. Remaining events stay queued for the next
            // 100-ms repaint; terminal events are never discarded.
            const MAX_EVENTS_PER_FRAME: usize = 4_096;
            for _ in 0..MAX_EVENTS_PER_FRAME {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        worker_died = true;
                        break;
                    }
                }
            }
        }
        // Bulk runs deliver thousands of events per frame; a linear scan per
        // event would be O(items × events). Build the path→index map once
        // per drain instead.
        let index: std::collections::HashMap<PathBuf, usize> = if events.is_empty() {
            std::collections::HashMap::new()
        } else {
            self.items
                .iter()
                .enumerate()
                .map(|(i, item)| (item.path.clone(), i))
                .collect()
        };
        let apply = |items: &mut Vec<DeleteItem>,
                     path: &Path,
                     state: Option<ItemState>,
                     progress: Option<Option<ProgressSnapshot>>| {
            if let Some(&i) = index.get(path) {
                if let Some(state) = state {
                    items[i].state = state;
                }
                if let Some(progress) = progress {
                    items[i].progress = progress;
                }
            }
        };
        for event in events {
            match event {
                WorkerEvent::Started(path) => {
                    apply(&mut self.items, &path, Some(ItemState::Running), None)
                }
                WorkerEvent::Progress(path, p) => {
                    apply(&mut self.items, &path, None, Some(Some(p)))
                }
                WorkerEvent::Done(path, Ok(())) => {
                    apply(&mut self.items, &path, Some(ItemState::Done), Some(None));
                }
                WorkerEvent::Done(path, Err(err)) => {
                    apply(
                        &mut self.items,
                        &path,
                        Some(ItemState::Failed(err)),
                        Some(None),
                    );
                }
                WorkerEvent::Finished(duration) => {
                    // Report active time only: pauses are not deletion work.
                    if let Some(at) = self.paused_at.take() {
                        self.paused_total += at.elapsed();
                    }
                    stop::request_resume();
                    self.finished = Some(duration.saturating_sub(self.paused_total));
                    clear_receiver = true;
                }
            }
        }
        // The worker channel disconnected without a Finished event (worker
        // thread panicked). Without this the dialog would spin forever.
        if worker_died && self.finished.is_none() {
            for item in &mut self.items {
                if matches!(item.state, ItemState::Pending | ItemState::Running) {
                    item.state = ItemState::Failed("worker stopped unexpectedly".to_owned());
                    item.progress = None;
                }
            }
            self.finished = Some(self.started_at.map(|s| s.elapsed()).unwrap_or_default());
            clear_receiver = true;
        }
        if clear_receiver {
            self.receiver = None;
        }
    }

    /// Mirror run progress onto the Windows taskbar button. Called every
    /// frame from `update` (UI thread). `fraction` is the same aggregate
    /// value the in-window progress bar shows.
    #[cfg(windows)]
    pub fn update_taskbar(&mut self, fraction: f32, counts: &StatusCounts) {
        if self.taskbar_unavailable {
            return;
        }
        if self.taskbar.is_none() {
            // The window may not exist on the very first frames — retry.
            let Some(hwnd) = crate::taskbar::find_thread_window() else {
                return;
            };
            match crate::taskbar::TaskbarProgress::new(hwnd) {
                Some(tb) => self.taskbar = Some(tb),
                None => {
                    self.taskbar_unavailable = true;
                    return;
                }
            }
        }
        let taskbar = self.taskbar.as_ref().expect("set above");
        if self.is_running() {
            taskbar.set_progress((fraction * 1000.0) as u64, 1000);
        } else if self.finished.is_some() && counts.failed > 0 {
            taskbar.set_error();
        } else {
            taskbar.clear();
        }
    }

    pub fn status_counts(&self) -> StatusCounts {
        let mut counts = StatusCounts::default();
        for item in &self.items {
            match item.state {
                ItemState::Pending => counts.pending += 1,
                ItemState::Running => counts.running += 1,
                ItemState::Done => counts.done += 1,
                ItemState::Failed(_) => counts.failed += 1,
            }
        }
        counts
    }

    pub fn poll_batch_session(&mut self) {
        if self.last_batch_poll.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last_batch_poll = Instant::now();
        // Extract paths_dir early — we can't borrow self while session is borrowed.
        let paths_dir = match &self.batch_session {
            Some(s) => s.paths_dir.clone(),
            None => return,
        };
        if self.is_running() || self.finished.is_some() {
            return;
        }
        // Touch lock through a re-borrow
        if let Some(ref mut session) = self.batch_session {
            if let Some(ref mut lock) = session.lock {
                batch::touch_lock(lock);
            }
        }
        let changed = self.add_batch_paths_from(&paths_dir);
        if let Some(ref mut session) = self.batch_session {
            session.last_path_count = self.items.len();
        }
        if changed {
            self.recalculate_total_size();
        }
    }

    pub fn finalize_batch_session(&mut self) {
        let paths_dir = match &self.batch_session {
            Some(s) => s.paths_dir.clone(),
            None => return,
        };
        if let Some(ref mut session) = self.batch_session {
            if let Some(ref mut lock) = session.lock {
                batch::touch_lock(lock);
            }
        }
        // Never wait for batch quiet on the UI thread. Regular throttled polls
        // have already collected committed files; perform one final snapshot.
        let changed = self.add_batch_paths_from(&paths_dir);
        if let Some(ref mut session) = self.batch_session {
            session.last_path_count = self.items.len();
            session.lock = None;
        }
        if changed {
            self.recalculate_total_size();
        }
    }

    fn add_batch_paths_from(&mut self, paths_dir: &Path) -> bool {
        let mut paths = batch::read_batch_paths(paths_dir);
        path_utils::dedup_paths(&mut paths);
        // Batch sessions can stream thousands of paths across polls; a
        // linear scan per candidate would be quadratic. Dedup via HashSet.
        let existing: std::collections::HashSet<&Path> =
            self.items.iter().map(|item| item.path.as_path()).collect();
        let new_paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|path| !existing.contains(path.as_path()))
            .collect();
        drop(existing);
        let mut changed = false;
        for path in new_paths {
            self.items.push(DeleteItem::pending(path));
            changed = true;
        }
        if changed {
            // Late-arriving batch paths may include protected system paths.
            // Re-arm confirmation so the worker never receives dangerous
            // permission inherited from an earlier, safer selection.
            let has_dangerous = self
                .items
                .iter()
                .any(|item| protect::is_dangerous_target(&item.path));
            if has_dangerous && !self.has_dangerous_paths {
                self.danger_confirmed = false;
            }
            self.has_dangerous_paths = has_dangerous;
        }
        changed
    }

    /// Add paths dropped onto the window (ignored while a run is active).
    /// Duplicates are skipped; dangerous-path detection and the size badge
    /// are refreshed.
    pub fn add_dropped_paths(&mut self, paths: Vec<PathBuf>) {
        if self.is_running() || paths.is_empty() {
            return;
        }
        let existing: std::collections::HashSet<&Path> =
            self.items.iter().map(|item| item.path.as_path()).collect();
        let new_paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|path| !existing.contains(path.as_path()))
            .collect();
        drop(existing);
        let mut added = false;
        for path in new_paths {
            self.items.push(DeleteItem::pending(path));
            added = true;
        }
        if !added {
            return;
        }
        // New selection invalidates the previous result view.
        self.finished = None;
        self.started_at = None;
        self.has_dangerous_paths = self
            .items
            .iter()
            .any(|item| protect::is_dangerous_target(&item.path));
        if self.has_dangerous_paths {
            self.danger_confirmed = false;
        }
        self.recalculate_total_size();
    }

    pub fn recalculate_total_size(&mut self) {
        self.total_size_calculating = true;
        if let Ok(mut total) = self.total_size.lock() {
            *total = None;
        }
        if let Ok(mut sizes) = self.item_sizes.lock() {
            *sizes = None;
        }
        if let Ok(mut treemap) = self.treemap_data.lock() {
            *treemap = None;
        }
        self.treemap_collecting = false;
        let generation = self.size_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let size_paths: Vec<PathBuf> = self.items.iter().map(|item| item.path.clone()).collect();
        spawn_size_calculation(
            Arc::clone(&self.total_size),
            Arc::clone(&self.item_sizes),
            Arc::clone(&self.size_generation),
            generation,
            size_paths,
        );
        if self.show_treemap {
            self.start_treemap_collection();
        }
    }

    /// Kick off background collection of treemap data (immediate children
    /// of each selected root with aggregated sizes — no double counting).
    pub fn start_treemap_collection(&mut self) {
        if self.treemap_collecting || self.treemap_data.lock().unwrap().is_some() {
            return;
        }
        self.treemap_collecting = true;
        let data_handle = Arc::clone(&self.treemap_data);
        let roots: Vec<PathBuf> = self.items.iter().map(|i| i.path.clone()).collect();
        thread::spawn(move || {
            let mut entries = collect_treemap_entries(&roots);
            // Sort/sum once here so every render frame works on a small,
            // ready-to-layout slice instead of the raw entry list.
            let total: u64 = entries.iter().map(|(_, sz)| sz).sum();
            entries.sort_by_key(|(_, sz)| std::cmp::Reverse(*sz));
            entries.truncate(zap::treemap::MAX_LAYOUT_ITEMS);
            *data_handle.lock().unwrap() = Some(TreemapSnapshot { entries, total });
        });
    }
}

fn should_bulk_delete_file_roots(paths: &[PathBuf], dry_run: bool, recycle: bool) -> bool {
    if dry_run || recycle || paths.len() < 64 {
        return false;
    }
    paths.iter().all(|path| {
        fs::symlink_metadata(path)
            .map(|meta| meta.is_file() || meta.file_type().is_symlink())
            .unwrap_or(false)
    })
}

fn run_bulk_file_roots_gui(
    paths: Vec<PathBuf>,
    threads: Option<usize>,
    shred: bool,
    allow_dangerous: bool,
    sender: Sender<WorkerEvent>,
) -> Vec<PathOutcome> {
    let total = paths.len() as u64;
    for path in &paths {
        let _ = sender.send(WorkerEvent::Started(path.clone()));
    }

    let bar = indicatif::ProgressBar::hidden();
    bar.set_length(total);
    let monitor_bar = bar.clone();
    let monitor_sender = sender.clone();
    let monitor_paths = paths.clone();
    let monitor_stop = Arc::new(AtomicBool::new(false));
    let monitor_done = Arc::clone(&monitor_stop);
    let monitor = thread::spawn(move || {
        while !monitor_done.load(Ordering::Relaxed) {
            send_bulk_progress(&monitor_sender, &monitor_paths, &monitor_bar, total);
            thread::sleep(PROGRESS_POLL_INTERVAL);
        }
        send_bulk_progress(&monitor_sender, &monitor_paths, &monitor_bar, total);
    });

    let worker_count = zap::parallelism::worker_count(threads);
    let summary = match rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .build()
    {
        Ok(pool) => pool.install(|| {
            delete::delete_file_roots_bulk(&paths, shred, allow_dangerous, Some(&bar))
        }),
        Err(err) => delete::BulkDeleteSummary {
            deleted: 0,
            errors: paths
                .iter()
                .cloned()
                .map(|path| (path, format!("failed to configure thread pool: {err}")))
                .collect(),
        },
    };
    monitor_stop.store(true, Ordering::Relaxed);
    let _ = monitor.join();

    let failed: std::collections::HashMap<PathBuf, String> = summary.errors.into_iter().collect();
    let mut outcomes: Vec<PathOutcome> = Vec::with_capacity(paths.len());
    for path in paths {
        let error = failed.get(&path).cloned();
        let _ = sender.send(WorkerEvent::Done(
            path.clone(),
            error.clone().map_or(Ok(()), Err),
        ));
        outcomes.push((path, error));
    }
    outcomes
}

fn send_bulk_progress(
    sender: &Sender<WorkerEvent>,
    paths: &[PathBuf],
    bar: &indicatif::ProgressBar,
    total: u64,
) {
    let position = bar.position();
    let message = format!("Bulk deleting files ({position}/{total})");
    for path in paths.iter().take(8) {
        let _ = sender.send(WorkerEvent::Progress(
            path.clone(),
            ProgressSnapshot {
                position,
                length: Some(total),
                message: message.clone(),
                bulk: true,
            },
        ));
    }
}

impl DeleteItem {
    fn pending(path: PathBuf) -> Self {
        Self {
            path,
            state: ItemState::Pending,
            progress: None,
        }
    }
}

fn spawn_size_calculation(
    total: Arc<Mutex<Option<u64>>>,
    sizes: ItemSizes,
    generation: Arc<std::sync::atomic::AtomicU64>,
    expected_generation: u64,
    paths: Vec<PathBuf>,
) {
    thread::spawn(move || {
        use rayon::prelude::*;
        // One pass computes both the per-root sizes (byte-weighted ETA) and
        // their sum (size badge). A private bounded pool prevents this optional
        // preview from competing with deletion through Rayon's global pool.
        let workers = zap::parallelism::worker_count(None);
        let pool = match rayon::ThreadPoolBuilder::new().num_threads(workers).build() {
            Ok(pool) => pool,
            Err(_) => return,
        };
        let per_root: std::collections::HashMap<PathBuf, u64> = pool.install(|| {
            paths
                .into_par_iter()
                .map(|p| {
                    let sz = size::dir_size_recursive(&p);
                    (p, sz)
                })
                .collect()
        });
        let sum: u64 = per_root.values().sum();
        if generation.load(Ordering::Acquire) != expected_generation {
            return;
        }
        if let Ok(mut guard) = sizes.lock() {
            *guard = Some(SizeSnapshot {
                by_path: per_root,
                total: sum,
            });
        }
        if let Ok(mut guard) = total.lock() {
            *guard = Some(sum);
        }
    });
}

/// Build treemap entries from the selected roots. For each directory root
/// the *immediate children* are used (sizes already aggregated by
/// `dir_size_tree`), so the rectangles partition the selection without
/// double counting files inside their parent directories.
fn collect_treemap_entries(roots: &[PathBuf]) -> Vec<(PathBuf, u64)> {
    let mut entries = Vec::new();
    for root in roots {
        match fs::symlink_metadata(root) {
            Ok(meta) if meta.is_dir() => {
                if let Ok(tree) = size::dir_size_tree(root) {
                    let children: Vec<(PathBuf, u64)> = tree
                        .iter()
                        .filter(|(p, _)| p.parent() == Some(root.as_path()))
                        .cloned()
                        .collect();
                    if children.is_empty() {
                        // Empty dir or unreadable children — show the root itself.
                        if let Some(total) = tree.iter().find(|(p, _)| p == root).map(|(_, sz)| *sz)
                        {
                            entries.push((root.clone(), total));
                        }
                    } else {
                        entries.extend(children);
                    }
                }
            }
            Ok(meta) => entries.push((root.clone(), meta.len())),
            Err(_) => {}
        }
    }
    entries
}

pub fn run_delete_path(
    path: &Path,
    threads: Option<usize>,
    dry_run: bool,
    recycle: bool,
    shred: bool,
    allow_dangerous: bool,
    sender: &Sender<WorkerEvent>,
) -> io::Result<()> {
    if dry_run {
        return delete::dry_run_path_silent(path);
    }
    let bar = indicatif::ProgressBar::hidden();
    let monitor_bar = bar.clone();
    let monitor_path = path.to_path_buf();
    let monitor_sender = sender.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let monitor_stop = Arc::clone(&stop);
    let monitor = thread::spawn(move || {
        while !monitor_stop.load(Ordering::Relaxed) {
            send_progress(&monitor_sender, &monitor_path, &monitor_bar);
            thread::sleep(PROGRESS_POLL_INTERVAL);
        }
        send_progress(&monitor_sender, &monitor_path, &monitor_bar);
    });
    let mut opts = DeleteOptions::default().with_threads(threads).with_bar(bar);
    if allow_dangerous {
        opts = opts.allow_dangerous();
    }
    if recycle {
        opts = opts.recycle();
    }
    if shred {
        opts = opts.shred();
    }
    let result = delete::delete_path(path, opts);
    stop.store(true, Ordering::Relaxed);
    let _ = monitor.join();
    result
}

fn send_progress(sender: &Sender<WorkerEvent>, path: &Path, bar: &indicatif::ProgressBar) {
    let _ = sender.send(WorkerEvent::Progress(
        path.to_path_buf(),
        ProgressSnapshot {
            position: bar.position(),
            length: bar.length(),
            message: bar.message(),
            bulk: false,
        },
    ));
}

pub fn normalize_worker_result(result: io::Result<()>, dry_run: bool) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(err) if !dry_run && err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

pub fn should_auto_close(app: &ZapApp, counts: &StatusCounts) -> bool {
    app.finished.is_some() && !app.dry_run && counts.failed == 0 && counts.done > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bulk_file_roots_requires_many_plain_files() {
        let dir = std::env::temp_dir().join(format!("zapg-bulk-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let few: Vec<PathBuf> = (0..4)
            .map(|i| {
                let path = dir.join(format!("file-{i}.txt"));
                fs::write(&path, b"x").unwrap();
                path
            })
            .collect();
        assert!(!should_bulk_delete_file_roots(&few, false, false));

        let many: Vec<PathBuf> = (0..64)
            .map(|i| {
                let path = dir.join(format!("many-{i}.txt"));
                fs::write(&path, b"x").unwrap();
                path
            })
            .collect();
        assert!(should_bulk_delete_file_roots(&many, false, false));
        assert!(!should_bulk_delete_file_roots(&many, true, false));
        assert!(!should_bulk_delete_file_roots(&many, false, true));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn destructive_delete_treats_missing_paths_as_already_done() {
        let result = normalize_worker_result(
            Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
            false,
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn dry_run_keeps_missing_paths_visible() {
        let result = normalize_worker_result(
            Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
            true,
        );
        assert_eq!(result, Err("missing".to_owned()));
    }

    #[test]
    fn destructive_success_closes_dialog() {
        let mut app = ZapApp::new(vec![PathBuf::from("deleted")], None, None);
        app.finished = Some(Duration::from_millis(1));
        app.items[0].state = ItemState::Done;
        assert!(should_auto_close(&app, &app.status_counts()));
    }

    #[test]
    fn destructive_failure_keeps_dialog_open() {
        let mut app = ZapApp::new(vec![PathBuf::from("locked")], None, None);
        app.finished = Some(Duration::from_millis(1));
        app.items[0].state = ItemState::Failed("locked".to_owned());
        assert!(!should_auto_close(&app, &app.status_counts()));
    }

    #[test]
    fn dry_run_success_keeps_dialog_open() {
        let mut app = ZapApp::new(vec![PathBuf::from("previewed")], None, None);
        app.dry_run = true;
        app.finished = Some(Duration::from_millis(1));
        app.items[0].state = ItemState::Done;
        assert!(!should_auto_close(&app, &app.status_counts()));
    }

    #[test]
    fn batch_poll_adds_late_path_even_if_file_sorts_before_existing() {
        let root =
            std::env::temp_dir().join(format!("zapg-batch-poll-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let paths_dir = root.join("paths");
        fs::create_dir_all(&paths_dir).unwrap();
        let lock_file = root.join("lock");
        let existing = PathBuf::from("existing");
        let late = PathBuf::from("late");
        batch::write_batch_paths(&paths_dir, std::slice::from_ref(&existing)).unwrap();
        batch::write_batch_paths(&paths_dir, std::slice::from_ref(&late)).unwrap();
        let lock = batch::try_acquire_lock(&lock_file).unwrap();
        let mut app = ZapApp::new(
            vec![existing.clone()],
            None,
            Some(BatchSession {
                paths_dir: paths_dir.clone(),
                lock_file: lock_file.clone(),
                lock: Some(lock),
                last_path_count: 1,
            }),
        );
        app.poll_batch_session();
        assert!(app.items.iter().any(|item| item.path == late));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn treemap_entries_use_immediate_children_without_double_counting() {
        let root = std::env::temp_dir().join(format!(
            "zapg-treemap-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(root.join("top.bin"), vec![0u8; 10]).unwrap();
        fs::write(sub.join("nested.bin"), vec![0u8; 30]).unwrap();

        let entries = collect_treemap_entries(std::slice::from_ref(&root));
        let total: u64 = entries.iter().map(|(_, sz)| sz).sum();
        assert_eq!(total, 40, "children must partition the root exactly");
        assert!(entries.iter().any(|(p, sz)| p == &sub && *sz == 30));
        assert!(entries
            .iter()
            .any(|(p, sz)| p.ends_with("top.bin") && *sz == 10));
        // The root itself must NOT be present alongside its children.
        assert!(!entries.iter().any(|(p, _)| p == &root));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn treemap_entries_for_file_root_uses_file_size() {
        let root =
            std::env::temp_dir().join(format!("zapg-treemap-file-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let file = root.join("alone.bin");
        fs::write(&file, vec![0u8; 17]).unwrap();

        let entries = collect_treemap_entries(std::slice::from_ref(&file));
        assert_eq!(entries, vec![(file.clone(), 17)]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_disconnect_without_finish_marks_items_failed() {
        let mut app = ZapApp::new(vec![PathBuf::from("stuck")], None, None);
        let (sender, receiver) = mpsc::channel::<WorkerEvent>();
        app.receiver = Some(receiver);
        app.started_at = Some(Instant::now());
        app.items[0].state = ItemState::Running;
        drop(sender); // simulate worker thread panic
        app.poll_events();
        assert!(app.finished.is_some(), "dialog must not spin forever");
        assert!(matches!(app.items[0].state, ItemState::Failed(_)));
        assert!(app.receiver.is_none());
    }
}
