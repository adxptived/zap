use super::*;

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// Semantic colours, resolved per frame for the active (light/dark) theme.
pub(super) struct Palette {
    pub(super) danger: Color32,
    pub(super) danger_soft: Color32,
    pub(super) warn: Color32,
    pub(super) success: Color32,
    pub(super) accent: Color32,
    pub(super) card: Color32,
    pub(super) card_stroke: Color32,
    pub(super) muted: Color32,
}

impl Palette {
    pub(super) fn for_ui(ui: &egui::Ui) -> Self {
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

pub(super) fn tune_visuals(mut v: egui::Visuals) -> egui::Visuals {
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
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(62, 66, 76));
        v.widgets.hovered.bg_fill = hover;
        v.widgets.hovered.weak_bg_fill = hover;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(56, 60, 70));
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(222, 224, 230));
        v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(210, 213, 220));
        v.selection.bg_fill = Color32::from_rgb(52, 98, 176);
    } else {
        let panel = Color32::from_rgb(246, 247, 249);
        v.panel_fill = panel;
        v.window_fill = panel;
        v.faint_bg_color = Color32::from_rgb(238, 240, 244);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(232, 235, 240);
        v.widgets.inactive.bg_fill = Color32::from_rgb(232, 235, 240);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(222, 226, 233);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(32, 35, 42));
        v.selection.bg_fill = Color32::from_rgb(64, 120, 220);
    }
    v
}

/// Use Segoe UI (the Windows UI font) when available: crisper than egui's
/// bundled font at small sizes and covers Cyrillic/Greek/Latin paths. Segoe
/// UI Symbol and the bundled fonts stay as fallbacks for symbols.
pub(super) fn install_fonts(ctx: &egui::Context) {
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
