#![cfg_attr(windows, windows_subsystem = "windows")]

use eframe::egui;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zap::delete::{self, DeleteOptions};

const APP_NAME: &str = "Zap";
const BATCH_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const BATCH_QUIET_POLLS: usize = 10;
const BATCH_MAX_POLLS: usize = 120;
const MAX_THREADS: usize = 1024;
const WINDOW_SIZE: [f32; 2] = [420.0, 225.0];

fn main() -> eframe::Result<()> {
    let args = parse_args();
    let (paths, batch_session) = if args.batch {
        match collect_batch_paths(&args.paths) {
            Some(batch) => batch,
            None => return Ok(()),
        }
    } else {
        (args.paths, None)
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_inner_size(WINDOW_SIZE)
        .with_min_inner_size(WINDOW_SIZE)
        .with_max_inner_size(WINDOW_SIZE)
        .with_resizable(false)
        .with_maximize_button(false);

    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }

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
            configure_style(&cc.egui_ctx);
            Ok(Box::new(ZapApp::new(paths, args.threads, batch_session)))
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

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    let widget_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.inactive.corner_radius = widget_radius;
    style.visuals.widgets.hovered.corner_radius = widget_radius;
    style.visuals.widgets.active.corner_radius = widget_radius;
    style.visuals.widgets.open.corner_radius = widget_radius;
    style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(4);

    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);

    ctx.set_style(style);
}

fn apply_dark_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    if !style.visuals.dark_mode {
        return;
    }

    let panel_bg = egui::Color32::from_rgb(30, 32, 38);
    if style.visuals.panel_fill == panel_bg {
        return;
    }

    let widget_bg = egui::Color32::from_rgb(42, 45, 52);
    let widget_bg_hover = egui::Color32::from_rgb(52, 56, 64);
    let widget_bg_active = egui::Color32::from_rgb(58, 62, 72);
    let text_color = egui::Color32::from_rgb(220, 222, 228);
    let weak_text = egui::Color32::from_rgb(150, 154, 164);
    let stroke_color = egui::Color32::from_rgb(70, 74, 84);

    style.visuals.panel_fill = panel_bg;
    style.visuals.window_fill = panel_bg;
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(24, 26, 30);
    style.visuals.faint_bg_color = egui::Color32::from_rgb(36, 38, 44);

    style.visuals.widgets.inactive.bg_fill = widget_bg;
    style.visuals.widgets.inactive.weak_bg_fill = widget_bg;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, stroke_color);

    style.visuals.widgets.hovered.bg_fill = widget_bg_hover;
    style.visuals.widgets.hovered.weak_bg_fill = widget_bg_hover;
    style.visuals.widgets.hovered.bg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 95, 108));

    style.visuals.widgets.active.bg_fill = widget_bg_active;
    style.visuals.widgets.active.weak_bg_fill = widget_bg_active;

    style.visuals.widgets.noninteractive.bg_fill = panel_bg;
    style.visuals.widgets.noninteractive.weak_bg_fill = panel_bg;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, stroke_color);
    style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, text_color);

    style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, weak_text);
    style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, text_color);
    style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, text_color);

    style.visuals.selection.bg_fill = egui::Color32::from_rgb(50, 100, 180);
    style.visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(140, 180, 240));

    ctx.set_style(style);
}

