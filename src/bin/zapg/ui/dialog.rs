use super::*;

// ---------------------------------------------------------------------------
// Main dialog
// ---------------------------------------------------------------------------

pub(super) fn render_dialog(
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Ready,
    Running,
    Paused,
    Succeeded,
    Previewed,
    Failed,
    Stopped,
}

impl Phase {
    pub(super) fn of(app: &ZapApp, counts: &StatusCounts) -> Self {
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
pub(super) enum Mode {
    Delete,
    Recycle,
    Shred,
    Preview,
}

impl Mode {
    pub(super) fn of(app: &ZapApp) -> Self {
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

pub(super) fn render_header(ui: &mut egui::Ui, app: &ZapApp, phase: Phase) {
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
            if let Some(subtitle) = subtitle(&app.items, app.lang) {
                ui.add(egui::Label::new(RichText::new(subtitle).color(pal.muted)).truncate())
                    .on_hover_text(full_selection_text(&app.items, app.lang));
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                match size_summary(app) {
                    Some(text) => {
                        ui.label(RichText::new(text).small().color(pal.muted));
                    }
                    None => {
                        ui.add(egui::Spinner::new().size(11.0));
                        ui.label(
                            RichText::new(pick(app.lang, "Calculating size…", "Подсчёт размера…"))
                                .small()
                                .color(pal.muted),
                        );
                    }
                }
            });
        });
    });
}

