//! egui theme and rendering for the zapg dialog.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, RichText, Stroke};
use zap::{size, treemap};

use crate::app::{should_auto_close, DeleteItem, ItemState, StatusCounts, ZapApp};
use crate::{WINDOW_HEIGHT_MAX, WINDOW_HEIGHT_MIN, WINDOW_WIDTH};

const MARGIN: i8 = 18;
const ITEM_LIST_MAX_HEIGHT: f32 = 148.0;
const FAILURE_LIST_MAX_HEIGHT: f32 = 120.0;
const TREEMAP_HEIGHT: f32 = 260.0;
/// How long a successful run stays visible before the dialog closes.
const SUCCESS_LINGER: Duration = Duration::from_millis(900);
const CANCELLED: &str = "cancelled by user";

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// Semantic colours, resolved per frame for the active (light/dark) theme.
struct Palette {
    danger: Color32,
    danger_soft: Color32,
    warn: Color32,
    success: Color32,
    accent: Color32,
    card: Color32,
    card_stroke: Color32,
    muted: Color32,
}

impl Palette {
    fn for_ui(ui: &egui::Ui) -> Self {
        if ui.visuals().dark_mode {
            Self {
                danger: Color32::from_rgb(232, 84, 84),
                danger_soft: Color32::from_rgb(64, 34, 38),
                warn: Color32::from_rgb(236, 176, 64),
                success: Color32::from_rgb(88, 192, 116),
                accent: Color32::from_rgb(92, 150, 240),
                card: Color32::from_rgb(38, 41, 48),
                card_stroke: Color32::from_rgb(56, 60, 70),
                muted: Color32::from_rgb(150, 155, 166),
            }
        } else {
            Self {
                danger: Color32::from_rgb(200, 48, 48),
                danger_soft: Color32::from_rgb(253, 236, 236),
                warn: Color32::from_rgb(190, 120, 10),
                success: Color32::from_rgb(34, 140, 70),
                accent: Color32::from_rgb(40, 104, 210),
                card: Color32::from_rgb(255, 255, 255),
                card_stroke: Color32::from_rgb(222, 225, 231),
                muted: Color32::from_rgb(105, 110, 122),
            }
        }
    }
}

pub fn configure_style(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.all_styles_mut(|style| {
        use egui::{FontFamily, FontId, TextStyle};
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(18.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(13.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.button_padding = egui::vec2(12.0, 5.0);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.interact_size.y = 24.0;
    });
    ctx.set_visuals_of(egui::Theme::Dark, tune_visuals(egui::Visuals::dark()));
    ctx.set_visuals_of(egui::Theme::Light, tune_visuals(egui::Visuals::light()));
}

fn tune_visuals(mut v: egui::Visuals) -> egui::Visuals {
    let radius = CornerRadius::same(6);
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = radius;
    }
    v.widgets.noninteractive.corner_radius = CornerRadius::same(4);
    if v.dark_mode {
        let panel = Color32::from_rgb(28, 30, 36);
        let widget = Color32::from_rgb(44, 47, 55);
        let hover = Color32::from_rgb(54, 58, 67);
        v.panel_fill = panel;
        v.window_fill = panel;
        v.extreme_bg_color = Color32::from_rgb(22, 24, 28);
        v.faint_bg_color = Color32::from_rgb(34, 37, 43);
        v.widgets.inactive.bg_fill = widget;
        v.widgets.inactive.weak_bg_fill = widget;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(62, 66, 76));
        v.widgets.hovered.bg_fill = hover;
        v.widgets.hovered.weak_bg_fill = hover;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(56, 60, 70));
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(222, 224, 230));
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(210, 213, 220));
        v.selection.bg_fill = Color32::from_rgb(52, 98, 176);
    } else {
        let panel = Color32::from_rgb(246, 247, 249);
        v.panel_fill = panel;
        v.window_fill = panel;
        v.faint_bg_color = Color32::from_rgb(238, 240, 244);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(232, 235, 240);
        v.widgets.inactive.bg_fill = Color32::from_rgb(232, 235, 240);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(222, 226, 233);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(32, 35, 42));
        v.selection.bg_fill = Color32::from_rgb(64, 120, 220);
    }
    v
}