fn load_app_icon() -> Option<egui::IconData> {
    let png_bytes = include_bytes!("../../assets/zap.png");
    let img = image::load_from_memory(png_bytes).ok()?.into_rgba8();
    let (w, h) = (img.width(), img.height());
    Some(egui::IconData {
        rgba: img.into_raw(),
        width: w,
        height: h,
    })
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

struct BatchSession {
    paths_dir: PathBuf,
    lock_file: PathBuf,
    lock: Option<File>,
    last_path_count: usize,
}

impl Drop for BatchSession {
    fn drop(&mut self) {
        drop(self.lock.take());
        let _ = fs::remove_dir_all(&self.paths_dir);
        let _ = fs::remove_file(&self.lock_file);
    }
}

impl BatchSession {
    fn touch_lock(&mut self) {
        if let Some(lock) = &mut self.lock {
            let _ = lock.write_all(b".");
            let _ = lock.flush();
        }
    }
}

fn collect_batch_paths(paths: &[PathBuf]) -> Option<(Vec<PathBuf>, Option<BatchSession>)> {
    let temp = std::env::temp_dir();
    let paths_dir = temp.join("zapg-batch-paths.d");
    let lock_file = temp.join("zapg-batch.lock");

    cleanup_stale_batch(&paths_dir, &lock_file);
    if write_batch_paths(&paths_dir, paths).is_err() {
        return Some((paths.to_vec(), None));
    }

    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_file)
    {
        Ok(lock) => {
            // Brief pause to let the first wave of Explorer-launched
            // processes write their paths. The GUI will keep polling
            // for late arrivals via poll_batch_session().
            thread::sleep(Duration::from_millis(150));
            let mut collected = read_batch_paths(&paths_dir);
            dedup_paths(&mut collected);
            let last_path_count = collected.len();
            Some((
                collected,
                Some(BatchSession {
                    paths_dir,
                    lock_file,
                    lock: Some(lock),
                    last_path_count,
                }),
            ))
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
    let pid = std::process::id();

    for attempt in 0..100_u32 {
        let file = paths_dir.join(format!("{nanos}-{pid}-{attempt}.txt"));
        match OpenOptions::new().create_new(true).write(true).open(file) {
            Ok(mut out) => {
                for path in paths {
                    writeln!(out, "{}", path.display())?;
                }
                out.flush()?;
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique batch path file",
    ))
}

fn wait_for_batch_quiet(paths_dir: &Path) {
    let mut last_state = batch_state(paths_dir);
    let mut quiet = 0;

    for _ in 0..BATCH_MAX_POLLS {
        thread::sleep(BATCH_POLL_INTERVAL);
        let state = batch_state(paths_dir);
        if state == last_state {
            quiet += 1;
            if quiet >= BATCH_QUIET_POLLS {
                break;
            }
        } else {
            quiet = 0;
            last_state = state;
        }
    }
}

fn batch_state(paths_dir: &Path) -> (usize, u64) {
    fs::read_dir(paths_dir)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then_some(meta.len())
        })
        .fold((0, 0), |(count, bytes), len| (count + 1, bytes + len))
}

fn read_batch_paths(paths_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(paths_dir)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| entry.path())
        })
        .collect();
    files.sort();

    files
        .into_iter()
        .flat_map(|file| {
            fs::read_to_string(file)
                .unwrap_or_default()
                .lines()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
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
    total_size: Arc<Mutex<Option<u64>>>,
    batch_session: Option<BatchSession>,
}

impl ZapApp {
    fn new(
        paths: Vec<PathBuf>,
        threads: Option<usize>,
        batch_session: Option<BatchSession>,
    ) -> Self {
        let total_size = Arc::new(Mutex::new(None));

        let size_handle = Arc::clone(&total_size);
        let size_paths: Vec<PathBuf> = paths.clone();
        thread::spawn(move || {
            let mut sum: u64 = 0;
            for path in &size_paths {
                sum += dir_size_recursive(path);
            }
            if let Ok(mut guard) = size_handle.lock() {
                *guard = Some(sum);
            }
        });

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
            total_size,
            batch_session,
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
        self.finalize_batch_session();

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

    fn poll_batch_session(&mut self) {
        let paths_dir = {
            let Some(session) = &mut self.batch_session else {
                return;
            };
            session.touch_lock();
            session.paths_dir.clone()
        };

        if self.is_running() || self.finished.is_some() {
            return;
        }

        let changed = self.add_batch_paths_from(&paths_dir);
        if let Some(session) = &mut self.batch_session {
            session.last_path_count = self.items.len();
        }
        if changed {
            self.recalculate_total_size();
        }
    }

    fn finalize_batch_session(&mut self) {
        let Some(session) = &mut self.batch_session else {
            return;
        };

        session.touch_lock();
        let paths_dir = session.paths_dir.clone();
        wait_for_batch_quiet(&paths_dir);
        let changed = self.add_batch_paths_from(&paths_dir);
        if let Some(session) = &mut self.batch_session {
            session.last_path_count = self.items.len();
        }
        if changed {
            self.recalculate_total_size();
        }
    }

    fn add_batch_paths_from(&mut self, paths_dir: &Path) -> bool {
        let mut paths = read_batch_paths(paths_dir);
        dedup_paths(&mut paths);

        let mut changed = false;
        for path in paths {
            if !self.items.iter().any(|item| item.path == path) {
                self.items.push(DeleteItem {
                    path,
                    state: ItemState::Pending,
                    progress: None,
                });
                changed = true;
            }
        }
        changed
    }

    fn recalculate_total_size(&mut self) {
        if let Ok(mut guard) = self.total_size.lock() {
            *guard = None;
        }
        let size_handle = Arc::clone(&self.total_size);
        let size_paths: Vec<PathBuf> = self.items.iter().map(|item| item.path.clone()).collect();
        thread::spawn(move || {
            let mut sum: u64 = 0;
            for path in &size_paths {
                sum += dir_size_recursive(path);
            }
            if let Ok(mut guard) = size_handle.lock() {
                *guard = Some(sum);
            }
        });
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
        apply_dark_theme(ctx);
        self.poll_events();
        self.poll_batch_session();

        let size_ready = self.total_size.lock().ok().and_then(|g| *g).is_none();
        if self.is_running() || size_ready || self.batch_session.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let running = self.is_running();
        let finished = self.finished.is_some();
        let can_start = !running && self.thread_error().is_none();

        let esc_pressed =
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if esc_pressed && !running {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let enter_pressed =
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        if enter_pressed && can_start && !self.items.is_empty() {
            self.start();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().inner_margin(egui::Margin::same(16)))
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
                ui.add_space(10.0);
                render_options(ui, self);
                ui.add_space(10.0);
                render_actions(ui, ctx, self, running, finished, can_start);
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
        let warn_color = egui::Color32::from_rgb(230, 170, 40);
        ui.label(egui::RichText::new("\u{26A0}").size(26.0).color(warn_color));
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.heading(
                egui::RichText::new(selection_title(total, app.dry_run))
                    .size(16.0)
                    .strong(),
            );
            let mut detail = selection_detail(&app.items);
            if let Some(size) = app.total_size.lock().ok().and_then(|g| *g) {
                detail.push_str(&format!("  ({})", format_size(size)));
            }
            ui.label(egui::RichText::new(detail).size(12.0));
        });
    });

    if app.started_at.is_some() || app.finished.is_some() {
        ui.add_space(6.0);
        ui.label(egui::RichText::new(status_text(counts, app)).size(12.0));
        ui.add_space(4.0);
        let progress_bar = egui::ProgressBar::new(progress)
            .animate(app.is_running())
            .desired_height(10.0);
        ui.add(progress_bar);
    } else {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Deletion is permanent. Drive roots are refused.")
                .size(12.0)
                .weak(),
        );
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

fn render_actions(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    app: &mut ZapApp,
    running: bool,
    finished: bool,
    can_start: bool,
) {
    let is_dark = ui.visuals().dark_mode;

    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let label = if app.dry_run {
            "Preview"
        } else if finished {
            "Run again"
        } else {
            "Delete"
        };
        let fill = if app.dry_run {
            if is_dark {
                egui::Color32::from_rgb(56, 120, 210)
            } else {
                egui::Color32::from_rgb(45, 110, 205)
            }
        } else if is_dark {
            egui::Color32::from_rgb(200, 60, 60)
        } else {
            egui::Color32::from_rgb(210, 50, 50)
        };

        let btn_radius = egui::CornerRadius::same(8);
        let start_btn = egui::Button::new(
            egui::RichText::new(label)
                .size(14.0)
                .strong()
                .color(egui::Color32::WHITE),
        )
        .fill(fill)
        .corner_radius(btn_radius)
        .min_size(egui::vec2(108.0, 34.0));

        if ui.add_enabled(can_start, start_btn).clicked() {
            app.start();
        }

        ui.add_space(8.0);

        let cancel_fill = if is_dark {
            egui::Color32::from_rgb(60, 63, 68)
        } else {
            egui::Color32::from_rgb(225, 228, 232)
        };
        let cancel_text_color = if is_dark {
            egui::Color32::from_rgb(210, 212, 216)
        } else {
            egui::Color32::from_rgb(50, 54, 60)
        };
        let cancel_btn = egui::Button::new(
            egui::RichText::new(if finished { "Close" } else { "Cancel" })
                .size(14.0)
                .color(cancel_text_color),
        )
        .fill(cancel_fill)
        .corner_radius(btn_radius)
        .min_size(egui::vec2(92.0, 34.0));

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

fn dir_size_recursive(path: &Path) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_file() {
        return meta.len();
    }
    if meta.file_type().is_symlink() {
        return 0;
    }
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            total += dir_size_recursive(&entry.path());
        }
    }
    total
}

fn format_size(bytes: u64) -> String {
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
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&lock_file)
            .unwrap();

        let existing = PathBuf::from("existing");
        let late = PathBuf::from("late");
        fs::write(
            paths_dir.join("b-existing.txt"),
            existing.display().to_string(),
        )
        .unwrap();
        fs::write(paths_dir.join("a-late.txt"), late.display().to_string()).unwrap();

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
}