pub(super) fn paint_badge(ui: &mut egui::Ui, glyph: &str, color: Color32) {
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

pub(super) fn title(app: &ZapApp, phase: Phase, mode: Mode) -> String {
    let lang = app.lang;
    let ru = lang == Language::Russian;
    let what = match app.items.as_slice() {
        [item] => format!("«{}»", compact_text(&display_name(&item.path), 30)),
        items => {
            let n = items.len() as u64;
            format!("{n} {}", items_word(lang, n))
        }
    };
    let text = |en: String, ru_text: String| if ru { ru_text } else { en };
    match phase {
        Phase::Ready => match mode {
            Mode::Delete => text(format!("Delete {what}?"), format!("Удалить {what}?")),
            Mode::Recycle => text(
                format!("Move {what} to Recycle Bin?"),
                format!("Переместить {what} в корзину?"),
            ),
            Mode::Shred => text(
                format!("Securely delete {what}?"),
                format!("Удалить {what} с перезаписью?"),
            ),
            Mode::Preview => text(format!("Preview {what}"), format!("Проверка: {what}")),
        },
        Phase::Running => pick(
            lang,
            match mode {
                Mode::Delete => "Deleting…",
                Mode::Recycle => "Moving to Recycle Bin…",
                Mode::Shred => "Overwriting and deleting…",
                Mode::Preview => "Scanning…",
            },
            match mode {
                Mode::Delete => "Удаление…",
                Mode::Recycle => "Перемещение в корзину…",
                Mode::Shred => "Перезапись и удаление…",
                Mode::Preview => "Проверка…",
            },
        )
        .to_owned(),
        Phase::Paused => pick(lang, "Paused", "Пауза").to_owned(),
        Phase::Succeeded => pick(
            lang,
            match mode {
                Mode::Recycle => "Moved to Recycle Bin",
                Mode::Shred => "Securely deleted",
                _ => "Deleted",
            },
            match mode {
                Mode::Recycle => "Перемещено в корзину",
                Mode::Shred => "Удалено с перезаписью",
                _ => "Удалено",
            },
        )
        .to_owned(),
        Phase::Previewed => pick(lang, "Preview complete", "Проверка завершена").to_owned(),
        Phase::Failed => pick(
            lang,
            "Some items could not be deleted",
            "Не всё удалось удалить",
        )
        .to_owned(),
        Phase::Stopped => pick(lang, "Stopped", "Остановлено").to_owned(),
    }
}

/// Second header line: the full path for one item, the shared parent folder
/// for several items in one place, or a name sample otherwise.
pub(super) fn subtitle(items: &[DeleteItem], lang: Language) -> Option<String> {
    match items {
        [] => None,
        [item] => Some(item.path.display().to_string()),
        [first, rest @ ..] => {
            let parent = first.path.parent();
            if parent.is_some() && rest.iter().all(|i| i.path.parent() == parent) {
                Some(format!("{} {}", pick(lang, "in", "в"), parent?.display()))
            } else {
                Some(format!(
                    "{} {} {}",
                    display_name(&first.path),
                    pick(lang, "and", "и ещё"),
                    rest.len()
                ))
            }
        }
    }
}

pub(super) fn full_selection_text(items: &[DeleteItem], lang: Language) -> String {
    let mut lines: Vec<String> = items
        .iter()
        .take(30)
        .map(|item| item.path.display().to_string())
        .collect();
    if items.len() > 30 {
        lines.push(format!(
            "… {} {}",
            pick(lang, "and", "и ещё"),
            items.len() - 30
        ));
    }
    lines.join("\n")
}

/// "1 234 files, 56 folders · 1.2 GB" (links only when there are any).
pub(super) fn preview_summary(stats: &size::TreeStats, lang: Language) -> String {
    let mut parts = vec![
        format!(
            "{} {}",
            format_count(stats.files),
            files_word(lang, stats.files)
        ),
        format!(
            "{} {}",
            format_count(stats.dirs),
            folders_word(lang, stats.dirs)
        ),
    ];
    if stats.links > 0 {
        parts.push(format!(
            "{} {}",
            format_count(stats.links),
            links_word(lang, stats.links)
        ));
    }
    format!("{} · {}", parts.join(", "), size::format_size(stats.bytes))
}

pub(super) fn size_summary(app: &ZapApp) -> Option<String> {
    let guard = app.item_sizes.lock().ok()?;
    let snapshot = guard.as_ref()?;
    Some(format!(
        "{}  ·  {} {}",
        size::format_size(snapshot.total),
        format_count(snapshot.entries),
        items_word(app.lang, snapshot.entries)
    ))
}

// ---------------------------------------------------------------------------
// Selection list
// ---------------------------------------------------------------------------

pub(super) fn render_item_list(ui: &mut egui::Ui, app: &mut ZapApp) {
    let pal = Palette::for_ui(ui);
    let lang = app.lang;
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
                                    .on_hover_text(pick(
                                        lang,
                                        "Remove from list",
                                        "Убрать из списка",
                                    ))
                                    .clicked()
                            {
                                remove = Some(index);
                            }
                            match &item.state {
                                ItemState::Failed(err) => {
                                    let short = if err.contains(CANCELLED) {
                                        pick(lang, "skipped", "пропущено")
                                    } else {
                                        pick(lang, "failed", "ошибка")
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
                        ItemState::Failed(err) => {
                            format!("{}\n{}", item.path.display(), localize_error(err, lang))
                        }
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

pub(super) fn state_icon(state: &ItemState, pal: &Palette) -> (&'static str, Color32) {
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

pub(super) fn render_danger_card(ui: &mut egui::Ui, app: &mut ZapApp) {
    let pal = Palette::for_ui(ui);
    let lang = app.lang;
    card(pal.danger_soft, pal.danger.gamma_multiply(0.5)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(
            RichText::new(pick(
                lang,
                "⚠  System folder selected",
                "⚠  Выбрана системная папка",
            ))
            .strong()
            .color(pal.danger),
        );
        ui.label(
            RichText::new(pick(
                lang,
                "The selection includes Windows, Program Files or user profile folders. \
                 Deleting them can break the system or lose personal data.",
                "В выборе есть папки Windows, Program Files или профиля пользователя. \
                 Их удаление может сломать систему или уничтожить личные данные.",
            ))
            .small(),
        );
        ui.add_space(2.0);
        ui.checkbox(
            &mut app.danger_confirmed,
            pick(
                lang,
                "I understand, delete anyway",
                "Я понимаю, всё равно удалить",
            ),
        );
    });
}

pub(super) fn render_options(ui: &mut egui::Ui, app: &mut ZapApp) {
    let pal = Palette::for_ui(ui);
    let lang = app.lang;

    let mut mode = if app.recycle {
        DefaultMode::Recycle
    } else if app.shred {
        DefaultMode::Shred
    } else {
        DefaultMode::Delete
    };
    if mode_switch(ui, &mut mode, lang, &pal) {
        app.recycle = mode == DefaultMode::Recycle;
        app.shred = mode == DefaultMode::Shred;
    }

    ui.add_space(4.0);
    let hint = match Mode::of(app) {
        Mode::Delete => pick(
            lang,
            "Removed immediately — bypasses the Recycle Bin and can’t be restored.",
            "Удаляется сразу, минуя корзину. Восстановить будет нельзя.",
        ),
        Mode::Recycle => pick(
            lang,
            "Items can be restored from the Recycle Bin.",
            "Можно будет восстановить из корзины.",
        ),
        Mode::Shred => pick(
            lang,
            "Contents are overwritten 3 times, then deleted, so recovery tools cannot restore them. Slower; not guaranteed on SSDs.",
            "Содержимое трижды перезаписывается, затем файлы удаляются — восстановить их нельзя даже специальными программами. Медленнее; на SSD не гарантируется.",
        ),
        Mode::Preview => pick(
            lang,
            "Nothing will be deleted — Zap only checks what would be removed.",
            "Ничего не удаляется — Zap только проверит, что будет удалено.",
        ),
    };
    ui.label(RichText::new(hint).small().color(pal.muted));

    ui.add_space(6.0);
    ui.checkbox(
        &mut app.dry_run,
        pick(
            lang,
            "Preview only (don’t delete anything)",
            "Только проверить (ничего не удалять)",
        ),
    );

    egui::CollapsingHeader::new(
        RichText::new(pick(lang, "Advanced", "Дополнительно")).color(pal.muted),
    )
    .id_salt("advanced")
    .show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut app.threads_enabled,
                pick(lang, "Limit worker threads", "Ограничить число потоков"),
            );
            ui.add_enabled(
                app.threads_enabled,
                egui::TextEdit::singleline(&mut app.threads_text).desired_width(40.0),
            );
        });
        if app.thread_error().is_some() {
            ui.colored_label(
                pal.danger,
                pick(
                    lang,
                    "Thread limit must be between 1 and 64.",
                    "Число потоков должно быть от 1 до 64.",
                ),
            );
        }
        ui.checkbox(
            &mut app.no_journal,
            pick(
                lang,
                "Don’t record this run in the journal",
                "Не записывать в журнал операций",
            ),
        );
    });
}