/// Use Segoe UI (the Windows UI font) when available: crisper than egui's
/// bundled font at small sizes and covers Cyrillic/Greek/Latin paths. The
/// bundled fonts stay as fallbacks for symbols and emoji.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let font_dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    let mut primary = Vec::new();
    for (name, file) in [
        ("segoe-ui", "segoeui.ttf"),
        ("segoe-symbol", "seguisym.ttf"),
    ] {
        if let Ok(bytes) = std::fs::read(font_dir.join(file)) {
            fonts
                .font_data
                .insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            primary.push(name.to_owned());
        }
    }
    let proportional = fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default();
    for (i, name) in primary.into_iter().enumerate() {
        proportional.insert(i, name);
    }
    ctx.set_fonts(fonts);
}

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
            self.add_dropped_paths(dropped);
        }
        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering_files && !self.is_running() {
            paint_drop_overlay(ctx);
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
            || self.batch_collecting.is_some()
            || self.treemap_collecting;
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
        let can_start = !running
            && !self.items.is_empty()
            && self.thread_error().is_none()
            && (!self.has_dangerous_paths || self.danger_confirmed);

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) && !running {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) && can_start {
            self.start();
        }

        let panel = egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(MARGIN)),
            )
            .show(ctx, |ui| {
                if self.items.is_empty() {
                    render_empty_state(ui, ctx);
                } else {
                    render_dialog(ui, ctx, self, &counts, &progress, can_start);
                }
                ui.min_rect().height()
            });

        fit_window_to_content(ctx, panel.inner);
    }
}

/// Size the (non-resizable) window to its content so no state needs a
/// hand-tuned height constant.
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

fn render_empty_state(ui: &mut egui::Ui, ctx: &egui::Context) {
    ui.heading("Nothing to delete");
    ui.add_space(2.0);
    ui.label(
        RichText::new("Drop files or folders here, or use “Delete with Zap” in Explorer.")
            .color(Palette::for_ui(ui).muted),
    );
    ui.add_space(14.0);
    // Nest the right-to-left layout in a row: at panel level it would claim
    // all remaining height and defeat fit_window_to_content.
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Close").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    });
}

fn render_dialog(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    app: &mut ZapApp,
    counts: &StatusCounts,
    progress: &RunProgress,
    can_start: bool,
) {
    let phase = Phase::of(app, counts);
    render_header(ui, app, phase);

    if app.items.len() > 1 || matches!(phase, Phase::Failed | Phase::Stopped) {
        ui.add_space(10.0);
        render_item_list(ui, app);
    }

    if app.has_dangerous_paths && phase == Phase::Ready {
        ui.add_space(10.0);
        render_danger_card(ui, app);
    }

    ui.add_space(12.0);
    match phase {
        Phase::Ready => render_options(ui, app),
        Phase::Running | Phase::Paused => render_progress(ui, app, progress, phase),
        Phase::Succeeded | Phase::Previewed => render_success(ui, app),
        Phase::Failed | Phase::Stopped => render_failures(ui, app, counts, phase),
    }

    ui.add_space(14.0);
    render_actions(ui, ctx, app, phase, can_start);

    if app.show_treemap {
        ui.add_space(10.0);
        render_treemap(ui, app);
    }
}

// ---------------------------------------------------------------------------
// Phase & header
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Ready,
    Running,
    Paused,
    Succeeded,
    Previewed,
    Failed,
    Stopped,
}

