//! egui theme and rendering for the zapg dialog.

use std::path::Path;
use std::time::Duration;

use eframe::egui;
use zap::{size, treemap};

use crate::app::{should_auto_close, DeleteItem, ItemState, StatusCounts, ZapApp};
use crate::{WINDOW_HEIGHT_DANGEROUS, WINDOW_HEIGHT_NORMAL, WINDOW_HEIGHT_TREEMAP, WINDOW_WIDTH};

/// Extra window height for the per-item status list during multi-item runs.
const ITEM_LIST_HEIGHT: f32 = 88.0;

pub fn configure_style(ctx: &egui::Context) {
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

impl eframe::App for ZapApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        apply_dark_theme(ctx);
        self.poll_events();
        self.poll_batch_collection();
        self.poll_batch_session();

        #[cfg(windows)]
        {
            let counts = self.status_counts();
            let fraction = aggregate_progress(self, &counts);
            self.update_taskbar(fraction, &counts);
        }

        // Drag & drop: add files/folders dropped onto the window.
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
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

        let show_item_list = self.items.len() > 1 && (self.is_running() || self.finished.is_some());

        // Resize window for the current content.
        let mut target_h = if self.show_treemap {
            WINDOW_HEIGHT_TREEMAP
        } else if self.has_dangerous_paths {
            WINDOW_HEIGHT_DANGEROUS
        } else {
            WINDOW_HEIGHT_NORMAL
        };
        if show_item_list && !self.show_treemap {
            target_h += ITEM_LIST_HEIGHT;
        }
        let current = ctx.input(|i| i.screen_rect().height());
        if (current - target_h).abs() > 2.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::Vec2::new(
                WINDOW_WIDTH,
                target_h,
            )));
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

        let running = self.is_running();
        let finished = self.finished.is_some();
        let can_start = !running
            && self.thread_error().is_none()
            && (!self.has_dangerous_paths || self.danger_confirmed);

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
                if show_item_list {
                    ui.add_space(6.0);
                    render_item_list(ui, &self.items);
                }
                ui.add_space(10.0);
                render_options(ui, self);
                ui.add_space(10.0);
                render_actions(ui, ctx, self, running, finished, can_start);

                ui.add_space(6.0);
                render_treemap_section(ui, self);
            });
    }
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
                egui::RichText::new(selection_title(total, app.dry_run, app.recycle, app.shred))
                    .size(16.0)
                    .strong(),
            );
            let mut detail = selection_detail(&app.items);
            if let Some(sz) = app.total_size.lock().ok().and_then(|g| *g) {
                detail.push_str(&format!("  ({})", size::format_size(sz)));
            }
            ui.label(egui::RichText::new(detail).size(12.0));
        });
    });

    if app.has_dangerous_paths && !app.danger_confirmed {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("\u{26A0} Contains system paths — check below")
                .size(12.0)
                .color(egui::Color32::from_rgb(230, 170, 40)),
        );
    }

    if app.started_at.is_some() || app.finished.is_some() {
        ui.add_space(6.0);
        ui.label(egui::RichText::new(status_text(counts, app)).size(12.0));
        ui.add_space(4.0);
        let pb = egui::ProgressBar::new(progress)
            .animate(app.is_running())
            .desired_height(10.0);
        ui.add(pb);
    } else {
        ui.add_space(6.0);
        let hint = if app.recycle {
            "Items will be moved to the Recycle Bin and can be restored."
        } else if app.shred {
            "Files are overwritten before deletion. This cannot be undone."
        } else {
            "Deletion is permanent. Drive roots are refused."
        };
        ui.label(egui::RichText::new(hint).size(12.0).weak());
    }
}

/// Scrollable per-item status list shown during multi-item runs so the
/// user can see exactly which paths succeeded, failed, or are in flight.
fn render_item_list(ui: &mut egui::Ui, items: &[DeleteItem]) {
    egui::ScrollArea::vertical()
        .max_height(ITEM_LIST_HEIGHT - 8.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for item in items {
                ui.horizontal(|ui| {
                    let (icon, color) = match &item.state {
                        ItemState::Pending => ("\u{25CB}", egui::Color32::from_rgb(150, 154, 164)),
                        ItemState::Running => ("\u{25B6}", egui::Color32::from_rgb(80, 140, 220)),
                        ItemState::Done => ("\u{2713}", egui::Color32::from_rgb(80, 180, 100)),
                        ItemState::Failed(_) => ("\u{2715}", egui::Color32::from_rgb(210, 70, 70)),
                    };
                    ui.label(egui::RichText::new(icon).size(11.0).color(color));
                    let name = item
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| item.path.display().to_string());
                    ui.label(egui::RichText::new(compact_text(&name, 40)).size(11.0));
                    if let ItemState::Failed(err) = &item.state {
                        ui.label(
                            egui::RichText::new(compact_text(err, 36))
                                .size(11.0)
                                .color(color),
                        );
                    }
                });
            }
        });
}

