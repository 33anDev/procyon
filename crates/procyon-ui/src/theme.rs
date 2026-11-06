//! Visual theme.
//!
//! A restrained, modern dark palette — near-black background, a single cyan
//! accent, generous rounding and spacing. Pulled out of the app logic so the
//! look can be tweaked (or a light variant added) in one place.

use egui::{Color32, Context, Rounding, Stroke, Visuals};

/// Accent used for selection, the active sort arrow, and focus.
pub const ACCENT: Color32 = Color32::from_rgb(56, 189, 248); // cyan-400
/// Warm red for the destructive "End" action.
pub const DANGER: Color32 = Color32::from_rgb(248, 113, 113); // red-400

const BG: Color32 = Color32::from_rgb(13, 17, 23); // near-black (github dark)
const PANEL: Color32 = Color32::from_rgb(22, 27, 34);
const ELEVATED: Color32 = Color32::from_rgb(33, 38, 45);
const TEXT: Color32 = Color32::from_rgb(201, 209, 217);
const TEXT_DIM: Color32 = Color32::from_rgb(125, 133, 144);

pub fn apply(ctx: &Context) {
    let mut v = Visuals::dark();

    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = PANEL; // text edit / scroll background
    v.faint_bg_color = Color32::from_rgb(18, 23, 30); // zebra striping
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT.linear_multiply(0.25);
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    let r = Rounding::same(7.0);
    v.widgets.noninteractive.rounding = r;
    v.widgets.inactive.rounding = r;
    v.widgets.hovered.rounding = r;
    v.widgets.active.rounding = r;

    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.inactive.bg_fill = ELEVATED;
    v.widgets.inactive.weak_bg_fill = ELEVATED;
    v.widgets.hovered.bg_fill = Color32::from_rgb(45, 51, 59);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(45, 51, 59);
    v.widgets.active.bg_fill = Color32::from_rgb(55, 62, 71);

    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

    let border = Stroke::new(1.0_f32, Color32::from_rgb(48, 54, 61));
    v.widgets.noninteractive.bg_stroke = border;
    v.widgets.inactive.bg_stroke = border;
    v.window_stroke = border;

    ctx.set_visuals(v);

    // A touch more breathing room than egui's defaults.
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 6.0);
        s.visuals.clip_rect_margin = 0.0;
    });
}

/// Dim helper for secondary text (user column, status bar).
pub fn dim() -> Color32 {
    TEXT_DIM
}