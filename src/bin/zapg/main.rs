//! `zapg` — GUI confirmation dialog binary.
//!
//! Split into focused modules:
//! * [`app`] — application state, worker thread, batch-session logic
//! * [`ui`]  — egui theme and rendering

#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod taskbar;
mod ui;

use std::fs::File;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use eframe::egui;
use zap::{batch, path_utils};

use app::{BatchReceiver, BatchSession, ZapApp};

pub const APP_NAME: &str = "Zap";
pub const MAX_THREADS: usize = 1024;
pub const WINDOW_WIDTH: f32 = 420.0;
pub const WINDOW_HEIGHT_NORMAL: f32 = 260.0;
pub const WINDOW_HEIGHT_DANGEROUS: f32 = 304.0;
pub const WINDOW_HEIGHT_TREEMAP: f32 = 520.0;

fn main() -> eframe::Result<()> {
    let args = parse_args();

    // In batch mode: non-coordinators must exit silently without opening
    // a window. Only the process that wins the lock shows the GUI. The
    // coordinator writes its own paths immediately, then collects the
    // rest from siblings asynchronously so the window appears instantly.
    let (paths, batch_session, batch_rx) = if args.batch {
        match try_become_coordinator(&args.paths) {
            CoordinatorOutcome::NotCoordinator => return Ok(()),
            CoordinatorOutcome::Coordinator {
                paths,
                batch_session,
                batch_rx,
            } => (paths, batch_session, Some(batch_rx)),
        }
    } else {
        (args.paths, None, None)
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT_NORMAL])
        .with_min_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT_NORMAL])
        .with_max_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT_TREEMAP])
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
            ui::configure_style(&cc.egui_ctx);
            let mut app = if let Some(rx) = batch_rx {
                ZapApp::new_collecting(paths, args.threads, batch_session, rx)
            } else {
                ZapApp::new(paths, args.threads, None)
            };
            // --shred and --recycle are mutually exclusive in the app UI;
            // shred wins if a broken registration passes both.
            app.recycle = args.recycle && !args.shred;
            app.shred = args.shred;
            app.no_journal = args.no_journal;
            Ok(Box::new(app))
        }),
    )
}

enum CoordinatorOutcome {
    NotCoordinator,
    Coordinator {
        paths: Vec<PathBuf>,
        batch_session: Option<BatchSession>,
        batch_rx: BatchReceiver,
    },
}

/// Fast path for non-coordinators: write own paths, try lock. If we lose
/// the lock race, exit immediately without opening any window. The winner
/// spawns a background thread to collect the rest.
fn try_become_coordinator(own_paths: &[PathBuf]) -> CoordinatorOutcome {
    let paths_dir = batch::batch_paths_dir();
    let lock_file = batch::batch_lock_file();

    batch::cleanup_stale_batch(&paths_dir, &lock_file);

    // Write own paths now — siblings need them even if we win the lock.
    if batch::write_batch_paths(&paths_dir, own_paths).is_err() {
        return CoordinatorOutcome::Coordinator {
            paths: own_paths.to_vec(),
            batch_session: None,
            batch_rx: {
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(Some((own_paths.to_vec(), None)));
                rx
            },
        };
    }

    // Fast lock race — this is the only sync delay (~0ms).
    let mut lock: File = match batch::try_acquire_lock(&lock_file) {
        Ok(l) => l,
        Err(_) => return CoordinatorOutcome::NotCoordinator,
    };

    // We won the lock — spawn the slow batch collection in background
    // so the window opens instantly.
    let (tx, rx) = mpsc::channel();
    let paths_dir_clone = paths_dir.clone();
    let lock_file_clone = lock_file.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        batch::touch_lock(&mut lock);
        batch::wait_for_batch_quiet(
            &paths_dir_clone,
            batch::BATCH_QUIET_POLLS,
            batch::BATCH_MAX_POLLS,
        );
        let mut collected = batch::read_batch_paths(&paths_dir_clone);
        path_utils::dedup_paths(&mut collected);
        let session = BatchSession {
            paths_dir: paths_dir_clone,
            lock_file: lock_file_clone,
            lock: Some(lock),
            last_path_count: collected.len(),
        };
        let _ = tx.send(Some((collected, Some(session))));
    });

    CoordinatorOutcome::Coordinator {
        paths: own_paths.to_vec(),
        batch_session: None,
        batch_rx: rx,
    }
}

#[derive(Default)]
struct GuiArgs {
    batch: bool,
    recycle: bool,
    /// Open the dialog with Shred pre-selected (context-menu "Shred" verb).
    shred: bool,
    no_journal: bool,
    threads: Option<usize>,
    paths: Vec<PathBuf>,
}

fn parse_args() -> GuiArgs {
    let mut parsed = GuiArgs::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--batch") => parsed.batch = true,
            Some("--recycle") => parsed.recycle = true,
            Some("--shred") => parsed.shred = true,
            Some("--no-journal") => parsed.no_journal = true,
            Some("--threads") | Some("-j") => {
                if let Some(value) = args.next() {
                    parsed.threads = value
                        .to_str()
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|v| (1..=MAX_THREADS).contains(v));
                }
            }
            _ => {
                // Never treat an unrecognized flag as a path to delete —
                // a typo in the context-menu registration must not turn
                // into a deletion target shown in the dialog.
                if arg
                    .to_str()
                    .is_some_and(|s| s.len() > 1 && s.starts_with('-'))
                {
                    continue;
                }
                parsed.paths.push(PathBuf::from(arg));
            }
        }
    }
    path_utils::dedup_paths(&mut parsed.paths);
    parsed
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
    let x = work.left + ((work.right - work.left) - WINDOW_WIDTH as i32) / 2;
    let y = work.top + ((work.bottom - work.top) - WINDOW_HEIGHT_NORMAL as i32) / 2;
    Some(egui::pos2(x.max(work.left) as f32, y.max(work.top) as f32))
}

#[cfg(not(windows))]
fn initial_window_position() -> Option<egui::Pos2> {
    None
}

fn load_app_icon() -> Option<egui::IconData> {
    let png_bytes = include_bytes!("../../../assets/branding/zap.png");
    let img = image::load_from_memory(png_bytes).ok()?.into_rgba8();
    let (w, h) = (img.width(), img.height());
    Some(egui::IconData {
        rgba: img.into_raw(),
        width: w,
        height: h,
    })
}
