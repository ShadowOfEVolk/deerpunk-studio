//! Design tokens ported from `deerpunk-inspector` (dark, teal accent) and
//! applied to the egui style. Data rows use the monospace font.

use eframe::egui::{self, Color32, Visuals};

pub const BG: Color32 = c("#090a0c");
pub const PANEL: Color32 = c("#14161c");
pub const HEADER: Color32 = c("#1a1d23");
pub const CARD: Color32 = c("#111318");
pub const CARD_BORDER: Color32 = c("#1c1f27");
pub const BORDER: Color32 = c("#2a2d35");
pub const ZEBRA: Color32 = c("#0d0f14");
pub const HOVER: Color32 = c("#1a1d2388");

pub const TEXT: Color32 = c("#d1d5db");
pub const TEXT_DIM: Color32 = c("#9ca3af");
pub const MUTED: Color32 = c("#6b7280");
pub const FAINT: Color32 = c("#4b5563");
pub const GHOST: Color32 = c("#374151");

pub const ACCENT: Color32 = c("#2cb1bc");
pub const ACCENT_TEXT: Color32 = c("#5eead4");
pub const CYAN: Color32 = c("#22d3ee");
pub const GREEN: Color32 = c("#22c55e");
pub const YELLOW: Color32 = c("#eab308");
pub const RED: Color32 = c("#ef4444");

// JsonView syntax colors (inspector JsonView.tsx).
pub const J_KEY: Color32 = c("#93c5fd");
pub const J_STRING: Color32 = c("#86efac");
pub const J_NUMBER: Color32 = c("#fdba74");
pub const J_BOOL: Color32 = c("#c4b5fd");
pub const J_NULL: Color32 = c("#6b7280");
pub const J_PUNCT: Color32 = c("#6b7280");

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
    Color32::from_rgba_unmultiplied_const(
        ((v >> 24) & 0xFF) as u8,
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
    )
}

/// Apply inspector palette to the egui style (dark, subtle borders).
pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        *v = Visuals::dark();
        v.dark_mode = true;
        v.panel_fill = PANEL;
        v.extreme_bg_color = BG;
        v.faint_bg_color = ZEBRA;
        v.window_fill = PANEL;
        v.widgets.noninteractive.bg_fill = HEADER;
        v.widgets.noninteractive.weak_bg_fill = HEADER;
        v.widgets.noninteractive.fg_stroke.color = TEXT_DIM;
        v.widgets.noninteractive.bg_stroke = stroke(BORDER);
        v.widgets.inactive.bg_fill = HEADER;
        v.widgets.inactive.weak_bg_fill = HEADER;
        v.widgets.inactive.bg_stroke = stroke(CARD_BORDER);
        v.widgets.hovered.bg_fill = HEADER;
        v.widgets.hovered.weak_bg_fill = HEADER;
        v.widgets.hovered.bg_stroke = stroke(BORDER);
        v.widgets.active.bg_fill = CARD;
        v.widgets.active.weak_bg_fill = CARD;
        v.widgets.active.bg_stroke = stroke(ACCENT);
        v.selection.bg_fill = with_a(ACCENT, 0x33);
        v.selection.stroke = stroke(ACCENT);
        v.override_text_color = Some(TEXT);
        v.window_stroke = stroke(BORDER);
        v.widgets.noninteractive.corner_radius = 4.0.into();
        v.widgets.inactive.corner_radius = 4.0.into();
        v.widgets.hovered.corner_radius = 4.0.into();
        v.widgets.active.corner_radius = 4.0.into();
        style.spacing.item_spacing = egui::vec2(8.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
    });
}

fn stroke(color: Color32) -> egui::Stroke {
    egui::Stroke::new(1.0, color)
}

fn with_a(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(c.r(), c.g(), c.b(), a)
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

/// Small pill badge (inspector `Badge` component).
pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    let fill = Color32::from_rgba_unmultiplied_const(color.r(), color.g(), color.b(), 0x22);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(mono_colored(text, color));
        });
}