impl Phase {
    fn of(app: &ZapApp, counts: &StatusCounts) -> Self {
        if app.is_running() {
            return if app.is_paused() {
                Self::Paused
            } else {
                Self::Running
            };
        }
        if app.finished.is_none() {
            return Self::Ready;
        }
        let cancelled = cancelled_count(&app.items);
        if counts.failed > cancelled {
            Self::Failed
        } else if cancelled > 0 {
            Self::Stopped
        } else if app.dry_run {
            Self::Previewed
        } else {
            Self::Succeeded
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Delete,
    Recycle,
    Shred,
    Preview,
}

impl Mode {
    fn of(app: &ZapApp) -> Self {
        if app.dry_run {
            Self::Preview
        } else if app.recycle {
            Self::Recycle
        } else if app.shred {
            Self::Shred
        } else {
            Self::Delete
        }
    }
}

fn render_header(ui: &mut egui::Ui, app: &ZapApp, phase: Phase) {
    let pal = Palette::for_ui(ui);
    let mode = Mode::of(app);
    let (glyph, color) = match phase {
        Phase::Succeeded | Phase::Previewed => ("✓", pal.success),
        Phase::Failed => ("!", pal.danger),
        Phase::Stopped => ("■", pal.warn),
        Phase::Running | Phase::Paused => ("…", pal.accent),
        Phase::Ready => match mode {
            Mode::Delete | Mode::Shred => ("!", pal.danger),
            Mode::Recycle => ("↺", pal.warn),
            Mode::Preview => ("?", pal.accent),
        },
    };

    ui.horizontal(|ui| {
        paint_badge(ui, glyph, color);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(
                egui::Label::new(RichText::new(title(app, phase, mode)).heading().strong())
                    .truncate(),
            );
            if let Some(subtitle) = subtitle(&app.items) {
                ui.add(egui::Label::new(RichText::new(subtitle).color(pal.muted)).truncate())
                    .on_hover_text(full_selection_text(&app.items));
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                match size_summary(app) {
                    Some(text) => {
                        ui.label(RichText::new(text).small().color(pal.muted));
                    }
                    None => {
                        ui.add(egui::Spinner::new().size(11.0));
                        ui.label(RichText::new("Calculating size…").small().color(pal.muted));
                    }
                }
            });
        });
    });
}

fn paint_badge(ui: &mut egui::Ui, glyph: &str, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), color.gamma_multiply(0.18));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(21.0),
        color,
    );
}

fn title(app: &ZapApp, phase: Phase, mode: Mode) -> String {
    let what = match app.items.as_slice() {
        [item] => format!("“{}”", compact_text(&display_name(&item.path), 30)),
        items => format!("{} items", items.len()),
    };
    match phase {
        Phase::Ready => match mode {
            Mode::Delete => format!("Delete {what}?"),
            Mode::Recycle => format!("Move {what} to Recycle Bin?"),
            Mode::Shred => format!("Shred {what}?"),
            Mode::Preview => format!("Preview {what}"),
        },
        Phase::Running => match mode {
            Mode::Delete => "Deleting…".to_owned(),
            Mode::Recycle => "Moving to Recycle Bin…".to_owned(),
            Mode::Shred => "Shredding…".to_owned(),
            Mode::Preview => "Scanning…".to_owned(),
        },
        Phase::Paused => "Paused".to_owned(),
        Phase::Succeeded => match mode {
            Mode::Recycle => "Moved to Recycle Bin".to_owned(),
            Mode::Shred => "Shredded".to_owned(),
            _ => "Deleted".to_owned(),
        },
        Phase::Previewed => "Preview complete".to_owned(),
        Phase::Failed => "Some items could not be deleted".to_owned(),
        Phase::Stopped => "Stopped".to_owned(),
    }
}

/// Second header line: the full path for one item, the shared parent folder
/// for several items in one place, or a name sample otherwise.
fn subtitle(items: &[DeleteItem]) -> Option<String> {
    match items {
        [] => None,
        [item] => Some(item.path.display().to_string()),
        [first, rest @ ..] => {
            let parent = first.path.parent();
            if parent.is_some() && rest.iter().all(|i| i.path.parent() == parent) {
                Some(format!("in {}", parent?.display()))
            } else {
                Some(format!(
                    "{} and {} more",
                    display_name(&first.path),
                    rest.len()
                ))
            }
        }
    }
}

fn full_selection_text(items: &[DeleteItem]) -> String {
    let mut lines: Vec<String> = items
        .iter()
        .take(30)
        .map(|item| item.path.display().to_string())
        .collect();
    if items.len() > 30 {
        lines.push(format!("… and {} more", items.len() - 30));
    }
    lines.join("\n")
}