fn render_options(ui: &mut egui::Ui, app: &mut ZapApp) {
    ui.add_enabled_ui(!app.is_running(), |ui| {
        ui.checkbox(&mut app.dry_run, "Preview only");
        if ui
            .checkbox(&mut app.recycle, "Move to Recycle Bin (recoverable)")
            .changed()
            && app.recycle
        {
            // Recycle and shred are mutually exclusive: shredded data
            // cannot be restored from the bin.
            app.shred = false;
        }
        if ui
            .checkbox(&mut app.shred, "Shred (overwrite data, unrecoverable)")
            .changed()
            && app.shred
        {
            app.recycle = false;
        }
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
        if app.has_dangerous_paths {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("System folder selected")
                    .size(12.0)
                    .color(egui::Color32::from_rgb(200, 60, 60)),
            );
            ui.checkbox(&mut app.danger_confirmed, "I understand — delete anyway");
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
        } else if app.recycle {
            "Recycle"
        } else if app.shred {
            "Shred"
        } else {
            "Delete"
        };
        let fill = if app.dry_run {
            if is_dark {
                egui::Color32::from_rgb(56, 120, 210)
            } else {
                egui::Color32::from_rgb(45, 110, 205)
            }
        } else if app.recycle {
            if is_dark {
                egui::Color32::from_rgb(190, 130, 35)
            } else {
                egui::Color32::from_rgb(200, 130, 25)
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
        let cancel_text = if is_dark {
            egui::Color32::from_rgb(210, 212, 216)
        } else {
            egui::Color32::from_rgb(50, 54, 60)
        };
        let cancel_btn = egui::Button::new(
            egui::RichText::new(if finished { "Close" } else { "Cancel" })
                .size(14.0)
                .color(cancel_text),
        )
        .fill(cancel_fill)
        .corner_radius(btn_radius)
        .min_size(egui::vec2(92.0, 34.0));
        if running {
            // While a run is active this button becomes a working Stop:
            // it raises the graceful-stop flag checked by the deletion
            // loops and the worker thread.
            let stop_btn =
                egui::Button::new(egui::RichText::new("Stop").size(14.0).color(cancel_text))
                    .fill(cancel_fill)
                    .corner_radius(btn_radius)
                    .min_size(egui::vec2(92.0, 34.0));
            if ui.add(stop_btn).clicked() {
                zap::stop::request_stop();
            }
            ui.add_space(8.0);
            // Pause blocks all deletion loops at their next checkpoint;
            // Resume releases them. Stop still works while paused.
            let pause_label = if app.is_paused() { "Resume" } else { "Pause" };
            let pause_btn = egui::Button::new(
                egui::RichText::new(pause_label)
                    .size(14.0)
                    .color(cancel_text),
            )
            .fill(cancel_fill)
            .corner_radius(btn_radius)
            .min_size(egui::vec2(92.0, 34.0));
            if ui.add(pause_btn).clicked() {
                app.toggle_pause();
            }
        } else if ui.add(cancel_btn).clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    });
}

fn render_treemap_section(ui: &mut egui::Ui, app: &mut ZapApp) {
    let toggle_label = if app.show_treemap {
        "\u{25B2} Hide disk analyzer"
    } else {
        "\u{25BC} Show disk analyzer"
    };
    if ui
        .add_enabled(!app.is_running(), egui::Button::new(toggle_label))
        .clicked()
    {
        app.show_treemap = !app.show_treemap;
        if app.show_treemap {
            app.start_treemap_collection();
        }
    }

    if app.show_treemap {
        ui.add_space(4.0);
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
                treemap::treemap_ui(ui, &[], 0);
            }
        }
    }
}

