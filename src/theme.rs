use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use eframe::egui::style::WidgetVisuals;
use eframe::egui::{
    self, Color32, Context, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily,
    FontId, Frame, LayerId, Margin, RichText, Shadow, Stroke, TextStyle, Ui, Visuals, pos2, vec2,
};

/// Palette principale.
/// On garde un look cyber / hacker, mais lisible et propre.
pub const BG_GRAPHITE: Color32 = Color32::from_rgb(7, 12, 16);
pub const BG_DEEP: Color32 = Color32::from_rgb(10, 18, 24);
pub const BG_PANEL: Color32 = Color32::from_rgb(14, 24, 30);
pub const BG_PANEL_ALT: Color32 = Color32::from_rgb(18, 30, 38);
pub const BG_SURFACE: Color32 = Color32::from_rgb(23, 39, 48);

pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(229, 241, 238);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(142, 171, 174);

pub const ORANGE: Color32 = Color32::from_rgb(80, 248, 164);
pub const ORANGE_SOFT: Color32 = Color32::from_rgb(49, 186, 129);
pub const RED: Color32 = Color32::from_rgb(255, 99, 104);
pub const RED_SOFT: Color32 = Color32::from_rgb(176, 70, 78);
pub const CYAN: Color32 = Color32::from_rgb(89, 187, 255);
pub const BORDER: Color32 = Color32::from_rgb(34, 67, 73);
pub const GRID: Color32 = Color32::from_rgba_unmultiplied_const(80, 248, 164, 10);
pub const SCANLINE: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 2);
pub const WARNING: Color32 = Color32::from_rgb(255, 194, 87);

#[derive(Clone, Copy)]
pub enum CardTone {
    Default,
    Accent,
    Warning,
    Danger,
    Info,
}

/// Applique le thème global egui.
pub fn apply_hacker_theme(ctx: &Context) {
    install_windows_fonts(ctx);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = vec2(10.0, 10.0);
        style.spacing.window_margin = Margin::same(18);
        style.spacing.button_padding = vec2(14.0, 10.0);
        style.spacing.menu_margin = Margin::same(12);
        style.spacing.indent = 18.0;
        style.spacing.interact_size = vec2(0.0, 36.0);
        style.spacing.combo_width = 200.0;
        style.spacing.text_edit_width = 220.0;
        style.spacing.scroll.bar_width = 9.0;
        style.spacing.scroll.handle_min_length = 32.0;
        style.visuals = cyber_visuals();

        style.text_styles = BTreeMap::from([
            (
                TextStyle::Heading,
                FontId::new(28.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(15.5, FontFamily::Proportional)),
            (
                TextStyle::Monospace,
                FontId::new(14.5, FontFamily::Monospace),
            ),
            (
                TextStyle::Button,
                FontId::new(15.0, FontFamily::Proportional),
            ),
            (TextStyle::Small, FontId::new(12.0, FontFamily::Monospace)),
            (
                TextStyle::Name("Hero".into()),
                FontId::new(30.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Name("Metric".into()),
                FontId::new(25.0, FontFamily::Monospace),
            ),
            (
                TextStyle::Name("Section".into()),
                FontId::new(20.0, FontFamily::Proportional),
            ),
        ]);
    });
}

/// Carte générique.
pub fn panel_card(accent: Color32) -> Frame {
    Frame::new()
        .fill(BG_PANEL_ALT)
        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(18))
        .inner_margin(Margin::same(16))
        .shadow(Shadow {
            offset: [0, 6],
            blur: 18,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0, 0, 0, 90),
        })
}

/// Carte KPI / métrique.
pub fn metric_card_variant(tone: CardTone) -> Frame {
    let accent = match tone {
        CardTone::Default => ORANGE_SOFT,
        CardTone::Accent => ORANGE,
        CardTone::Warning => WARNING,
        CardTone::Danger => RED,
        CardTone::Info => CYAN,
    };

    Frame::new()
        .fill(BG_SURFACE)
        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.55)))
        .corner_radius(CornerRadius::same(16))
        .inner_margin(Margin::same(14))
        .shadow(Shadow {
            offset: [0, 4],
            blur: 14,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0, 0, 0, 72),
        })
}

/// Petite puce d’état.
pub fn status_chip(ui: &mut Ui, text: impl Into<String>, accent: Color32) {
    Frame::new()
        .fill(BG_SURFACE)
        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.75)))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text.into())
                    .size(12.0)
                    .monospace()
                    .color(accent),
            );
        });
}

