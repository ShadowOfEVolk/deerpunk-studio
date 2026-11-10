//! Blender-inspired dark theme: flat neutral grays, slightly lighter header
//! strips, sharp corners, dense layout, blue selection. Applied to the egui
//! style; data rows use the monospace font.

use eframe::egui::{self, Color32, FontId, TextStyle, Visuals};

// Base surfaces (Blender: editor body / header strips / inputs).
pub const BG: Color32 = c("#1d1d1d");
pub const PANEL: Color32 = c("#282828");
pub const HEADER: Color32 = c("#232323");
pub const CARD: Color32 = c("#2e2e2e");
pub const CARD_BORDER: Color32 = c("#3a3a3a");
pub const FIELD: Color32 = c("#353535");
pub const BORDER: Color32 = c("#3f3f3f");
pub const BORDER_DARK: Color32 = c("#161616");
pub const ZEBRA: Color32 = c("#2c2c2c");
pub const HOVER: Color32 = c("#3a3a3a");

// Text.
pub const TEXT: Color32 = c("#e6e6e6");
pub const TEXT_DIM: Color32 = c("#a0a0a0");
pub const MUTED: Color32 = c("#858585");
pub const FAINT: Color32 = c("#6e6e6e");
pub const GHOST: Color32 = c("#5a5a5a");

// Accents (Blender: blue selection, orange active object).
pub const ACCENT: Color32 = c("#4772b3");
pub const ACCENT_TEXT: Color32 = c("#cfe0f4");
pub const ORANGE: Color32 = c("#ff9c29");

// Status colors.
pub const GREEN: Color32 = c("#5f9e57");
pub const YELLOW: Color32 = c("#d0a13e");
pub const RED: Color32 = c("#d4483d");

// Tabs (workspace strip + dock tab bars).
pub const TAB_INACTIVE: Color32 = c("#9a9a9a");

/// Toolbar strip tint while an instance is building/running.
pub const BAR_ACTIVE: Color32 = c("#2b3950");

// JsonView syntax colors (desaturated to fit the neutral surface).
pub const J_KEY: Color32 = c("#9db9dd");
pub const J_STRING: Color32 = c("#a5d6a7");
pub const J_NUMBER: Color32 = c("#e6b980");
pub const J_BOOL: Color32 = c("#c3aed6");
pub const J_NULL: Color32 = c("#808080");
pub const J_PUNCT: Color32 = c("#808080");

// Console stream: Blender-console-level text, readable without glaring.
pub const CONSOLE_TEXT: Color32 = c("#c8c8c8");
/// Brightness above which a near-neutral ANSI color is softened to
/// `CONSOLE_TEXT` (white cargo lines should not glare).
const CONSOLE_BRIGHT_MAX: u8 = 230;
/// Max channel spread for an ANSI color to count as near-neutral.
const CONSOLE_NEUTRAL_SPREAD: u8 = 24;

const fn hex_val(b: u8) -> u32 {
    match b {
        b'0'..=b'9' => (b - b'0') as u32,
        b'a'..=b'f' => (b - b'a' + 10) as u32,
        b'A'..=b'F' => (b - b'A' + 10) as u32,
        _ => 0,
    }
}