fn size_summary(app: &ZapApp) -> Option<String> {
    let guard = app.item_sizes.lock().ok()?;
    let snapshot = guard.as_ref()?;
    Some(format!(
        "{}  ·  {} {}",
        size::format_size(snapshot.total),
        format_count(snapshot.entries),
        if snapshot.entries == 1 {
            "item"
        } else {
            "items"
        }
    ))
}

// ---------------------------------------------------------------------------
// Selection list
// ---------------------------------------------------------------------------

fn render_item_list(ui: &mut egui::Ui, app: &mut ZapApp) {
    let pal = Palette::for_ui(ui);
    let editable = !app.is_running() && app.started_at.is_none();
    let mut remove: Option<usize> = None;
    // Read sizes under the lock instead of cloning the whole map per frame;
    // only the visible rows (`show_rows`) look anything up.
    let sizes_guard = app.item_sizes.lock().ok();
    let sizes = sizes_guard.as_ref().and_then(|g| g.as_ref());

    card(pal.card, pal.card_stroke).show(ui, |ui| {
        egui::ScrollArea::vertical()
            .max_height(ITEM_LIST_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show_rows(ui, 22.0, app.items.len(), |ui, range| {
                for index in range {
                    let item = &app.items[index];
                    let row = ui.horizontal(|ui| {
                        ui.set_min_height(22.0);
                        let (icon, color) = state_icon(&item.state, &pal);
                        ui.label(RichText::new(icon).color(color));
                        let right_width = 84.0;
                        let name_width = (ui.available_width() - right_width).max(40.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(name_width, 20.0),
                            Layout::left_to_right(Align::Center),
                            |ui| {
                                ui.add(egui::Label::new(display_name(&item.path)).truncate());
                            },
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if editable
                                && ui
                                    .add(egui::Button::new(RichText::new("✕").small()).frame(false))
                                    .on_hover_text("Remove from list")
                                    .clicked()
                            {
                                remove = Some(index);
                            }
                            match &item.state {
                                ItemState::Failed(err) => {
                                    let short = if err.contains(CANCELLED) {
                                        "skipped"
                                    } else {
                                        "failed"
                                    };
                                    ui.label(RichText::new(short).small().color(pal.danger));
                                }
                                _ => {
                                    if let Some(stats) =
                                        sizes.and_then(|s| s.by_path.get(&item.path))
                                    {
                                        ui.label(
                                            RichText::new(size::format_size(stats.bytes))
                                                .small()
                                                .color(pal.muted),
                                        );
                                    }
                                }
                            }
                        });
                    });
                    let hover = match &item.state {
                        ItemState::Failed(err) => format!("{}\n{}", item.path.display(), err),
                        _ => item.path.display().to_string(),
                    };
                    row.response.on_hover_text(hover);
                }
            });
    });

    drop(sizes_guard);
    if let Some(index) = remove {
        app.remove_item(index);
    }
}

