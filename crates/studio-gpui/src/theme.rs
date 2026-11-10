//! Target palette: dark violet-tinted editor (background #1e1a24, panels
//! #241f2c/#292331, violet accent #a78bfa). Ported from the Blender-inspired
//! egui palette (src/theme.rs), with background/accent overridden.

use gpui_kit::component::Theme;
use gpui_kit::{App, Hsla, hsla, px};

// Base surfaces (violet-tinted dark).
pub const BG: Hsla = h(0x1e1a24);
pub const PANEL: Hsla = h(0x241f2c);
pub const PANEL_ALT: Hsla = h(0x292331);
pub const HEADER: Hsla = h(0x201b26);
pub const CARD: Hsla = h(0x2d2736);
pub const FIELD: Hsla = h(0x342d3e);
pub const BORDER: Hsla = h(0x3a3344);
pub const HOVER: Hsla = h(0x3a3344);

// Text.
pub const TEXT: Hsla = h(0xd8d4e0);
pub const TEXT_DIM: Hsla = h(0xa39dae);
pub const MUTED: Hsla = h(0x8881a0);
pub const FAINT: Hsla = h(0x6e6682);

// Accent: violet.
pub const ACCENT: Hsla = h(0xa78bfa);
pub const ACCENT_TEXT: Hsla = h(0x1a1230);

// Status colors.
pub const GREEN: Hsla = h(0x5f9e57);
pub const YELLOW: Hsla = h(0xd0a13e);
pub const RED: Hsla = h(0xd4483d);

// JsonView syntax colors (desaturated to fit the neutral surface).
pub const J_KEY: Hsla = h(0x9db9dd);
pub const J_STRING: Hsla = h(0xa5d6a7);
pub const J_NUMBER: Hsla = h(0xe6b980);
pub const J_BOOL: Hsla = h(0xc3aed6);
pub const J_NULL: Hsla = h(0x808080);

const fn h(hex: u32) -> Hsla {
    let r = ((hex >> 16) & 0xff) as f32 / 255.;
    let g = ((hex >> 8) & 0xff) as f32 / 255.;
    let b = (hex & 0xff) as f32 / 255.;
    let max = if r > g {
        if r > b { r } else { b }
    } else if g > b {
        g
    } else {
        b
    };
    let min = if r < g {
        if r < b { r } else { b }
    } else if g < b {
        g
    } else {
        b
    };
    let l = (max + min) / 2.;
    let d = max - min;
    let (hue, sat) = if d <= 0. {
        (0., 0.)
    } else {
        let sat = if l > 0.5 {
            d / (2. - max - min)
        } else {
            d / (max + min)
        };
        let hue = if max == r {
            (g - b) / d + if g < b { 6. } else { 0. }
        } else if max == g {
            (b - r) / d + 2.
        } else {
            (r - g) / d + 4.
        };
        (hue / 6., sat)
    };
    hsla(hue, sat, l, 1.)
}

/// Apply the reference palette to the active gpui-kit theme.
pub fn apply(cx: &mut App) {
    Theme::update(cx, |t| {
        t.background = BG;
        t.foreground = TEXT;
        t.border = BORDER;
        t.primary = ACCENT;
        t.primary_foreground = ACCENT_TEXT;
        t.primary_hover = h(0xbca5fc);
        t.secondary = PANEL_ALT;
        t.secondary_foreground = TEXT;
        t.secondary_hover = HOVER;

        t.title_bar = HEADER;
        t.title_bar_border = BORDER;
        t.sidebar = PANEL;
        t.sidebar_border = BORDER;
        t.sidebar_foreground = TEXT;
        t.sidebar_accent = PANEL_ALT;
        t.sidebar_accent_foreground = TEXT;

        t.tab_bar = PANEL;
        t.tab = TEXT_DIM;
        t.tab_foreground = TEXT_DIM;
        t.tab_active = PANEL_ALT;
        t.tab_active_foreground = TEXT;

        t.input = FIELD;
        t.list_hover = HOVER;
        t.list_even = PANEL;
        t.muted = PANEL_ALT;
        t.muted_foreground = MUTED;
        t.popover = PANEL_ALT;
        t.popover_foreground = TEXT;

        t.success = GREEN;
        t.warning = YELLOW;
        t.danger = RED;
        t.link = ACCENT;
        t.ring = ACCENT;

        // Editor density: the egui reference runs body/button/mono at 13px,
        // small at 11px (src/theme.rs text_styles). `Root` feeds `font_size`
        // into `window.set_rem_size`, so every rem-based size (text_base,
        // paddings, gaps) scales down with it.
        t.font_size = px(13.);
        t.mono_font_size = px(13.);
    });
}
