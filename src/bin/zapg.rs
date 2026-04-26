#![cfg_attr(windows, windows_subsystem = "windows")]

use eframe::egui;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zap::delete::{self, DeleteOptions};

const APP_NAME: &str = "Zap";
const BATCH_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const BATCH_QUIET_POLLS: usize = 4;
const BATCH_MAX_POLLS: usize = 100;
const MAX_THREADS: usize = 1024;
const WINDOW_SIZE: [f32; 2] = [400.0, 230.0];

fn main() -> eframe::Result<()> {
    let args = parse_args();
    let paths = if args.batch {
        match collect_batch_paths(&args.paths) {
            Some(paths) => paths,
            None => return Ok(()),
        }
    } else {
        args.paths
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_inner_size(WINDOW_SIZE)
        .with_min_inner_size(WINDOW_SIZE)
        .with_max_inner_size(WINDOW_SIZE)
        .with_resizable(false)
        .with_maximize_button(false);

    if let Some(position) = initial_window_position() {
        viewport = viewport.with_position(position);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            Ok(Box::new(ZapApp::new(paths, args.threads)))
        }),
    )
}

#[cfg(windows)]
fn initial_window_position() -> Option<egui::Pos2> {
    use std::ffi::c_void;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct MonitorInfo {
        cbSize: u32,
        rcMonitor: Rect,
        rcWork: Rect,
        dwFlags: u32,
    }

    type HMonitor = *mut c_void;
    const MONITOR_DEFAULTTONEAREST: u32 = 2;

    extern "system" {
        fn GetCursorPos(lpPoint: *mut Point) -> i32;
        fn MonitorFromPoint(pt: Point, dwFlags: u32) -> HMonitor;
        fn GetMonitorInfoW(hMonitor: HMonitor, lpmi: *mut MonitorInfo) -> i32;
    }

    let mut cursor = Point { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        return None;
    }

    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }

    let mut info = MonitorInfo {
        cbSize: std::mem::size_of::<MonitorInfo>() as u32,
        rcMonitor: Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rcWork: Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dwFlags: 0,
    };

    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }

    let work = info.rcWork;
    let x = work.left + ((work.right - work.left) - WINDOW_SIZE[0] as i32) / 2;
    let y = work.top + ((work.bottom - work.top) - WINDOW_SIZE[1] as i32) / 2;
    Some(egui::pos2(x.max(work.left) as f32, y.max(work.top) as f32))
}

#[cfg(not(windows))]
fn initial_window_position() -> Option<egui::Pos2> {
    None
}

#[derive(Default)]
struct GuiArgs {
    batch: bool,
    threads: Option<usize>,
    paths: Vec<PathBuf>,
}

fn parse_args() -> GuiArgs {
    let mut parsed = GuiArgs::default();
    let mut args = std::env::args_os().skip(1);

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--batch") => parsed.batch = true,
            Some("--threads") | Some("-j") => {
                if let Some(value) = args.next() {
                    parsed.threads = value
                        .to_str()
                        .and_then(|value| value.parse::<usize>().ok())
                        .filter(|value| (1..=MAX_THREADS).contains(value));
                }
            }
            _ => parsed.paths.push(PathBuf::from(arg)),
        }
    }

    dedup_paths(&mut parsed.paths);
    parsed
}

fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::with_capacity(paths.len());
    paths.retain(|path| seen.insert(path.clone()));
}

fn collect_batch_paths(paths: &[PathBuf]) -> Option<Vec<PathBuf>> {
    let temp = std::env::temp_dir();
    let paths_dir = temp.join("zapg-batch-paths.d");
    let lock_file = temp.join("zapg-batch.lock");

    cleanup_stale_batch(&paths_dir, &lock_file);
    if write_batch_paths(&paths_dir, paths).is_err() {
        return Some(paths.to_vec());
    }

    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_file)
    {
        Ok(lock) => {
            wait_for_batch_quiet(&paths_dir);
            drop(lock);
            let mut collected = read_batch_paths(&paths_dir);
            dedup_paths(&mut collected);
            let _ = fs::remove_dir_all(&paths_dir);
            let _ = fs::remove_file(&lock_file);
            Some(collected)
        }
        Err(_) => None,
    }
}

fn cleanup_stale_batch(paths_dir: &Path, lock_file: &Path) {
    if let Ok(meta) = fs::metadata(lock_file) {
        if let Ok(modified) = meta.modified() {
            if modified.elapsed().unwrap_or_default() > Duration::from_secs(10) {
                let _ = fs::remove_dir_all(paths_dir);
                let _ = fs::remove_file(lock_file);
            }
        }
    }
}