fn state_icon(state: &ItemState, pal: &Palette) -> (&'static str, Color32) {
    match state {
        ItemState::Pending => ("○", pal.muted),
        ItemState::Running => ("▶", pal.accent),
        ItemState::Done => ("✓", pal.success),
        ItemState::Failed(err) if err.contains(CANCELLED) => ("–", pal.warn),
        ItemState::Failed(_) => ("✕", pal.danger),
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

fn render_danger_card(ui: &mut egui::Ui, app: &mut ZapApp) {
    let pal = Palette::for_ui(ui);
    card(pal.danger_soft, pal.danger.gamma_multiply(0.5)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(
            RichText::new("⚠  System folder selected")
                .strong()
                .color(pal.danger),
        );
        ui.label(
            RichText::new(
                "The selection includes Windows, Program Files or user profile folders. \
                 Deleting them can break the system or lose personal data.",
            )
            .small(),
        );
        ui.add_space(2.0);
        ui.checkbox(&mut app.danger_confirmed, "I understand, delete anyway");
    });
}

fn render_options(ui: &mut egui::Ui, app: &mut ZapApp) {
    let pal = Palette::for_ui(ui);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let segments = [
            ("Delete", !app.recycle && !app.shred),
            ("Recycle Bin", app.recycle),
            ("Shred", app.shred),
        ];
        for (index, (label, selected)) in segments.into_iter().enumerate() {
            let button = egui::Button::new(RichText::new(label).strong().color(if selected {
                Color32::WHITE
            } else {
                ui.visuals().text_color()
            }))
            .fill(if selected {
                pal.accent
            } else {
                ui.visuals().widgets.inactive.weak_bg_fill
            })
            .min_size(egui::vec2(0.0, 28.0));
            if ui.add(button).clicked() {
                app.recycle = index == 1;
                app.shred = index == 2;
            }
        }
    });

    ui.add_space(4.0);
    let hint = match Mode::of(app) {
        Mode::Delete => "Removed immediately — bypasses the Recycle Bin and can’t be restored.",
        Mode::Recycle => "Items can be restored from the Recycle Bin.",
        Mode::Shred => "File contents are overwritten before deletion. Slower; unrecoverable.",
        Mode::Preview => "Nothing will be deleted — Zap only checks what would be removed.",
    };
    ui.label(RichText::new(hint).small().color(pal.muted));

    ui.add_space(6.0);
    ui.checkbox(&mut app.dry_run, "Preview only (don’t delete anything)");

    egui::CollapsingHeader::new(RichText::new("Advanced").color(pal.muted))
        .id_salt("advanced")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut app.threads_enabled, "Limit worker threads");
                ui.add_enabled(
                    app.threads_enabled,
                    egui::TextEdit::singleline(&mut app.threads_text).desired_width(40.0),
                );
            });
            if let Some(error) = app.thread_error() {
                ui.colored_label(pal.danger, error);
            }
            ui.checkbox(&mut app.no_journal, "Don’t record this run in the journal");
        });
}

// ---------------------------------------------------------------------------
// Progress & results
// ---------------------------------------------------------------------------

/// Selection-wide progress, computed once per frame.
pub struct RunProgress {
    /// 0..=1, or None when nothing meaningful is known yet.
    pub fraction: Option<f32>,
    /// Entries processed so far and the total, when the size pass knows it.
    pub entries: Option<(u64, u64)>,
}

/// Progress weighted by entry counts from the background size pass:
/// deletion cost is per entry (each one is a kernel open + unlink), so a
/// folder of 100k tiny files dominates a single 10 GB file — byte weighting
/// would get the ETA badly wrong.
fn run_progress(app: &ZapApp, counts: &StatusCounts) -> RunProgress {
    let total_items = app.items.len();
    if total_items == 0 {
        return RunProgress {
            fraction: None,
            entries: None,
        };
    }
    // Bulk file-root runs report selection-wide progress: use it directly.
    if let Some(p) = app.items.iter().find_map(|item| {
        matches!(item.state, ItemState::Running)
            .then_some(item.progress.as_ref())
            .flatten()
            .filter(|p| p.bulk)
    }) {
        let len = p.length.unwrap_or(total_items as u64).max(1);
        return RunProgress {
            fraction: Some((p.position as f32 / len as f32).clamp(0.0, 1.0)),
            entries: Some((p.position, len)),
        };
    }

    if let Some(snapshot) = app.item_sizes.lock().ok().as_ref().and_then(|g| g.as_ref()) {
        if snapshot.entries > 0 {
            let mut done: u64 = 0;
            for item in &app.items {
                let entries = snapshot.by_path.get(&item.path).map_or(1, |s| s.entries);
                match &item.state {
                    ItemState::Done | ItemState::Failed(_) => done += entries,
                    ItemState::Running => {
                        let position = item.progress.as_ref().map_or(0, |p| match p.length {
                            Some(len) if len > 0 => {
                                (p.position as f64 / len as f64 * entries as f64) as u64
                            }
                            _ => p.position,
                        });
                        done += position.min(entries);
                    }
                    ItemState::Pending => {}
                }
            }
            let done = done.min(snapshot.entries);
            return RunProgress {
                fraction: Some(done as f32 / snapshot.entries as f32),
                entries: Some((done, snapshot.entries)),
            };
        }
    }

    let finished = (counts.done + counts.failed) as f32;
    RunProgress {
        fraction: (finished > 0.0).then(|| finished / total_items as f32),
        entries: None,
    }
}

