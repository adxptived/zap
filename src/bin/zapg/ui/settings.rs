use super::*;

// ---------------------------------------------------------------------------
// Settings screen
// ---------------------------------------------------------------------------

pub(super) fn render_settings(ui: &mut egui::Ui, app: &mut ZapApp) {
    let pal = Palette::for_ui(ui);
    // Show the draft language immediately so the choice is previewed.
    let lang = app.settings_draft.language.resolve();

    ui.horizontal(|ui| {
        if ui
            .add(egui::Button::new(RichText::new("←").size(16.0)).frame(false))
            .on_hover_text(pick(lang, "Back (Esc)", "Назад (Esc)"))
            .clicked()
        {
            app.screen = Screen::Main;
        }
        ui.heading(RichText::new(pick(lang, "Settings", "Настройки")).strong());
    });
    ui.add_space(8.0);

    // Set inside the scroll area, acted on after it releases the draft.
    let mut open_journal = false;
    egui::ScrollArea::vertical()
        .max_height(SETTINGS_BODY_MAX_HEIGHT)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let draft = &mut app.settings_draft;

            section(ui, &pal, pick(lang, "General", "Общие"));
            ui.horizontal(|ui| {
                ui.label(pick(lang, "Language", "Язык"));
                let label = |l: Language| match l {
                    Language::Auto => pick(lang, "System default", "Как в системе"),
                    Language::English => "English",
                    Language::Russian => "Русский",
                };
                egui::ComboBox::from_id_salt("language")
                    .selected_text(label(draft.language))
                    .show_ui(ui, |ui| {
                        for l in Language::ALL {
                            ui.selectable_value(&mut draft.language, l, label(l));
                        }
                    });
            });

            ui.add_space(10.0);
            section(
                ui,
                &pal,
                pick(lang, "Explorer context menu", "Контекстное меню Проводника"),
            );
            ui.checkbox(
                &mut draft.menu.enabled,
                pick(
                    lang,
                    "Add Zap to the right-click menu",
                    "Добавить Zap в меню по правой кнопке",
                ),
            );
            ui.add_enabled_ui(draft.menu.enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.radio_value(
                        &mut draft.menu.layout,
                        MenuLayout::Submenu,
                        pick(lang, "Submenu “Zap ›”", "Подменю «Zap ›»"),
                    );
                    ui.radio_value(
                        &mut draft.menu.layout,
                        MenuLayout::Flat,
                        pick(lang, "Items directly in the menu", "Пункты прямо в меню"),
                    );
                });
                ui.checkbox(
                    &mut draft.menu.icons,
                    pick(lang, "Show icons", "Показывать иконки"),
                );
                ui.label(RichText::new(pick(lang, "Actions:", "Действия:")).color(pal.muted));
                ui.indent("menu-actions", |ui| {
                    ui.checkbox(
                        &mut draft.menu.show_dialog,
                        pick(
                            lang,
                            "Delete with confirmation…",
                            "Удалить с подтверждением…",
                        ),
                    );
                    ui.checkbox(
                        &mut draft.menu.show_recycle,
                        pick(lang, "Move to Recycle Bin", "Переместить в корзину"),
                    );
                    ui.checkbox(
                        &mut draft.menu.show_quick_delete,
                        pick(
                            lang,
                            "Quick delete (no confirmation)",
                            "Быстрое удаление (без подтверждения)",
                        ),
                    );
                });
                ui.checkbox(
                    &mut draft.menu.show_background,
                    pick(
                        lang,
                        "“Delete this folder…” on folder background",
                        "«Удалить эту папку…» на фоне папки",
                    ),
                );
                ui.checkbox(
                    &mut draft.menu.shift_only,
                    pick(lang, "Only when Shift is held", "Только при зажатом Shift"),
                );

                ui.add_space(4.0);
                ui.label(
                    RichText::new(pick(lang, "Preview:", "Как будет выглядеть:")).color(pal.muted),
                );
                render_menu_preview(ui, draft, lang, &pal);
            });

            ui.add_space(10.0);
            section(ui, &pal, pick(lang, "Deletion dialog", "Окно удаления"));
            ui.label(
                RichText::new(pick(lang, "Default action", "Действие по умолчанию"))
                    .color(pal.muted),
            );
            mode_switch(ui, &mut draft.default_mode, lang, &pal);
            ui.checkbox(
                &mut draft.auto_close,
                pick(
                    lang,
                    "Close automatically after success",
                    "Закрывать после успешного удаления",
                ),
            );
            ui.checkbox(
                &mut draft.notify_failures,
                pick(
                    lang,
                    "Show a message when a quick deletion fails",
                    "Сообщать об ошибках быстрого удаления",
                ),
            );
            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut draft.journal,
                    pick(lang, "Keep an operation journal", "Вести журнал операций"),
                );
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(pick(lang, "Open journal", "Открыть журнал"))
                                .color(pal.accent),
                        )
                        .frame(false),
                    )
                    .on_hover_text(zap::journal::journal_path().display().to_string())
                    .clicked()
                {
                    open_journal = true;
                }
            });
            ui.horizontal(|ui| {
                let mut limited = draft.threads.is_some();
                if ui
                    .checkbox(
                        &mut limited,
                        pick(lang, "Limit worker threads", "Ограничить число потоков"),
                    )
                    .changed()
                {
                    draft.threads = limited.then(|| zap::parallelism::worker_count(None));
                }
                if let Some(threads) = draft.threads.as_mut() {
                    ui.add(egui::DragValue::new(threads).range(1..=zap::parallelism::MAX_WORKERS));
                } else {
                    ui.label(RichText::new(pick(lang, "auto", "авто")).color(pal.muted));
                }
            });
        });

    if open_journal {
        app.open_journal(lang);
    }

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if let Some((message, is_error)) = &app.settings_status {
            let color = if *is_error { pal.danger } else { pal.success };
            ui.label(RichText::new(message).small().color(color));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let size = egui::vec2(96.0, 32.0);
            let dirty = app.settings_draft != app.settings;
            let save = egui::Button::new(
                RichText::new(pick(lang, "Save", "Сохранить"))
                    .strong()
                    .color(Color32::WHITE),
            )
            .fill(pal.accent)
            .min_size(size);
            if ui.add_enabled(dirty, save).clicked() {
                app.save_settings();
            }
            if ui
                .add(egui::Button::new(pick(lang, "Back", "Назад")).min_size(size))
                .clicked()
            {
                app.screen = Screen::Main;
            }
        });
    });
}

