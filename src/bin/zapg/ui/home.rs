use super::*;

// ---------------------------------------------------------------------------
// Home (no selection)
// ---------------------------------------------------------------------------

pub(super) fn render_home(ui: &mut egui::Ui, ctx: &egui::Context, app: &mut ZapApp) {
    let pal = Palette::for_ui(ui);
    let lang = app.lang;
    ui.heading(RichText::new("Zap").strong());
    ui.label(
        RichText::new(pick(
            lang,
            "Fast deletion for Windows.",
            "Быстрое удаление для Windows.",
        ))
        .color(pal.muted),
    );
    ui.add_space(12.0);

    // Drop zone.
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 120.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), pal.card);
    let dash = Stroke::new(1.5_f32, pal.muted.gamma_multiply(0.6));
    let r = rect.shrink(1.0);
    for (a, b) in [
        (r.left_top(), r.right_top()),
        (r.right_top(), r.right_bottom()),
        (r.right_bottom(), r.left_bottom()),
        (r.left_bottom(), r.left_top()),
    ] {
        painter.add(egui::Shape::dashed_line(&[a, b], dash, 6.0, 4.0));
    }
    painter.text(
        rect.center() - egui::vec2(0.0, 10.0),
        egui::Align2::CENTER_CENTER,
        pick(
            lang,
            "Drop files or folders here",
            "Перетащите сюда файлы или папки",
        ),
        egui::FontId::proportional(15.0),
        ui.visuals().text_color(),
    );
    painter.text(
        rect.center() + egui::vec2(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        pick(
            lang,
            "or right-click them in Explorer → Zap",
            "или нажмите на них ПКМ в Проводнике → Zap",
        ),
        egui::FontId::proportional(12.0),
        pal.muted,
    );

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        if ui
            .add(
                egui::Button::new(RichText::new(settings_label(lang)).color(pal.accent))
                    .frame(false),
            )
            .clicked()
        {
            app.open_settings();
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(pick(lang, "Close", "Закрыть"))
                        .min_size(egui::vec2(96.0, 32.0)),
                )
                .clicked()
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    });
}

pub(super) fn settings_label(lang: Language) -> &'static str {
    pick(lang, "⚙ Settings", "⚙ Настройки")
}