fn render_progress(ui: &mut egui::Ui, app: &mut ZapApp, progress: &RunProgress, phase: Phase) {
    let pal = Palette::for_ui(ui);
    let fraction = progress.fraction.unwrap_or(0.0);
    let bar = egui::ProgressBar::new(fraction)
        .desired_height(14.0)
        .fill(if phase == Phase::Paused {
            pal.warn
        } else {
            pal.accent
        })
        .animate(phase == Phase::Running && progress.fraction.is_none());
    let bar = if progress.fraction.is_some() {
        bar.text(
            RichText::new(format!("{:.0}%", fraction * 100.0))
                .small()
                .color(Color32::WHITE),
        )
    } else {
        bar
    };
    ui.add(bar);

    ui.horizontal(|ui| {
        let elapsed = app.active_elapsed().unwrap_or_default().as_secs_f32();
        let left = match progress.entries {
            Some((done, total)) => {
                let mut text = format!("{} of {} items", format_count(done), format_count(total));
                if elapsed >= 0.5 && done > 0 {
                    text.push_str(&format!(
                        "  ·  {}/s",
                        format_count((done as f32 / elapsed) as u64)
                    ));
                }
                text
            }
            None => {
                let counts = app.status_counts();
                format!(
                    "{} of {} done",
                    counts.done + counts.failed,
                    app.items.len()
                )
            }
        };
        ui.label(RichText::new(left).small().color(pal.muted));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut right = format_duration(elapsed);
            if phase == Phase::Running {
                if let Some(eta) = estimate_eta_secs(progress.fraction, elapsed) {
                    right = format!("about {} left  ·  {right}", format_duration(eta));
                }
            }
            ui.label(RichText::new(right).small().color(pal.muted));
        });
    });
}

fn render_success(ui: &mut egui::Ui, app: &ZapApp) {
    let pal = Palette::for_ui(ui);
    let elapsed = app.finished.unwrap_or_default().as_secs_f32();
    let text = if app.dry_run {
        format!(
            "Nothing was deleted. {} would be removed.",
            size_summary(app).unwrap_or_else(|| "The selection".to_owned())
        )
    } else {
        format!("Finished in {}.", format_duration(elapsed))
    };
    ui.label(RichText::new(text).color(pal.success));
}

fn render_failures(ui: &mut egui::Ui, app: &ZapApp, counts: &StatusCounts, phase: Phase) {
    let pal = Palette::for_ui(ui);
    let cancelled = cancelled_count(&app.items);
    let failed = counts.failed - cancelled;
    let mut summary = Vec::new();
    if counts.done > 0 {
        summary.push(format!("{} done", counts.done));
    }
    if failed > 0 {
        summary.push(format!("{failed} failed"));
    }
    if cancelled > 0 {
        summary.push(format!("{cancelled} skipped"));
    }
    ui.label(RichText::new(summary.join("  ·  ")).strong());

    if phase == Phase::Failed {
        ui.add_space(4.0);
        card(pal.danger_soft, pal.danger.gamma_multiply(0.4)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .max_height(FAILURE_LIST_MAX_HEIGHT)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for item in &app.items {
                        if let ItemState::Failed(err) = &item.state {
                            if err.contains(CANCELLED) {
                                continue;
                            }
                            ui.add(
                                egui::Label::new(
                                    RichText::new(item.path.display().to_string())
                                        .small()
                                        .strong(),
                                )
                                .truncate(),
                            );
                            ui.label(RichText::new(err).small().color(pal.danger));
                            ui.add_space(4.0);
                        }
                    }
                });
        });
        ui.add_space(2.0);
        ui.label(
            RichText::new(
                "Tip: close programs that use these files, or run as administrator, then retry.",
            )
            .small()
            .color(pal.muted),
        );
    } else {
        ui.label(
            RichText::new("The run was stopped; remaining items were left untouched.")
                .small()
                .color(pal.muted),
        );
    }
}