pub(super) fn section(ui: &mut egui::Ui, pal: &Palette, title: &str) {
    ui.label(RichText::new(title).strong());
    let rect = ui.available_rect_before_wrap();
    ui.painter().hline(
        rect.x_range(),
        rect.top(),
        Stroke::new(1.0_f32, pal.card_stroke),
    );
    ui.add_space(6.0);
}

/// Mock of the Explorer menu the current draft would register.
pub(super) fn render_menu_preview(
    ui: &mut egui::Ui,
    draft: &zap::settings::Settings,
    lang: Language,
    pal: &Palette,
) {
    let entries = zap::context_menu::preview(draft);
    card(pal.card, pal.card_stroke).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if entries.is_empty() {
            ui.label(
                RichText::new(pick(
                    lang,
                    "(nothing will be added)",
                    "(ничего не будет добавлено)",
                ))
                .small()
                .color(pal.muted),
            );
            return;
        }
        for entry in entries {
            if entry.separator_before {
                let rect = ui.available_rect_before_wrap();
                ui.painter().hline(
                    (rect.left() + 24.0)..=rect.right(),
                    rect.top() + 1.0,
                    Stroke::new(1.0_f32, pal.card_stroke),
                );
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                ui.add_space(f32::from(entry.depth) * 22.0);
                let icon = if entry.icon { "⚡" } else { "" };
                ui.add_sized(
                    [16.0, 18.0],
                    egui::Label::new(RichText::new(icon).color(Color32::from_rgb(240, 170, 40))),
                );
                ui.label(&entry.label);
                if entry.submenu {
                    ui.label(RichText::new("›").color(pal.muted));
                }
            });
        }
    });
}
