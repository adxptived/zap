//! egui rendering for the zapg window: the frame loop and shared helpers
//! live here; `theme`, `home`, `dialog` and `settings` draw the screens.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, RichText, Stroke};
use zap::settings::{DefaultMode, Language, MenuLayout};
use zap::size;

use crate::app::{should_auto_close, DeleteItem, ItemState, Screen, StatusCounts, ZapApp};
use crate::i18n::{files_word, folders_word, items_word, links_word, localize_error, pick};
use crate::{WINDOW_HEIGHT_MAX, WINDOW_HEIGHT_MIN, WINDOW_WIDTH};

const MARGIN: i8 = 18;
const ITEM_LIST_MAX_HEIGHT: f32 = 148.0;
const FAILURE_LIST_MAX_HEIGHT: f32 = 120.0;
const SETTINGS_BODY_MAX_HEIGHT: f32 = 560.0;
/// How long a successful run stays visible before the dialog closes.
const SUCCESS_LINGER: Duration = Duration::from_millis(900);
const CANCELLED: &str = "cancelled by user";

mod dialog;
mod home;
mod settings;
mod theme;

use dialog::*;
use home::*;
use settings::*;
pub use theme::configure_style;
use theme::*;

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

impl eframe::App for ZapApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        self.poll_batch_collection();
        self.poll_batch_session();

        // Build status once per frame. The taskbar and the central panel share
        // it instead of each scanning a potentially huge selection.
        let counts = self.status_counts();
        let progress = run_progress(self, &counts);
        #[cfg(windows)]
        self.update_taskbar(progress.fraction.unwrap_or(0.0), &counts);

        // Drag & drop: add files/folders dropped onto the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.screen = Screen::Main;
            self.add_dropped_paths(dropped);
        }
        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering_files && !self.is_running() {
            paint_drop_overlay(ctx, self.lang);
        }

        if self.total_size_calculating {
            if let Ok(guard) = self.total_size.lock() {
                if guard.is_some() {
                    self.total_size_calculating = false;
                }
            }
        }

        let should_repaint = self.is_running()
            || self.total_size_calculating
            || self.batch_session.is_some()
            || self.batch_collecting.is_some();
        if should_repaint {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // A clean destructive run shows its result briefly, then closes.
        if should_auto_close(self, &counts) {
            let shown = self.finished_at.map(|at| at.elapsed()).unwrap_or_default();
            if shown >= SUCCESS_LINGER {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(SUCCESS_LINGER - shown);
            }
        }

        let running = self.is_running();
        // Wait for the Explorer batch to finish arriving: starting earlier
        // would run on a partial selection the user has not seen.
        let can_start = !running
            && self.batch_collecting.is_none()
            && !self.items.is_empty()
            && self.thread_error().is_none()
            && (!self.has_dangerous_paths || self.danger_confirmed);

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) && !running {
            if self.screen == Screen::Settings {
                self.screen = Screen::Main;
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if self.screen == Screen::Main
            && can_start
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
        {
            self.start();
        }

        let panel = egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(MARGIN)),
            )
            .show(ctx, |ui| {
                // Measure inside a scope: the panel's own `min_rect` always
                // spans the whole window, so it could only ever grow — after
                // the tall settings screen the dialog never shrank back.
                ui.scope(|ui| match self.screen {
                    Screen::Settings => render_settings(ui, self),
                    Screen::Main if self.items.is_empty() => render_home(ui, ctx, self),
                    Screen::Main => render_dialog(ui, ctx, self, &counts, &progress, can_start),
                })
                .response
                .rect
                .height()
            });

        fit_window_to_content(ctx, panel.inner);
    }
}