fn render_actions(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    app: &mut ZapApp,
    phase: Phase,
    can_start: bool,
) {
    let pal = Palette::for_ui(ui);
    ui.horizontal(|ui| {
        // Left: secondary tools.
        let analyzer_label = if app.show_treemap {
            "Hide disk usage"
        } else {
            "Disk usage"
        };
        if ui
            .add_enabled(
                !app.is_running(),
                egui::Button::new(RichText::new(analyzer_label).color(pal.accent)).frame(false),
            )
            .clicked()
        {
            app.show_treemap = !app.show_treemap;
            if app.show_treemap {
                app.start_treemap_collection();
            }
        }
        if phase == Phase::Failed
            && ui
                .add(egui::Button::new(RichText::new("Copy report").color(pal.accent)).frame(false))
                .clicked()
        {
            ctx.copy_text(app.failure_report());
        }

        // Right: primary + secondary actions.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let size = egui::vec2(96.0, 32.0);
            match phase {
                Phase::Running | Phase::Paused => {
                    if ui.add(egui::Button::new("Stop").min_size(size)).clicked() {
                        zap::stop::request_stop();
                    }
                    let label = if phase == Phase::Paused {
                        "Resume"
                    } else {
                        "Pause"
                    };
                    if ui.add(egui::Button::new(label).min_size(size)).clicked() {
                        app.toggle_pause();
                    }
                }
                _ => {
                    let finished = app.finished.is_some();
                    let (label, fill) = match (Mode::of(app), finished) {
                        (_, true) if phase == Phase::Previewed => ("Preview again", pal.accent),
                        (_, true) => ("Retry", pal.danger),
                        (Mode::Preview, _) => ("Preview", pal.accent),
                        (Mode::Recycle, _) => ("Recycle", pal.warn),
                        (Mode::Shred, _) => ("Shred", pal.danger),
                        (Mode::Delete, _) => ("Delete", pal.danger),
                    };
                    let primary =
                        egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                            .fill(fill)
                            .min_size(size);
                    if ui
                        .add_enabled(can_start, primary)
                        .on_hover_text("Enter")
                        .clicked()
                    {
                        app.start();
                    }
                    let close_label = if finished { "Close" } else { "Cancel" };
                    if ui
                        .add(egui::Button::new(close_label).min_size(size))
                        .on_hover_text("Esc")
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        });
    });
}

fn render_treemap(ui: &mut egui::Ui, app: &mut ZapApp) {
    let width = ui.available_width();
    ui.allocate_ui(egui::vec2(width, TREEMAP_HEIGHT), |ui| {
        ui.set_min_height(TREEMAP_HEIGHT);
        // Render while holding the lock — the snapshot is only read here,
        // which avoids cloning every frame. Entries are pre-sorted and
        // capped by the collection thread; total is pre-computed.
        let guard = app.treemap_data.lock().unwrap();
        match guard.as_ref() {
            Some(snapshot) => {
                app.treemap_collecting = false;
                treemap::treemap_ui(ui, &snapshot.entries, snapshot.total);
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Spinner::new());
                });
            }
        }
    });
}

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

fn format_duration(secs: f32) -> String {
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 60.0 {
        format!("{}s", secs.round() as u64)
    } else if secs < 3600.0 {
        format!("{}m {:02}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
    } else {
        format!(
            "{}h {:02}m",
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
fn paint_drop_overlay(ctx: &egui::Context) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("drop_overlay"),
    ));
    let rect = ctx.screen_rect();
    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(150));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Drop to add to the list",
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
        assert_eq!(format_duration(1.25), "1.2s");
        assert_eq!(format_duration(42.4), "42s");
        assert_eq!(format_duration(125.0), "2m 05s");
        assert_eq!(format_duration(3_720.0), "1h 02m");
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
        assert_eq!(subtitle(&items).as_deref(), Some(r"in C:\work"));
    }
}
