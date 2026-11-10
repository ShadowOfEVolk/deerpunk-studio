//! Font Awesome 6 Free (solid) as the studio's icon source — a font glyph
//! instead of the Lucide SVGs that half of the UI could not render.
//!
//! The webfont is embedded (`fa-solid-900.ttf`, Font Awesome 6.7.2 Free
//! license: CC BY 4.0) and registered with the gpui text system at startup.
//! Consumers never hard-code the family name: the macOS text system resolves
//! `select_family_by_name` against the name CoreText reports for the TTF
//! (`CTFontCopyFamilyName` → "Font Awesome 6 Free"), and a miss makes GPUI
//! panic on the first line it lays out — so always go through [`family()`].
//!
//! Codepoints are FA6 solid. Three icons of the requested set do not exist
//! in the Free 6.7.2 build (`activity` f6d0, `flame` f6df and
//! `square-terminal` e32a were dropped/never free); the nearest present
//! analogs are kept under the contract names, verified against the font's
//! cmap.

use std::borrow::Cow;

use gpui_kit::{
    AnyElement, App, IntoElement as _, ParentElement as _, Pixels, SharedString, Styled as _, div,
    relative,
};

/// Embedded Font Awesome 6 Free Solid webfont.
const FA_SOLID: &[u8] = include_bytes!("../assets/fa-solid-900.ttf");

/// Family name under which the embedded TTF resolves (CoreText reports the
/// preferred family, nameID 16). Matched by `MemSource::select_family_by_name`.
const FAMILY: &str = "Font Awesome 6 Free";

// The icon set, one `char` per FA6 solid codepoint. The table is the module's
// API surface: zones adopt icons progressively, so not every name has a
// callsite yet.
#[allow(dead_code)]
pub const GAMEPAD: char = '\u{f11b}';
#[allow(dead_code)]
pub const PLAY: char = '\u{f04b}';
#[allow(dead_code)]
pub const STOP: char = '\u{f04d}';
#[allow(dead_code)]
pub const SPINNER: char = '\u{f110}';
/// `activity` (f6d0) is not in FA Free 6.7.2 — pulse analog `heart-pulse`.
#[allow(dead_code)]
pub const ACTIVITY: char = '\u{f21e}';
#[allow(dead_code)]
pub const FOLDER: char = '\u{f07b}';
#[allow(dead_code)]
pub const FOLDER_OPEN: char = '\u{f07c}';
#[allow(dead_code)]
pub const SLIDERS: char = '\u{f1de}';
#[allow(dead_code)]
pub const GEAR: char = '\u{f013}';
/// `flame` (f6df) is not in FA Free 6.7.2 — FA6 rename `fire-flame-curved`.
#[allow(dead_code)]
pub const FLAME: char = '\u{f7e4}';
#[allow(dead_code)]
pub const TERMINAL: char = '\u{f120}';
#[allow(dead_code)]
pub const PLUG: char = '\u{f1e6}';
#[allow(dead_code)]
pub const PLUG_CIRCLE_XMARK: char = '\u{e560}';
#[allow(dead_code)]
pub const ROTATE: char = '\u{f2f1}';
#[allow(dead_code)]
pub const CIRCLE_EXCLAMATION: char = '\u{f06a}';
#[allow(dead_code)]
pub const CHECK: char = '\u{f00c}';
#[allow(dead_code)]
pub const CHEVRON_DOWN: char = '\u{f078}';
#[allow(dead_code)]
pub const LINK: char = '\u{f0c1}';
#[allow(dead_code)]
pub const DATABASE: char = '\u{f1c0}';
/// `square-terminal` (e32a) is not in FA Free 6.7.2 — same glyph as
/// [`TERMINAL`].
#[allow(dead_code)]
pub const SQUARE_TERMINAL: char = '\u{f120}';

/// The registered icon font family, as [`Font`](gpui_kit::Font) consumers
/// (`font_family`) expect it.
pub fn family() -> SharedString {
    SharedString::new_static(FAMILY)
}

/// The icon glyph as a one-character string (a `char` never heap-allocates in
/// a `SharedString`).
pub fn icon(code: char) -> SharedString {
    SharedString::from(code)
}

/// The glyph at `size`, inheriting the surrounding `text_color` — drop-in
/// replacement for an `Icon` element in any flex row/column.
pub fn sized(code: char, size: Pixels) -> AnyElement {
    div()
        .font_family(family())
        .text_size(size)
        // Match the line box to the glyph so surrounding `items_center`
        // layouts center the mark instead of a tall default line.
        .line_height(relative(1.))
        .child(icon(code))
        .into_any_element()
}

/// Registers the embedded webfont with the text system. Call once at
/// startup, before any window lays out text in [`family()`].
pub fn register(cx: &mut App) {
    cx.text_system()
        .add_fonts(vec![Cow::Borrowed(FA_SOLID)])
        .expect("failed to register the embedded Font Awesome 6 Free font");
}