/// Segmented Delete / Recycle Bin / Shred control. Returns true on change.
pub(super) fn mode_switch(
    ui: &mut egui::Ui,
    mode: &mut DefaultMode,
    lang: Language,
    pal: &Palette,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (value, label) in [
            (
                DefaultMode::Delete,
                pick(lang, "Delete permanently", "Удалить навсегда"),
            ),
            (DefaultMode::Recycle, pick(lang, "Recycle Bin", "В корзину")),
            (
                DefaultMode::Shred,
                pick(lang, "Secure delete", "Удалить с перезаписью"),
            ),
        ] {
            let selected = *mode == value;
            let text_color = if selected {
                Color32::WHITE
            } else {
                ui.visuals().text_color()
            };
            let button = egui::Button::new(RichText::new(label).strong().color(text_color))
                .fill(if selected {
                    pal.accent
                } else {
                    ui.visuals().widgets.inactive.weak_bg_fill
                })
                .min_size(egui::vec2(0.0, 28.0));
            if ui.add(button).clicked() && !selected {
                *mode = value;
                changed = true;
            }
        }
    });
    changed
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
pub(super) fn run_progress(app: &ZapApp, counts: &StatusCounts) -> RunProgress {
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

pub(super) fn render_progress(
    ui: &mut egui::Ui,
    app: &mut ZapApp,
    progress: &RunProgress,
    phase: Phase,
) {
    let pal = Palette::for_ui(ui);
    let lang = app.lang;
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
                let mut text = format!(
                    "{} {} {} {}",
                    format_count(done),
                    pick(lang, "of", "из"),
                    format_count(total),
                    items_word(lang, total)
                );
                if elapsed >= 0.5 && done > 0 {
                    text.push_str(&format!(
                        "  ·  {}/{}",
                        format_count((done as f32 / elapsed) as u64),
                        pick(lang, "s", "с")
                    ));
                }
                text
            }
            None => {
                let counts = app.status_counts();
                format!(
                    "{} {} {}",
                    counts.done + counts.failed,
                    pick(lang, "of", "из"),
                    app.items.len()
                )
            }
        };
        ui.label(RichText::new(left).small().color(pal.muted));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut right = format_duration(elapsed, lang);
            if phase == Phase::Running {
                if let Some(eta) = estimate_eta_secs(progress.fraction, elapsed) {
                    let eta = format_duration(eta, lang);
                    right = match lang {
                        Language::Russian => format!("осталось ~{eta}  ·  {right}"),
                        _ => format!("about {eta} left  ·  {right}"),
                    };
                }
            }
            ui.label(RichText::new(right).small().color(pal.muted));
        });
    });
}

pub(super) fn render_success(ui: &mut egui::Ui, app: &ZapApp) {
    let pal = Palette::for_ui(ui);
    let lang = app.lang;
    let elapsed = app.finished.unwrap_or_default().as_secs_f32();
    let text = if app.dry_run {
        format!(
            "{} {}",
            pick(
                lang,
                "Nothing was deleted. Would be removed:",
                "Ничего не удалено. Будет удалено:"
            ),
            preview_summary(&app.preview_total, lang)
        )
    } else {
        format!(
            "{} {}.",
            pick(lang, "Finished in", "Готово за"),
            format_duration(elapsed, lang)
        )
    };
    ui.label(RichText::new(text).color(pal.success));
}