fn write_batch_paths(paths_dir: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
    fs::create_dir_all(paths_dir)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file = paths_dir.join(format!("{}-{nanos}.txt", std::process::id()));
    let mut out = OpenOptions::new().create_new(true).write(true).open(file)?;
    for path in paths {
        writeln!(out, "{}", path.display())?;
    }
    Ok(())
}

fn wait_for_batch_quiet(paths_dir: &Path) {
    let mut last_count = usize::MAX;
    let mut quiet = 0;

    for _ in 0..BATCH_MAX_POLLS {
        let count = fs::read_dir(paths_dir)
            .map(|entries| entries.count())
            .unwrap_or(0);
        if count == last_count {
            quiet += 1;
            if quiet >= BATCH_QUIET_POLLS {
                break;
            }
        } else {
            quiet = 0;
            last_count = count;
        }
        thread::sleep(BATCH_POLL_INTERVAL);
    }
}

fn read_batch_paths(paths_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = match fs::read_dir(paths_dir) {
        Ok(entries) => entries.filter_map(Result::ok).collect(),
        Err(_) => return Vec::new(),
    };
    files.sort_by_key(|entry| entry.file_name());

    let mut paths = Vec::new();
    for file in files {
        if let Ok(content) = fs::read_to_string(file.path()) {
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    paths.push(PathBuf::from(trimmed));
                }
            }
        }
    }
    paths
}

#[derive(Clone)]
struct DeleteItem {
    path: PathBuf,
    state: ItemState,
    progress: Option<ProgressSnapshot>,
}

#[derive(Clone)]
enum ItemState {
    Pending,
    Running,
    Done,
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ProgressSnapshot {
    position: u64,
    length: Option<u64>,
    message: String,
}

struct ZapApp {
    items: Vec<DeleteItem>,
    threads_enabled: bool,
    threads_text: String,
    dry_run: bool,
    receiver: Option<Receiver<WorkerEvent>>,
    started_at: Option<Instant>,
    finished: Option<Duration>,
}

impl ZapApp {
    fn new(paths: Vec<PathBuf>, threads: Option<usize>) -> Self {
        Self {
            items: paths
                .into_iter()
                .map(|path| DeleteItem {
                    path,
                    state: ItemState::Pending,
                    progress: None,
                })
                .collect(),
            threads_enabled: threads.is_some(),
            threads_text: threads.unwrap_or(4).to_string(),
            dry_run: false,
            receiver: None,
            started_at: None,
            finished: None,
        }
    }

    fn is_running(&self) -> bool {
        self.receiver.is_some()
    }

    fn thread_error(&self) -> Option<&'static str> {
        if !self.threads_enabled {
            return None;
        }
        match self.threads_text.parse::<usize>() {
            Ok(value) if (1..=MAX_THREADS).contains(&value) => None,
            _ => Some("Thread limit must be between 1 and 1024."),
        }
    }

    fn parsed_threads(&self) -> Option<usize> {
        if self.thread_error().is_some() || !self.threads_enabled {
            None
        } else {
            self.threads_text.parse::<usize>().ok()
        }
    }

    fn start(&mut self) {
        let paths: Vec<PathBuf> = self.items.iter().map(|item| item.path.clone()).collect();
        let threads = self.parsed_threads();
        let dry_run = self.dry_run;
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
            for path in paths {
                let _ = sender.send(WorkerEvent::Started(path.clone()));
                let result = run_delete_path(&path, threads, dry_run, &sender);
                let _ = sender.send(WorkerEvent::Done(
                    path,
                    normalize_worker_result(result, dry_run),
                ));
            }
            let _ = sender.send(WorkerEvent::Finished(start.elapsed()));
        });
    }

    fn poll_events(&mut self) {
        let mut events = Vec::new();
        let mut clear_receiver = false;
        if let Some(receiver) = &self.receiver {
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
        }

        for event in events {
            match event {
                WorkerEvent::Started(path) => self.set_state(&path, ItemState::Running),
                WorkerEvent::Progress(path, progress) => self.set_progress(&path, progress),
                WorkerEvent::Done(path, Ok(())) => {
                    self.set_progress_none(&path);
                    self.set_state(&path, ItemState::Done);
                }
                WorkerEvent::Done(path, Err(err)) => {
                    self.set_progress_none(&path);
                    self.set_state(&path, ItemState::Failed(err));
                }
                WorkerEvent::Finished(duration) => {
                    self.finished = Some(duration);
                    clear_receiver = true;
                }
            }
        }
        if clear_receiver {
            self.receiver = None;
        }
    }

    fn set_state(&mut self, path: &Path, state: ItemState) {
        if let Some(item) = self.items.iter_mut().find(|item| item.path == path) {
            item.state = state;
        }
    }

    fn set_progress(&mut self, path: &Path, progress: ProgressSnapshot) {
        if let Some(item) = self.items.iter_mut().find(|item| item.path == path) {
            item.progress = Some(progress);
        }
    }

    fn set_progress_none(&mut self, path: &Path) {
        if let Some(item) = self.items.iter_mut().find(|item| item.path == path) {
            item.progress = None;
        }
    }

    fn status_counts(&self) -> StatusCounts {
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
}

