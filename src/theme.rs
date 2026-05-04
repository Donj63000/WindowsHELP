use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use eframe::egui::style::WidgetVisuals;
use eframe::egui::{
    Color32, Context, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId,
    Frame, LayerId, Margin, RichText, Shadow, Stroke, TextStyle, Ui, Visuals, pos2, vec2,
};

pub const BG_GRAPHITE: Color32 = Color32::from_rgb(9, 15, 19);
pub const BG_DEEP: Color32 = Color32::from_rgb(12, 20, 25);
pub const BG_PANEL: Color32 = Color32::from_rgb(15, 25, 31);
pub const BG_PANEL_ALT: Color32 = Color32::from_rgb(18, 30, 37);
pub const BG_SURFACE: Color32 = Color32::from_rgb(22, 35, 43);
pub const BG_SURFACE_SOFT: Color32 = Color32::from_rgb(27, 42, 51);

pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(236, 243, 246);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(162, 175, 186);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(104, 122, 134);

pub const ORANGE: Color32 = Color32::from_rgb(38, 166, 255);
pub const ORANGE_SOFT: Color32 = Color32::from_rgb(94, 210, 112);
pub const RED: Color32 = Color32::from_rgb(255, 92, 70);
pub const RED_SOFT: Color32 = Color32::from_rgb(255, 154, 57);
pub const CYAN: Color32 = Color32::from_rgb(48, 177, 255);
pub const BORDER: Color32 = Color32::from_rgb(42, 56, 66);
pub const GRID: Color32 = Color32::from_rgba_unmultiplied_const(58, 82, 96, 38);
pub const SCANLINE: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 2);
pub const WARNING: Color32 = Color32::from_rgb(255, 179, 47);

#[derive(Clone, Copy)]
pub enum CardTone {
    Default,
    Accent,
    Warning,
    Danger,
    Info,
}

pub fn apply_hacker_theme(ctx: &Context) {
    install_windows_fonts(ctx);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = vec2(8.0, 8.0);
        style.spacing.window_margin = Margin::same(10);
        style.spacing.button_padding = vec2(10.0, 7.0);
        style.spacing.menu_margin = Margin::same(8);
        style.spacing.indent = 14.0;
        style.spacing.interact_size = vec2(0.0, 31.0);
        style.spacing.combo_width = 190.0;
        style.spacing.text_edit_width = 220.0;
        style.spacing.scroll.bar_width = 7.0;
        style.spacing.scroll.handle_min_length = 30.0;
        style.visuals = app_visuals();

        style.text_styles = BTreeMap::from([
            (
                TextStyle::Heading,
                FontId::new(20.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
            (
                TextStyle::Button,
                FontId::new(13.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Name("Hero".into()),
                FontId::new(17.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Name("Metric".into()),
                FontId::new(25.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Name("Section".into()),
                FontId::new(15.5, FontFamily::Proportional),
            ),
        ]);
    });
}

pub fn panel_card(accent: Color32) -> Frame {
    panel_frame().stroke(Stroke::new(1.0, accent.gamma_multiply(0.32)))
}

pub fn panel_frame() -> Frame {
    Frame::new()
        .fill(BG_PANEL_ALT)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(12))
        .shadow(Shadow {
            offset: [0, 6],
            blur: 16,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0, 0, 0, 92),
        })
}

pub fn metric_card_variant(tone: CardTone) -> Frame {
    let accent = tone_color(tone);

    Frame::new()
        .fill(BG_PANEL_ALT)
        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.28)))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(12))
        .shadow(Shadow {
            offset: [0, 4],
            blur: 12,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0, 0, 0, 70),
        })
}

pub fn tone_color(tone: CardTone) -> Color32 {
    match tone {
        CardTone::Default => TEXT_SECONDARY,
        CardTone::Accent => ORANGE_SOFT,
        CardTone::Warning => WARNING,
        CardTone::Danger => RED,
        CardTone::Info => CYAN,
    }
}

pub fn status_chip(ui: &mut Ui, text: impl Into<String>, accent: Color32) {
    Frame::new()
        .fill(BG_SURFACE)
        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.42)))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text.into())
                    .size(12.0)
                    .strong()
                    .color(TEXT_PRIMARY),
            );
        });
}

pub fn section_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .text_style(TextStyle::Name("Section".into()))
                .strong()
                .color(TEXT_PRIMARY),
        );
        if !subtitle.is_empty() {
            ui.label(RichText::new(subtitle).size(12.0).color(TEXT_MUTED));
        }
    });
    ui.add_space(6.0);
}

pub fn sidebar_frame() -> Frame {
    Frame::new()
        .fill(BG_PANEL)
        .inner_margin(Margin::ZERO)
        .stroke(Stroke::new(1.0, BORDER))
}

pub fn topbar_frame() -> Frame {
    Frame::new()
        .fill(BG_PANEL)
        .inner_margin(Margin::ZERO)
        .stroke(Stroke::new(1.0, BORDER))
}

pub fn workspace_frame() -> Frame {
    Frame::new().fill(BG_DEEP).inner_margin(Margin::same(12))
}