/// En-tête homogène de section.
pub fn section_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.add_space(2.0);

    ui.label(
        RichText::new(title)
            .text_style(TextStyle::Name("Section".into()))
            .color(TEXT_PRIMARY),
    );
    ui.label(
        RichText::new(subtitle)
            .text_style(TextStyle::Small)
            .color(TEXT_SECONDARY),
    );

    ui.add_space(6.0);

    let width = ui.available_width().max(48.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 10.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let y = rect.center().y;

    painter.line_segment(
        [pos2(rect.left(), y), pos2(rect.right(), y)],
        Stroke::new(1.0, BORDER.gamma_multiply(0.8)),
    );
    painter.line_segment(
        [
            pos2(rect.left(), y),
            pos2(rect.left() + rect.width() * 0.24, y),
        ],
        Stroke::new(2.0, ORANGE.gamma_multiply(0.9)),
    );

    ui.add_space(8.0);
}

/// Frame de la sidebar.
pub fn sidebar_frame() -> Frame {
    Frame::new()
        .fill(BG_PANEL)
        .inner_margin(Margin::same(14))
        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.7)))
}

/// Frame des barres haute / basse.
pub fn topbar_frame() -> Frame {
    Frame::new()
        .fill(BG_PANEL)
        .inner_margin(Margin::symmetric(18, 12))
        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.7)))
}

/// Frame de la zone centrale.
/// Important : on ne peint plus le décor ici via une couche custom globale.
pub fn workspace_frame() -> Frame {
    Frame::new()
        .fill(Color32::TRANSPARENT)
        .inner_margin(Margin::same(18))
}

/// Frame semi-opaque posée au-dessus du décor central.
pub fn workspace_content_frame() -> Frame {
    Frame::new()
        .fill(Color32::from_rgba_unmultiplied(14, 24, 30, 228))
        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.75)))
        .corner_radius(CornerRadius::same(18))
        .inner_margin(Margin::same(18))
        .shadow(Shadow {
            offset: [0, 6],
            blur: 16,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0, 0, 0, 85),
        })
}

/// Petit bandeau d’info.
pub fn banner_frame(accent: Color32) -> Frame {
    Frame::new()
        .fill(BG_SURFACE)
        .inner_margin(Margin::symmetric(12, 10))
        .corner_radius(CornerRadius::same(14))
        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.65)))
}

/// Fond global très simple et sûr.
/// Ici on se contente d’un fond opaque, sans décor agressif.
pub fn paint_app_background(ctx: &Context) {
    let rect = ctx.input(|input| input.content_rect());
    let painter = ctx.layer_painter(LayerId::background());
    painter.rect_filled(rect, 0.0, BG_GRAPHITE);
}

/// Décor “cyber” uniquement dans la zone de travail centrale.
/// Comme on peint directement dans le `Ui` central AVANT le contenu,
/// le fond reste bien derrière les widgets.
pub fn paint_workspace_background(ui: &mut Ui) {
    let rect = ui.max_rect();
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, CornerRadius::same(20), BG_DEEP);

    // Halos doux.
    painter.circle_filled(
        pos2(rect.right() - 120.0, rect.top() + 80.0),
        240.0,
        ORANGE.gamma_multiply(0.04),
    );

    painter.circle_filled(
        pos2(rect.left() + 140.0, rect.bottom() - 120.0),
        280.0,
        CYAN.gamma_multiply(0.025),
    );

    // Grille large.
    let grid_step = 84.0;

    let mut gx = rect.left();
    while gx < rect.right() {
        painter.line_segment(
            [pos2(gx, rect.top()), pos2(gx, rect.bottom())],
            Stroke::new(1.0, GRID),
        );
        gx += grid_step;
    }

    let mut gy = rect.top();
    while gy < rect.bottom() {
        painter.line_segment(
            [pos2(rect.left(), gy), pos2(rect.right(), gy)],
            Stroke::new(1.0, GRID),
        );
        gy += grid_step;
    }

    // Scanlines très discrètes.
    let mut scan_y = rect.top();
    while scan_y < rect.bottom() {
        painter.line_segment(
            [pos2(rect.left(), scan_y), pos2(rect.right(), scan_y)],
            Stroke::new(1.0, SCANLINE),
        );
        scan_y += 18.0;
    }
}

pub fn tonal_text(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(TEXT_PRIMARY)
}