const fn c(hex: &str) -> Color32 {
    let b = hex.as_bytes();
    let mut v: u32 = 0;
    let mut i = if b.len() == 9 { 1 } else { 0 };
    while i < b.len() {
        v = (v << 4) | hex_val(b[i]);
        i += 1;
    }
    if b.len() == 9 {
        Color32::from_rgba_unmultiplied_const(
            ((v >> 24) & 0xFF) as u8,
            ((v >> 16) & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        )
    } else {
        // #RRGGBB: opaque — never treat the blue byte as alpha.
        Color32::from_rgb(((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8)
    }
}

/// Soften bright near-neutral ANSI colors (white/gray) down to the console
/// text level; genuinely colored spans (green/cyan/yellow/red) pass through.
pub fn console_soften(color: Color32) -> Color32 {
    let (r, g, b) = (color.r(), color.g(), color.b());
    let max = r.max(g).max(b);
    let near_neutral = max - r.min(g).min(b) <= CONSOLE_NEUTRAL_SPREAD;
    if near_neutral && max > CONSOLE_BRIGHT_MAX {
        CONSOLE_TEXT
    } else {
        color
    }
}

/// Apply the Blender-inspired look: flat, sharp, dense.
pub fn apply(ctx: &egui::Context) {
    // Pin the theme: a light OS appearance must never flip panels back to white.
    ctx.set_theme(egui::ThemePreference::Dark);
    // Icon font (Phosphor regular) as a proportional fallback + named family.
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::variants::Variant::Regular);
    ctx.set_fonts(fonts);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        *v = Visuals::dark();
        v.dark_mode = true;
        v.panel_fill = PANEL;
        v.extreme_bg_color = BG;
        v.faint_bg_color = ZEBRA;
        v.window_fill = PANEL;
        v.override_text_color = Some(TEXT);

        // Blender widgets: flat #353535 fields, #4772b3 press/selection.
        v.widgets.noninteractive.bg_fill = HEADER;
        v.widgets.noninteractive.weak_bg_fill = HEADER;
        v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT_DIM);
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER_DARK);
        v.widgets.inactive.bg_fill = FIELD;
        v.widgets.inactive.weak_bg_fill = FIELD;
        v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
        v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);
        v.widgets.hovered.bg_fill = HOVER;
        v.widgets.hovered.weak_bg_fill = HOVER;
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, BORDER);
        v.widgets.active.bg_fill = ACCENT;
        v.widgets.active.weak_bg_fill = ACCENT;
        v.widgets.active.bg_stroke = egui::Stroke::NONE;
        v.widgets.active.fg_stroke = egui::Stroke::new(1.0, Color32::WHITE);

        v.selection.bg_fill = ACCENT;
        v.selection.stroke = egui::Stroke::NONE;
        v.window_stroke = egui::Stroke::new(1.0, BORDER);
        v.window_fill = PANEL;
        v.window_shadow = egui::Shadow::NONE;
        v.popup_shadow = egui::Shadow::NONE;

        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
        ] {
            w.corner_radius = 2.0.into();
        }

        // Density.
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        style.spacing.button_padding = egui::vec2(8.0, 2.0);
        style.spacing.menu_margin = egui::Margin::same(2);
        style.spacing.interact_size = egui::vec2(60.0, 20.0);
        style.spacing.scroll.bar_inner_margin = 2.0;
        style.spacing.scroll.bar_width = 8.0;

        style.text_styles.insert(TextStyle::Body, FontId::new(13.0, egui::FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Button, FontId::new(13.0, egui::FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Small, FontId::new(11.0, egui::FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Monospace, FontId::new(13.0, egui::FontFamily::Monospace));
        style.text_styles.insert(TextStyle::Heading, FontId::new(15.0, egui::FontFamily::Proportional));
    });
}

/// Monospace rich text for data rows.
pub fn mono(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).monospace()
}

/// Monospace text in the primary text color.
pub fn mono_text(text: impl Into<String>) -> egui::RichText {
    mono(text).color(TEXT)
}

/// Monospace text in an explicit color.
pub fn mono_colored(text: impl Into<String>, color: Color32) -> egui::RichText {
    mono(text).color(color)
}

/// Small dim text (status bar).
pub fn small(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).small().color(TEXT_DIM)
}

/// Small pill badge.
pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    let fill = Color32::from_rgba_unmultiplied_const(color.r(), color.g(), color.b(), 0x22);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(2.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(mono_colored(text, color));
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `#RRGGBB` constants must be fully opaque — a hex parser
    /// bug once read the blue byte as alpha, making every fills ~11% opaque
    /// and the whole UI look washed-out white.
    #[test]
    fn palette_colors_are_opaque() {
        for (name, color) in [
            ("BG", BG),
            ("PANEL", PANEL),
            ("HEADER", HEADER),
            ("CARD", CARD),
            ("FIELD", FIELD),
            ("TEXT", TEXT),
            ("ACCENT", ACCENT),
            ("CONSOLE_TEXT", CONSOLE_TEXT),
        ] {
            assert_eq!(color.a(), 255, "{name} must be opaque");
        }
        assert_eq!(BG, Color32::from_rgb(0x1d, 0x1d, 0x1d));
    }
}