/// Size the (non-resizable) window to its content — growing *and*
/// shrinking — so no state needs a hand-tuned height constant.
fn fit_window_to_content(ctx: &egui::Context, content_height: f32) {
    let target =
        (content_height + 2.0 * f32::from(MARGIN)).clamp(WINDOW_HEIGHT_MIN, WINDOW_HEIGHT_MAX);
    let current = ctx.input(|i| i.screen_rect().height());
    if (current - target).abs() > 1.0 {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            WINDOW_WIDTH,
            target,
        )));
    }
}

// Note: right-to-left button rows are always nested in `ui.horizontal`.
// At panel level a right-to-left layout claims all remaining height and
// defeats `fit_window_to_content`.

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn card(fill: Color32, stroke: Color32) -> egui::Frame {
    egui::Frame::default()
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(10, 8))
}

fn cancelled_count(items: &[DeleteItem]) -> usize {
    items
        .iter()
        .filter(|item| matches!(&item.state, ItemState::Failed(e) if e.contains(CANCELLED)))
        .count()
}

/// Remaining-time estimate; None until progress is stable enough.
fn estimate_eta_secs(fraction: Option<f32>, elapsed: f32) -> Option<f32> {
    let fraction = fraction?;
    if !(0.02..1.0).contains(&fraction) || elapsed < 0.5 {
        return None;
    }
    Some(elapsed * (1.0 - fraction) / fraction)
}

fn format_duration(secs: f32, lang: Language) -> String {
    let (s, m, h) = if lang == Language::Russian {
        ("с", "м", "ч")
    } else {
        ("s", "m", "h")
    };
    if secs < 10.0 {
        format!("{secs:.1}{s}")
    } else if secs < 60.0 {
        format!("{}{s}", secs.round() as u64)
    } else if secs < 3600.0 {
        format!("{}{m} {:02}{s}", (secs / 60.0) as u64, (secs % 60.0) as u64)
    } else {
        format!(
            "{}{h} {:02}{m}",
            (secs / 3600.0) as u64,
            ((secs % 3600.0) / 60.0) as u64
        )
    }
}

/// `12345` → `12,345`.
fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn compact_text(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_owned();
    }
    let prefix: String = text.chars().take(max_len.saturating_sub(1)).collect();
    format!("{prefix}…")
}

/// Dim the window and show a hint while files are dragged over it.
fn paint_drop_overlay(ctx: &egui::Context, lang: Language) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("drop_overlay"),
    ));
    let rect = ctx.screen_rect();
    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(150));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        pick(lang, "Drop to add to the list", "Отпустите, чтобы добавить"),
        egui::FontId::proportional(16.0),
        Color32::WHITE,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_count_groups_thousands() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(1_234_567), "1,234,567");
    }

    #[test]
    fn format_duration_picks_readable_units() {
        let en = Language::English;
        assert_eq!(format_duration(1.25, en), "1.2s");
        assert_eq!(format_duration(42.4, en), "42s");
        assert_eq!(format_duration(125.0, en), "2m 05s");
        assert_eq!(format_duration(3_720.0, en), "1h 02m");
        assert_eq!(format_duration(125.0, Language::Russian), "2м 05с");
    }

    #[test]
    fn eta_needs_stable_progress() {
        assert_eq!(estimate_eta_secs(None, 5.0), None);
        assert_eq!(estimate_eta_secs(Some(0.01), 5.0), None);
        assert_eq!(estimate_eta_secs(Some(0.5), 0.2), None);
        assert_eq!(estimate_eta_secs(Some(0.5), 4.0), Some(4.0));
    }

    #[test]
    fn subtitle_uses_shared_parent() {
        let items: Vec<DeleteItem> = [r"C:\work\a", r"C:\work\b"]
            .iter()
            .map(|p| DeleteItem {
                path: PathBuf::from(p),
                state: ItemState::Pending,
                progress: None,
            })
            .collect();
        assert_eq!(
            subtitle(&items, Language::English).as_deref(),
            Some(r"in C:\work")
        );
        assert_eq!(
            subtitle(&items, Language::Russian).as_deref(),
            Some(r"в C:\work")
        );
    }
}