#[derive(Default)]
struct StatusCounts {
    pending: usize,
    running: usize,
    done: usize,
    failed: usize,
}

impl eframe::App for ZapApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        if self.is_running() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().inner_margin(egui::Margin::same(14)))
            .show(ctx, |ui| {
                if self.items.is_empty() {
                    render_empty_state(ui, ctx);
                    return;
                }

                let counts = self.status_counts();
                if should_auto_close(self, &counts) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }

                render_prompt(ui, self, &counts);
                ui.add_space(8.0);
                render_options(ui, self);
                ui.add_space(8.0);
                render_actions(ui, ctx, self);
            });
    }
}

fn run_delete_path(
    path: &Path,
    threads: Option<usize>,
    dry_run: bool,
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

    let result = delete::delete_path(
        path,
        DeleteOptions::default()
            .with_threads(threads)
            .with_bar(bar)
            .allow_dangerous(),
    );

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
        },
    ));
}

fn normalize_worker_result(result: io::Result<()>, dry_run: bool) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(err) if !dry_run && err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

fn should_auto_close(app: &ZapApp, counts: &StatusCounts) -> bool {
    app.finished.is_some() && !app.dry_run && counts.failed == 0 && counts.done > 0
}

fn render_empty_state(ui: &mut egui::Ui, ctx: &egui::Context) {
    ui.heading("No paths were provided");
    ui.label("Open Zap from the Windows Explorer context menu.");
    ui.add_space(12.0);
    if ui.button("Close").clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

fn render_prompt(ui: &mut egui::Ui, app: &ZapApp, counts: &StatusCounts) {
    let total = app.items.len();
    let progress = aggregate_progress(app, counts);

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("!").size(28.0).strong());
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.heading(
                egui::RichText::new(selection_title(total, app.dry_run))
                    .size(16.0)
                    .strong(),
            );
            ui.label(egui::RichText::new(selection_detail(&app.items)).size(12.0));
        });
    });

    if app.started_at.is_some() || app.finished.is_some() {
        ui.add_space(6.0);
        ui.label(egui::RichText::new(status_text(counts, app)).size(12.0));
        ui.add_space(3.0);
        let progress_bar = egui::ProgressBar::new(progress)
            .animate(app.is_running())
            .desired_height(10.0);
        ui.add(progress_bar);
    } else {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Deletion is permanent. Drive roots are refused.").size(12.0));
    }
}

fn render_options(ui: &mut egui::Ui, app: &mut ZapApp) {
    ui.add_enabled_ui(!app.is_running(), |ui| {
        ui.checkbox(&mut app.dry_run, "Preview only");
        ui.horizontal(|ui| {
            ui.checkbox(&mut app.threads_enabled, "Limit threads");
            ui.add_enabled(
                app.threads_enabled,
                egui::TextEdit::singleline(&mut app.threads_text).desired_width(46.0),
            );
        });
        if let Some(error) = app.thread_error() {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    });
}

fn render_actions(ui: &mut egui::Ui, ctx: &egui::Context, app: &mut ZapApp) {
    let running = app.is_running();
    let finished = app.finished.is_some();
    let can_start = !running && app.thread_error().is_none();

    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let label = if app.dry_run {
            "Preview"
        } else if finished {
            "Run again"
        } else {
            "Delete"
        };
        let fill = if app.dry_run {
            egui::Color32::from_rgb(50, 115, 200)
        } else {
            egui::Color32::from_rgb(190, 64, 64)
        };

        let start_btn = egui::Button::new(egui::RichText::new(label).size(14.0).strong())
            .fill(fill)
            .min_size(egui::vec2(104.0, 32.0));

        if ui.add_enabled(can_start, start_btn).clicked() {
            app.start();
        }

        ui.add_space(8.0);

        let cancel_btn = egui::Button::new(
            egui::RichText::new(if finished { "Close" } else { "Cancel" }).size(14.0),
        )
        .min_size(egui::vec2(88.0, 32.0));

        if ui.add_enabled(!running, cancel_btn).clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    });
}