fn selection_title(total: usize, dry_run: bool, recycle: bool, shred: bool) -> String {
    let action = if dry_run {
        "Preview"
    } else if recycle {
        "Recycle"
    } else if shred {
        "Shred"
    } else {
        "Delete"
    };
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
    let finished_cnt = counts.done + counts.failed;
    let total = app.items.len();
    let elapsed = app.active_elapsed();
    let mut text = if app.is_paused() {
        format!("Paused - {finished_cnt}/{total} complete")
    } else {
        format!("{finished_cnt}/{total} complete")
    };
    if counts.failed > 0 {
        text.push_str(&format!(", {} failed", counts.failed));
        if let Some(error) = first_failure(&app.items) {
            text.push_str(": ");
            text.push_str(&compact_text(error, 64));
        }
    }
    if let Some(d) = elapsed {
        text.push_str(&format!(" - {:.1}s", d.as_secs_f32()));
    }
    if app.is_running() && !app.is_paused() {
        if let Some(eta) = estimate_eta_secs(app, counts) {
            text.push_str(&format!(" - ETA ~{}", format_eta(eta)));
        }
    }
    if let Some(msg) = active_progress_message(&app.items) {
        text.push_str(" - ");
        text.push_str(msg);
    }
    text
}

/// Remaining-time estimate from byte-weighted progress and active elapsed
/// time. Byte weighting matters on mixed selections: deleting 90 small files
/// out of 100 means little if the 10 remaining are the multi-GB folders.
/// Returns None until enough progress exists for a stable estimate.
fn estimate_eta_secs(app: &ZapApp, counts: &StatusCounts) -> Option<f32> {
    let fraction = byte_weighted_progress(app, counts).unwrap_or_else(|| {
        // Sizes not computed yet — fall back to the count-based fraction.
        aggregate_progress(app, counts)
    });
    if !(0.02..1.0).contains(&fraction) {
        return None;
    }
    let elapsed = app.active_elapsed()?.as_secs_f32();
    if elapsed < 0.5 {
        return None;
    }
    Some(elapsed * (1.0 - fraction) / fraction)
}

fn format_eta(secs: f32) -> String {
    if secs < 60.0 {
        format!("{}s", secs.ceil() as u64)
    } else if secs < 3600.0 {
        format!("{}m {}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
    } else {
        format!(
            "{}h {}m",
            (secs / 3600.0) as u64,
            ((secs % 3600.0) / 60.0) as u64
        )
    }
}

/// Selection-wide progress weighted by per-root byte sizes (computed by the
/// background size pass). Completed items contribute their full size, the
/// running item its internal fraction. Returns None until sizes are known
/// or when the selection is all zero-sized.
fn byte_weighted_progress(app: &ZapApp, _counts: &StatusCounts) -> Option<f32> {
    let guard = app.item_sizes.lock().ok()?;
    let sizes = guard.as_ref()?;
    let total = sizes.total;
    if total == 0 {
        return None;
    }
    // The size thread pre-builds the map and total, so per frame this is a
    // single O(items) pass with O(1) lookups — no per-frame index rebuild.
    let mut done_bytes: f64 = 0.0;
    for item in &app.items {
        let size = sizes.by_path.get(&item.path).copied().unwrap_or(0) as f64;
        match &item.state {
            ItemState::Done | ItemState::Failed(_) => done_bytes += size,
            ItemState::Running => {
                done_bytes += size * item_progress_fraction(item) as f64;
            }
            ItemState::Pending => {}
        }
    }
    Some((done_bytes / total as f64).clamp(0.0, 1.0) as f32)
}

fn aggregate_progress(app: &ZapApp, counts: &StatusCounts) -> f32 {
    let total = app.items.len();
    if total == 0 {
        return 0.0;
    }
    // Prefer byte-weighted progress when the background size pass has
    // finished: on mixed selections it reflects real remaining work far
    // better than counting every item equally.
    if let Some(fraction) = byte_weighted_progress(app, counts) {
        return fraction;
    }
    // Bulk file-root runs report selection-wide progress: use it directly.
    // Weighting it per running item would keep the bar near zero for the
    // whole run and then jump to 100% at the end.
    let bulk = app.items.iter().find(|item| {
        matches!(item.state, ItemState::Running) && item.progress.as_ref().is_some_and(|p| p.bulk)
    });
    if let Some(item) = bulk {
        return item_progress_fraction(item);
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
        let msg = item.progress.as_ref()?.message.trim();
        (!msg.is_empty()).then_some(msg)
    })
}

fn first_failure(items: &[DeleteItem]) -> Option<&str> {
    items.iter().find_map(|item| match &item.state {
        ItemState::Failed(e) => Some(e.as_str()),
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

/// Dim the window and show a hint while files are dragged over it.
fn paint_drop_overlay(ctx: &egui::Context) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("drop_overlay"),
    ));
    let rect = ctx.screen_rect();
    painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(120));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Drop to add to the list",
        egui::FontId::proportional(16.0),
        egui::Color32::WHITE,
    );
}
