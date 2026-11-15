//! Dark theme modelled after the reference screenshot.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

pub const BG: Color32 = Color32::from_rgb(30, 30, 30);
pub const BAR_BG: Color32 = Color32::from_rgb(36, 36, 36);
pub const FIELD_BG: Color32 = Color32::from_rgb(14, 14, 14);
pub const BORDER: Color32 = Color32::from_rgb(58, 58, 58);
pub const ACCENT: Color32 = Color32::from_rgb(12, 102, 142);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(20, 120, 165);
pub const ROW_HOVER: Color32 = Color32::from_rgb(44, 48, 52);
pub const TEXT: Color32 = Color32::from_rgb(222, 222, 222);
pub const TEXT_DIM: Color32 = Color32::from_rgb(150, 150, 150);
pub const SEPARATOR: Color32 = Color32::from_rgb(52, 52, 52);
pub const BUTTON_BG: Color32 = Color32::from_rgb(60, 60, 60);
pub const DANGER: Color32 = Color32::from_rgb(200, 70, 60);
pub const WARN: Color32 = Color32::from_rgb(210, 160, 50);
pub const OK: Color32 = Color32::from_rgb(80, 170, 100);

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.global_style_mut(|style| {
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = Color32::from_rgb(38, 38, 38);
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.extreme_bg_color = FIELD_BG;
        v.faint_bg_color = Color32::from_rgb(34, 34, 34);
        v.override_text_color = None;
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.hyperlink_color = ACCENT_HOVER;
        v.window_corner_radius = CornerRadius::same(8);
        v.menu_corner_radius = CornerRadius::same(6);

        let w = &mut v.widgets;
        w.noninteractive.bg_stroke = Stroke::new(1.0, SEPARATOR);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.weak_bg_fill = BUTTON_BG;
        w.inactive.bg_fill = Color32::from_rgb(70, 70, 70);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = CornerRadius::same(4);
        w.hovered.weak_bg_fill = Color32::from_rgb(78, 78, 78);
        w.hovered.bg_fill = Color32::from_rgb(85, 85, 85);
        w.hovered.corner_radius = CornerRadius::same(4);
        w.active.weak_bg_fill = ACCENT;
        w.active.bg_fill = ACCENT;
        w.active.corner_radius = CornerRadius::same(4);
        w.open.weak_bg_fill = Color32::from_rgb(70, 70, 70);

        style.spacing.button_padding = egui::vec2(7.0, 3.0);
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.interact_size.y = 22.0;
        style.interaction.selectable_labels = false;
    });
}