fn selection_title(total: usize, dry_run: bool) -> String {
    let action = if dry_run { "Preview" } else { "Delete" };
    match total {
        1 => format!("{action} this item?"),
        _ => format!("{action} these {total} items?"),
    }
}

fn selection_detail(items: &[DeleteItem]) -> String {
    match items {
        [] => String::new(),
        [item] => compact_path(&item.path),
        [first, rest @ ..] => format!("{} and {} more", compact_path(&first.path), rest.len()),
    }
}

fn compact_path(path: &Path) -> String {
    let text = path.display().to_string();
    const MAX_LEN: usize = 52;
    if text.chars().count() <= MAX_LEN {
        return text;
    }

    let suffix: String = text
        .chars()
        .rev()
        .take(MAX_LEN - 3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{suffix}")
}

fn status_text(counts: &StatusCounts, app: &ZapApp) -> String {
    let finished = counts.done + counts.failed;
    let total = app.items.len();
    let elapsed = app
        .finished
        .or_else(|| app.started_at.map(|started| started.elapsed()));

    let mut text = format!("{finished}/{total} complete");
    if counts.failed > 0 {
        text.push_str(&format!(", {} failed", counts.failed));
        if let Some(error) = first_failure(&app.items) {
            text.push_str(": ");
            text.push_str(&compact_text(error, 64));
        }
    }
    if let Some(duration) = elapsed {
        text.push_str(&format!(" - {:.1}s", duration.as_secs_f32()));
    }
    if let Some(message) = active_progress_message(&app.items) {
        text.push_str(" - ");
        text.push_str(message);
    }
    text
}

fn aggregate_progress(app: &ZapApp, counts: &StatusCounts) -> f32 {
    let total = app.items.len();
    if total == 0 {
        return 0.0;
    }

    let mut completed = (counts.done + counts.failed) as f32;
    for item in &app.items {
        if matches!(item.state, ItemState::Running) {
            completed += item_progress_fraction(item);
        }
    }
    (completed / total as f32).clamp(0.0, 1.0)
}

fn item_progress_fraction(item: &DeleteItem) -> f32 {
    let Some(progress) = &item.progress else {
        return 0.0;
    };
    let Some(length) = progress.length else {
        return 0.0;
    };
    if length == 0 {
        return 0.0;
    }
    (progress.position as f32 / length as f32).clamp(0.0, 1.0)
}

fn active_progress_message(items: &[DeleteItem]) -> Option<&str> {
    items.iter().find_map(|item| {
        if !matches!(item.state, ItemState::Running) {
            return None;
        }
        let message = item.progress.as_ref()?.message.trim();
        (!message.is_empty()).then_some(message)
    })
}

fn first_failure(items: &[DeleteItem]) -> Option<&str> {
    items.iter().find_map(|item| match &item.state {
        ItemState::Failed(error) => Some(error.as_str()),
        _ => None,
    })
}

fn compact_text(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_owned();
    }

    let prefix: String = text.chars().take(max_len.saturating_sub(3)).collect();
    format!("{prefix}...")
}

enum WorkerEvent {
    Started(PathBuf),
    Progress(PathBuf, ProgressSnapshot),
    Done(PathBuf, Result<(), String>),
    Finished(Duration),
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut app = ZapApp::new(vec![PathBuf::from("deleted")], None);
        app.finished = Some(Duration::from_millis(1));
        app.items[0].state = ItemState::Done;

        assert!(should_auto_close(&app, &app.status_counts()));
    }

    #[test]
    fn destructive_failure_keeps_dialog_open() {
        let mut app = ZapApp::new(vec![PathBuf::from("locked")], None);
        app.finished = Some(Duration::from_millis(1));
        app.items[0].state = ItemState::Failed("locked".to_owned());

        assert!(!should_auto_close(&app, &app.status_counts()));
    }

    #[test]
    fn dry_run_success_keeps_dialog_open() {
        let mut app = ZapApp::new(vec![PathBuf::from("previewed")], None);
        app.dry_run = true;
        app.finished = Some(Duration::from_millis(1));
        app.items[0].state = ItemState::Done;

        assert!(!should_auto_close(&app, &app.status_counts()));
    }
}
