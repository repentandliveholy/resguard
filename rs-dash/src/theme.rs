use egui::{Color32, CornerRadius, Margin, Shadow, Stroke, Style, Visuals};
use resguard::governor::Zone;
use resguard::model::MemoryPressure;

pub const BG_BASE: Color32 = Color32::from_rgb(11, 15, 22);
pub const BG_SURFACE: Color32 = Color32::from_rgb(17, 23, 34);
pub const BG_ELEVATED: Color32 = Color32::from_rgb(24, 32, 46);
pub const BG_HOVER: Color32 = Color32::from_rgb(31, 41, 58);

pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(36, 46, 66);
pub const BORDER_BRIGHT: Color32 = Color32::from_rgb(56, 72, 102);

pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(235, 241, 250);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(165, 178, 200);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(112, 126, 150);

pub const ACCENT_CYAN: Color32 = Color32::from_rgb(56, 189, 248);
pub const ACCENT_BLUE: Color32 = Color32::from_rgb(96, 165, 250);
pub const ACCENT_INDIGO: Color32 = Color32::from_rgb(129, 140, 248);
pub const ACCENT_PURPLE: Color32 = Color32::from_rgb(192, 132, 252);

pub const STATUS_GREEN: Color32 = Color32::from_rgb(52, 211, 153);
pub const STATUS_AMBER: Color32 = Color32::from_rgb(251, 191, 36);
pub const STATUS_ORANGE: Color32 = Color32::from_rgb(251, 146, 60);
pub const STATUS_RED: Color32 = Color32::from_rgb(248, 113, 113);
pub const STATUS_CRIMSON: Color32 = Color32::from_rgb(239, 68, 68);

pub fn apply_theme(ctx: &egui::Context) {
    let mut style = Style::default();
    let mut visuals = Visuals::dark();

    visuals.dark_mode = true;
    visuals.override_text_color = Some(TEXT_PRIMARY);
    visuals.panel_fill = BG_BASE;
    visuals.window_fill = BG_SURFACE;
    visuals.extreme_bg_color = Color32::from_rgb(8, 11, 16);
    visuals.faint_bg_color = BG_ELEVATED;

    visuals.widgets.noninteractive.bg_fill = BG_SURFACE;
    visuals.widgets.noninteractive.weak_bg_fill = BG_SURFACE;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_SECONDARY);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(8);

    visuals.widgets.inactive.bg_fill = BG_ELEVATED;
    visuals.widgets.inactive.weak_bg_fill = BG_ELEVATED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(6);

    visuals.widgets.hovered.bg_fill = BG_HOVER;
    visuals.widgets.hovered.weak_bg_fill = BG_HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_BRIGHT);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.5, TEXT_PRIMARY);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(6);

    visuals.widgets.active.bg_fill = Color32::from_rgb(38, 52, 74);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(38, 52, 74);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT_CYAN);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    visuals.widgets.active.corner_radius = CornerRadius::same(6);

    visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(56, 189, 248, 48);
    visuals.selection.stroke = Stroke::new(1.0, ACCENT_CYAN);

    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.window_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    visuals.window_shadow = Shadow::NONE;
    visuals.popup_shadow = Shadow::NONE;

    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.window_margin = Margin::same(12);

    ctx.set_style(style);
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BG_SURFACE)
        .stroke(Stroke::new(1.0, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(14))
}

pub fn subcard_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BG_ELEVATED)
        .stroke(Stroke::new(1.0, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(10))
}

pub fn pill_badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    let bg = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 34);
    let border = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 110);
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(99))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(text)
                    .size(11.0)
                    .strong()
                    .color(color),
            );
        });
}

pub fn zone_color(zone: Zone) -> Color32 {
    match zone {
        Zone::Normal => STATUS_GREEN,
        Zone::Soft => STATUS_AMBER,
        Zone::High => ACCENT_PURPLE,
        Zone::Cap => STATUS_RED,
        Zone::MemoryEmergency => STATUS_CRIMSON,
    }
}

pub fn zone_label(zone: Zone) -> &'static str {
    match zone {
        Zone::Normal => "NORMAL",
        Zone::Soft => "SOFT QoS",
        Zone::High => "HIGH DUTY",
        Zone::Cap => "CAP THROTTLE",
        Zone::MemoryEmergency => "MEM / SSD GUARD",
    }
}

pub fn pressure_color(p: MemoryPressure) -> Color32 {
    match p {
        MemoryPressure::Normal => STATUS_GREEN,
        MemoryPressure::Warning => STATUS_AMBER,
        MemoryPressure::Critical => STATUS_RED,
    }
}

pub fn pct_color(val: f64, warn: f64, crit: f64) -> Color32 {
    if val >= crit {
        STATUS_RED
    } else if val >= warn {
        STATUS_AMBER
    } else {
        STATUS_GREEN
    }
}