pub fn workspace_content_frame() -> Frame {
    Frame::new()
        .fill(Color32::TRANSPARENT)
        .inner_margin(Margin::ZERO)
}

pub fn banner_frame(accent: Color32) -> Frame {
    Frame::new()
        .fill(BG_SURFACE)
        .inner_margin(Margin::symmetric(10, 8))
        .corner_radius(CornerRadius::same(6))
        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.35)))
}

pub fn table_header_frame() -> Frame {
    Frame::new()
        .fill(Color32::from_rgb(20, 32, 39))
        .inner_margin(Margin::symmetric(10, 8))
        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.8)))
}

pub fn table_row_frame(selected: bool) -> Frame {
    let fill = if selected {
        Color32::from_rgb(20, 78, 112)
    } else {
        Color32::from_rgb(18, 29, 35)
    };
    let stroke = if selected {
        ORANGE.gamma_multiply(0.55)
    } else {
        BORDER.gamma_multiply(0.55)
    };

    Frame::new()
        .fill(fill)
        .inner_margin(Margin::symmetric(10, 7))
        .stroke(Stroke::new(1.0, stroke))
}

pub fn paint_app_background(ctx: &Context) {
    let rect = ctx.input(|input| input.content_rect());
    let painter = ctx.layer_painter(LayerId::background());
    painter.rect_filled(rect, 0.0, BG_GRAPHITE);
}

pub fn paint_workspace_background(ui: &mut Ui) {
    let rect = ui.max_rect();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, BG_DEEP);

    let mut gx = rect.left();
    while gx < rect.right() {
        painter.line_segment(
            [pos2(gx, rect.top()), pos2(gx, rect.bottom())],
            Stroke::new(1.0, GRID),
        );
        gx += 96.0;
    }

    let mut gy = rect.top();
    while gy < rect.bottom() {
        painter.line_segment(
            [pos2(rect.left(), gy), pos2(rect.right(), gy)],
            Stroke::new(1.0, GRID),
        );
        gy += 96.0;
    }

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
        "segoe-ui",
        Path::new("C:\\Windows\\Fonts\\segoeui.ttf"),
        FontFamily::Proportional,
    );
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

fn app_visuals() -> Visuals {
    let mut visuals = Visuals::dark();

    visuals.override_text_color = Some(TEXT_PRIMARY);
    visuals.weak_text_color = Some(TEXT_SECONDARY);
    visuals.hyperlink_color = CYAN;
    visuals.faint_bg_color = BG_PANEL_ALT;
    visuals.extreme_bg_color = BG_PANEL;
    visuals.code_bg_color = BG_SURFACE;
    visuals.text_edit_bg_color = Some(Color32::from_rgb(13, 22, 28));
    visuals.warn_fg_color = WARNING;
    visuals.error_fg_color = RED;

    visuals.window_corner_radius = CornerRadius::same(8);
    visuals.window_fill = BG_PANEL_ALT;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.menu_corner_radius = CornerRadius::same(7);
    visuals.panel_fill = BG_PANEL;

    visuals.window_shadow = Shadow {
        offset: [0, 8],
        blur: 22,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(0, 0, 0, 130),
    };
    visuals.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 18,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(0, 0, 0, 110),
    };

    visuals.selection.bg_fill = ORANGE.gamma_multiply(0.26);
    visuals.selection.stroke = Stroke::new(1.0, ORANGE);

    visuals.button_frame = true;
    visuals.collapsing_header_frame = false;
    visuals.indent_has_left_vline = false;
    visuals.striped = true;
    visuals.slider_trailing_fill = true;
    visuals.interact_cursor = Some(CursorIcon::PointingHand);

    visuals.widgets.noninteractive = widget_visuals(
        BG_PANEL_ALT,
        BG_PANEL_ALT,
        Stroke::new(1.0, BORDER.gamma_multiply(0.65)),
        Stroke::new(1.0, TEXT_SECONDARY),
        7,
        0.0,
    );
    visuals.widgets.inactive = widget_visuals(
        BG_SURFACE,
        BG_SURFACE,
        Stroke::new(1.0, BORDER.gamma_multiply(0.85)),
        Stroke::new(1.0, TEXT_PRIMARY),
        7,
        0.0,
    );
    visuals.widgets.hovered = widget_visuals(
        BG_SURFACE_SOFT,
        BG_SURFACE_SOFT,
        Stroke::new(1.0, ORANGE.gamma_multiply(0.75)),
        Stroke::new(1.0, TEXT_PRIMARY),
        7,
        0.0,
    );
    visuals.widgets.active = widget_visuals(
        Color32::from_rgb(22, 57, 79),
        Color32::from_rgb(22, 57, 79),
        Stroke::new(1.0, ORANGE),
        Stroke::new(1.0, TEXT_PRIMARY),
        7,
        0.0,
    );
    visuals.widgets.open = widget_visuals(
        BG_SURFACE_SOFT,
        BG_SURFACE_SOFT,
        Stroke::new(1.0, CYAN.gamma_multiply(0.8)),
        Stroke::new(1.0, TEXT_PRIMARY),
        7,
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