pub fn muted_text(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(TEXT_SECONDARY)
}

fn install_windows_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();

    load_font_into(
        &mut fonts,
        "bahnschrift",
        Path::new("C:\\Windows\\Fonts\\bahnschrift.ttf"),
        FontFamily::Proportional,
    );
    load_font_into(
        &mut fonts,
        "consolas",
        Path::new("C:\\Windows\\Fonts\\consola.ttf"),
        FontFamily::Monospace,
    );

    ctx.set_fonts(fonts);
}

fn load_font_into(fonts: &mut FontDefinitions, key: &str, path: &Path, family: FontFamily) {
    let Ok(bytes) = fs::read(path) else {
        return;
    };

    fonts
        .font_data
        .insert(key.to_owned(), FontData::from_owned(bytes).into());

    fonts
        .families
        .entry(family)
        .or_default()
        .insert(0, key.to_owned());
}

fn cyber_visuals() -> Visuals {
    let mut visuals = Visuals::dark();

    visuals.override_text_color = Some(TEXT_PRIMARY);
    visuals.weak_text_color = Some(TEXT_SECONDARY);
    visuals.hyperlink_color = CYAN;
    visuals.faint_bg_color = BG_PANEL_ALT;
    visuals.extreme_bg_color = BG_PANEL;
    visuals.code_bg_color = BG_SURFACE;
    visuals.text_edit_bg_color = Some(BG_SURFACE);
    visuals.warn_fg_color = WARNING;
    visuals.error_fg_color = RED;

    visuals.window_corner_radius = CornerRadius::same(20);
    visuals.window_fill = BG_PANEL_ALT;
    visuals.window_stroke = Stroke::new(1.0, BORDER.gamma_multiply(0.75));
    visuals.menu_corner_radius = CornerRadius::same(16);
    visuals.panel_fill = BG_PANEL;

    visuals.window_shadow = Shadow {
        offset: [0, 10],
        blur: 24,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(0, 0, 0, 120),
    };
    visuals.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 18,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(0, 0, 0, 110),
    };

    visuals.selection.bg_fill = ORANGE.gamma_multiply(0.22);
    visuals.selection.stroke = Stroke::new(1.0, ORANGE);

    visuals.button_frame = true;
    visuals.collapsing_header_frame = false;
    visuals.indent_has_left_vline = true;
    visuals.striped = true;
    visuals.slider_trailing_fill = true;
    visuals.interact_cursor = Some(CursorIcon::PointingHand);

    visuals.widgets.noninteractive = widget_visuals(
        BG_PANEL_ALT,
        BG_PANEL_ALT,
        Stroke::new(1.0, BORDER.gamma_multiply(0.65)),
        Stroke::new(1.0, TEXT_SECONDARY),
        14,
        0.0,
    );
    visuals.widgets.inactive = widget_visuals(
        BG_SURFACE,
        BG_SURFACE,
        Stroke::new(1.0, BORDER.gamma_multiply(0.9)),
        Stroke::new(1.0, TEXT_PRIMARY),
        14,
        0.0,
    );
    visuals.widgets.hovered = widget_visuals(
        Color32::from_rgb(27, 48, 53),
        Color32::from_rgb(27, 48, 53),
        Stroke::new(1.0, ORANGE.gamma_multiply(0.9)),
        Stroke::new(1.0, TEXT_PRIMARY),
        14,
        0.0,
    );
    visuals.widgets.active = widget_visuals(
        Color32::from_rgb(31, 59, 55),
        Color32::from_rgb(31, 59, 55),
        Stroke::new(1.0, ORANGE),
        Stroke::new(1.0, TEXT_PRIMARY),
        14,
        0.0,
    );
    visuals.widgets.open = widget_visuals(
        Color32::from_rgb(33, 53, 62),
        Color32::from_rgb(33, 53, 62),
        Stroke::new(1.0, CYAN.gamma_multiply(0.8)),
        Stroke::new(1.0, TEXT_PRIMARY),
        14,
        0.0,
    );

    visuals
}

fn widget_visuals(
    bg_fill: Color32,
    weak_bg_fill: Color32,
    bg_stroke: Stroke,
    fg_stroke: Stroke,
    radius: u8,
    expansion: f32,
) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill,
        weak_bg_fill,
        bg_stroke,
        corner_radius: CornerRadius::same(radius),
        fg_stroke,
        expansion,
    }
}
