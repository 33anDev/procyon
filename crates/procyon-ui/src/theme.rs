//! Visual theme.
//!
//! A deep near-black canvas with a cyan → violet accent, glassy elevated
//! panels and crisp rounding. All colors, fonts and spacing live here so the
//! look can be tweaked (or a light variant added) in one place; `app.rs`
//! only refers to the named constants and helpers below.

use egui::epaint::Shadow;
use egui::{
    Color32, Context, FontData, FontDefinitions, FontFamily, FontId, Frame, Margin, Rounding,
    Stroke, TextStyle, Visuals,
};

// --- Palette -----------------------------------------------------------------

/// Primary accent: selection, active sort column, focus, logo.
pub const ACCENT: Color32 = Color32::from_rgb(34, 211, 238); // cyan
/// Secondary accent, used in gradients and the CPU bar's hot end.
pub const VIOLET: Color32 = Color32::from_rgb(167, 139, 250);
/// Destructive actions and very hot CPU.
pub const DANGER: Color32 = Color32::from_rgb(248, 113, 113);
/// Success messages in the status bar.
pub const OK: Color32 = Color32::from_rgb(74, 222, 128);

/// Window background (the "canvas" behind the glass panels).
pub const BG: Color32 = Color32::from_rgb(7, 9, 13);
/// Translucent panel fill — the background glow shows faintly through it.
pub const GLASS: Color32 = Color32::from_rgba_premultiplied(14, 18, 26, 235);
/// Raised controls: buttons, chips, text fields.
pub const ELEVATED: Color32 = Color32::from_rgb(22, 27, 38);
/// Hairline borders on panels and controls.
pub const BORDER: Color32 = Color32::from_rgb(36, 43, 58);

pub const TEXT: Color32 = Color32::from_rgb(214, 222, 235);
pub const TEXT_BRIGHT: Color32 = Color32::from_rgb(244, 248, 255);
pub const TEXT_DIM: Color32 = Color32::from_rgb(124, 135, 152);

/// Corner radius used for panels; controls use slightly less.
pub const RADIUS: f32 = 10.0;

// --- Setup -------------------------------------------------------------------

/// Install fonts, visuals and spacing. Call once at startup.
pub fn apply(ctx: &Context) {
    install_fonts(ctx);

    let mut v = Visuals::dark();

    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = ELEVATED;
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.window_rounding = Rounding::same(RADIUS);
    v.window_shadow = soft_shadow();
    v.popup_shadow = soft_shadow();
    v.menu_rounding = Rounding::same(8.0);
    v.extreme_bg_color = Color32::from_rgb(11, 14, 20); // text field / scroll bg
    // Low-alpha overlays are built with `gamma_multiply` (premultiplied,
    // gamma space). `from_rgba_unmultiplied` premultiplies in linear space,
    // which makes faint white washes far brighter than intended.
    v.faint_bg_color = Color32::WHITE.gamma_multiply(0.022); // zebra
    v.hyperlink_color = ACCENT;

    // Selection: soft cyan wash, bright text on top.
    v.selection.bg_fill = ACCENT.gamma_multiply(0.16);
    v.selection.stroke = Stroke::new(1.0_f32, TEXT_BRIGHT);

    let r = Rounding::same(7.0);
    let w = &mut v.widgets;
    for state in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        state.rounding = r;
    }

    w.noninteractive.bg_fill = GLASS;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);

    w.inactive.bg_fill = ELEVATED;
    w.inactive.weak_bg_fill = ELEVATED;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);

    w.hovered.bg_fill = Color32::from_rgb(30, 37, 52);
    w.hovered.weak_bg_fill = Color32::from_rgb(30, 37, 52);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.55));
    w.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT_BRIGHT);

    w.active.bg_fill = Color32::from_rgb(38, 47, 66);
    w.active.weak_bg_fill = Color32::from_rgb(38, 47, 66);
    w.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.active.fg_stroke = Stroke::new(1.0_f32, TEXT_BRIGHT);

    w.open.weak_bg_fill = Color32::from_rgb(30, 37, 52);
    w.open.bg_stroke = Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.55));

    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.menu_margin = Margin::same(6.0);
        s.spacing.interact_size.y = 24.0;
        s.visuals.clip_rect_margin = 0.0;

        // Typography: one proportional family for prose, a monospace for
        // numbers so columns of digits line up.
        s.text_styles = [
            (TextStyle::Heading, FontId::proportional(18.0)),
            (TextStyle::Body, FontId::proportional(13.5)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();
    });
}

/// Prefer the native Windows UI fonts (Segoe UI, Cascadia Mono / Consolas)
/// when present; egui's bundled fonts stay as fallbacks for any glyph those
/// lack (e.g. ◆ and emoji), and are the only fonts on other platforms.
fn install_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();

    #[cfg(target_os = "windows")]
    {
        let dir = std::env::var("WINDIR")
            .map(|w| std::path::PathBuf::from(w).join("Fonts"))
            .unwrap_or_else(|_| "C:\\Windows\\Fonts".into());

        // First file that exists wins; read once at startup.
        let load = |candidates: &[&str]| {
            candidates.iter().find_map(|f| std::fs::read(dir.join(f)).ok())
        };

        if let Some(bytes) = load(&["segoeui.ttf"]) {
            fonts.font_data.insert("ui".into(), FontData::from_owned(bytes));
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".into());
        }
        if let Some(bytes) = load(&["CascadiaMono.ttf", "consola.ttf"]) {
            fonts.font_data.insert("mono".into(), FontData::from_owned(bytes));
            fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "mono".into());
        }
        // Last-resort fallback for symbols none of the above carry
        // (◆ logo, ▾▴ sort arrows, ⏸ ▶ menu glyphs).
        if let Some(bytes) = load(&["seguisym.ttf"]) {
            fonts.font_data.insert("symbols".into(), FontData::from_owned(bytes));
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts.families.entry(family).or_default().push("symbols".into());
            }
        }
    }

    ctx.set_fonts(fonts);
}

// --- Helpers used by the layout ---------------------------------------------

fn soft_shadow() -> Shadow {
    Shadow {
        offset: egui::vec2(0.0, 8.0),
        blur: 24.0,
        spread: 0.0,
        color: Color32::from_black_alpha(140),
    }
}

/// The glass "card" that holds the process table.
pub fn card() -> Frame {
    Frame {
        inner_margin: Margin::symmetric(10.0, 8.0),
        outer_margin: Margin::ZERO,
        rounding: Rounding::same(RADIUS),
        shadow: soft_shadow(),
        fill: GLASS,
        stroke: Stroke::new(1.0_f32, BORDER),
    }
}

/// Small rounded chip for header stats.
pub fn chip() -> Frame {
    Frame {
        inner_margin: Margin::symmetric(10.0, 5.0),
        outer_margin: Margin::ZERO,
        rounding: Rounding::same(8.0),
        shadow: Shadow::NONE,
        fill: ELEVATED,
        stroke: Stroke::new(1.0_f32, BORDER),
    }
}

/// Color for a CPU bar at `t` = 0..1 load: cyan → violet → red.
pub fn heat(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        lerp(ACCENT, VIOLET, t * 2.0)
    } else {
        lerp(VIOLET, DANGER, (t - 0.5) * 2.0)
    }
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Dim helper for secondary text.
pub fn dim() -> Color32 {
    TEXT_DIM
}