pub(super) fn render_failures(
    ui: &mut egui::Ui,
    app: &ZapApp,
    counts: &StatusCounts,
    phase: Phase,
) {
    let pal = Palette::for_ui(ui);
    let lang = app.lang;
    let cancelled = cancelled_count(&app.items);
    let failed = counts.failed - cancelled;
    let mut summary = Vec::new();
    if counts.done > 0 {
        summary.push(format!("{} {}", counts.done, pick(lang, "done", "готово")));
    }
    if failed > 0 {
        summary.push(format!("{failed} {}", pick(lang, "failed", "с ошибкой")));
    }
    if cancelled > 0 {
        summary.push(format!(
            "{cancelled} {}",
            pick(lang, "skipped", "пропущено")
        ));
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
                            ui.label(
                                RichText::new(localize_error(err, lang))
                                    .small()
                                    .color(pal.danger),
                            );
                            ui.add_space(4.0);
                        }
                    }
                });
        });
        ui.add_space(2.0);
        ui.label(
            RichText::new(pick(
                lang,
                "Tip: close programs that use these files, or run as administrator, then retry.",
                "Совет: закройте программы, использующие эти файлы, или запустите от имени администратора и повторите.",
            ))
            .small()
            .color(pal.muted),
        );
        ui.horizontal(|ui| {
            let link = |text: &str| {
                egui::Button::new(RichText::new(text).small().color(pal.accent)).frame(false)
            };
            if ui
                .add(link(pick(lang, "Copy report", "Копировать отчёт")))
                .clicked()
            {
                ui.ctx().copy_text(app.failure_report());
            }
            #[cfg(windows)]
            if !crate::app::elevation::is_elevated()
                && ui
                    .add(link(pick(
                        lang,
                        "🛡 Retry as administrator",
                        "🛡 Повторить от имени администратора",
                    )))
                    .on_hover_text(pick(
                        lang,
                        "Retry only the failed items with administrator rights (UAC prompt).",
                        "Повторить только неудавшиеся элементы с правами администратора (запрос UAC).",
                    ))
                    .clicked()
                && app.retry_failed_elevated().is_ok()
            {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    } else {
        ui.label(
            RichText::new(pick(
                lang,
                "The run was stopped; remaining items were left untouched.",
                "Операция остановлена, оставшиеся элементы не тронуты.",
            ))
            .small()
            .color(pal.muted),
        );
    }
}

pub(super) fn render_actions(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    app: &mut ZapApp,
    phase: Phase,
    can_start: bool,
) {
    let pal = Palette::for_ui(ui);
    let lang = app.lang;
    ui.horizontal(|ui| {
        // Left: settings, labelled like on the home screen (a bare gear
        // was easy to miss). Hidden while a run is active.
        if !app.is_running()
            && ui
                .add(
                    egui::Button::new(RichText::new(settings_label(lang)).color(pal.accent))
                        .frame(false),
                )
                .clicked()
        {
            app.open_settings();
        }

        // Right: primary + secondary actions.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let size = egui::vec2(96.0, 32.0);
            match phase {
                Phase::Running | Phase::Paused => {
                    if ui
                        .add(egui::Button::new(pick(lang, "Stop", "Остановить")).min_size(size))
                        .clicked()
                    {
                        zap::stop::request_stop();
                    }
                    let label = if phase == Phase::Paused {
                        pick(lang, "Resume", "Продолжить")
                    } else {
                        pick(lang, "Pause", "Приостановить")
                    };
                    if ui.add(egui::Button::new(label).min_size(size)).clicked() {
                        app.toggle_pause();
                    }
                }
                _ => {
                    let finished = app.finished.is_some();
                    let (label, fill) = match (Mode::of(app), finished) {
                        (_, true) if phase == Phase::Previewed => {
                            (pick(lang, "Preview again", "Проверить снова"), pal.accent)
                        }
                        (_, true) => (pick(lang, "Retry", "Повторить"), pal.danger),
                        (Mode::Preview, _) => {
                            (pick(lang, "Run preview", "Выполнить проверку"), pal.accent)
                        }
                        (Mode::Recycle, _) => (
                            pick(lang, "Move to Recycle Bin", "Переместить в корзину"),
                            pal.warn,
                        ),
                        (Mode::Shred, _) => (
                            pick(lang, "Secure delete", "Удалить с перезаписью"),
                            pal.danger,
                        ),
                        (Mode::Delete, _) => (
                            pick(lang, "Delete permanently", "Удалить навсегда"),
                            pal.danger,
                        ),
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
                    let close_label = if finished {
                        pick(lang, "Close", "Закрыть")
                    } else {
                        pick(lang, "Cancel", "Отмена")
                    };
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
