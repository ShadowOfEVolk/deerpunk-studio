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
use std::time::Duration;

use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, ElementId, Hsla, IntoElement as _,
    ParentElement as _, Pixels, SharedString, Styled as _, Transformation, div, radians, relative,
    svg,
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
#[allow(dead_code)]
pub const COPY: char = '\u{f0c5}';
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

/// Ring-spinner mark, drawn as an embedded SVG instead of the FA font
/// glyph. GPUI's SVG pipeline renders a *single-color alpha mask* from the
/// bytes and tints it with the element's text color, so the stroke color
/// below is irrelevant — [`spinner`] controls the tint. The silhouette
/// (three-quarter circle stroke) matches the FA `spinner` f110 glyph so the
/// icon slot keeps its density when it starts rotating.
const SPINNER_RING: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M12 3.5a8.5 8.5 0 1 0 8.5 8.5" fill="none" stroke="#000" stroke-width="3" stroke-linecap="round"/></svg>"##;

/// Public rotation is only available on the `svg()` element (`Svg::
/// with_transformation`) — font glyphs cannot be rotated — so the animated
/// spinner must be SVG. [`svg().data()`] embeds the bytes directly and
/// bypasses the [`crate`](gpui) asset source entirely, keeping the helper
/// self-contained.
///
/// One revolution per second, re-laid out at most 12 fps (`with_max_fps`):
/// on a machine busy with a release build the spinner advances in coarse
/// steps instead of demanding 120 Hz. `with_animation` also respects
/// `App::reduce_motion`, rendering the unrotated ring.
///
/// `active` selects the animated ring; `false` renders the same ring
/// statically (no animation element, no frames scheduled).
pub fn spinner(size: Pixels, color: Hsla, id: impl Into<ElementId>, active: bool) -> AnyElement {
    let ring = svg()
        .data(SPINNER_RING)
        .size(size)
        .text_color(color)
        .flex_shrink_0();
    if active {
        ring.with_animation(
            id,
            Animation::new(Duration::from_secs(1))
                .repeat()
                .with_max_fps(12.),
            |ring, delta| {
                ring.with_transformation(Transformation::rotate(radians(
                    delta * std::f32::consts::TAU,
                )))
            },
        )
        .into_any_element()
    } else {
        ring.into_any_element()
    }
}

/// Registers the embedded webfont with the text system. Call once at
/// startup, before any window lays out text in [`family()`].
pub fn register(cx: &mut App) {
    cx.text_system()
        .add_fonts(vec![Cow::Borrowed(FA_SOLID)])
        .expect("failed to register the embedded Font Awesome 6 Free font");
}

// Tofu guard: every codepoint in the table above must exist in the embedded
// font's cmap — a miss renders .notdef boxes instead of the icon. The headless
// text system cannot rasterize font glyphs, so this byte-level check is the
// real verification of "the glyph exists in FA Free".
#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal cmap reader for the embedded TTF (format 4 and 12 subtables —
    /// everything FA Free ships), std-only.
    fn covered_codepoints() -> std::collections::BTreeSet<u32> {
        let data = FA_SOLID;
        let be16 = |off: usize| -> usize {
            u16::from_be_bytes([data[off], data[off + 1]]) as usize
        };
        let be32 = |off: usize| -> u32 {
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
        };
        let num_tables = be16(4);
        let mut cmap = None;
        for t in 0..num_tables {
            let rec = 12 + 16 * t;
            if data[rec..rec + 4] == *b"cmap" {
                cmap = Some(be32(rec + 8) as usize);
            }
        }
        let cmap = cmap.expect("embedded FA font must carry a cmap table");
        let mut best = None;
        for i in 0..be16(cmap + 2) {
            // Encoding record: platformID(2) + encodingID(2) + offset(4) —
            // the offset field sits at record_start + 4 (records begin at
            // cmap + 4, after version + numTables).
            let off = cmap + be32(cmap + 8 + 8 * i) as usize;
            let format = be16(off);
            match format {
                12 => best = Some(off),
                4 if best.is_none() => best = Some(off),
                _ => {}
            }
        }
        let off = best.expect("cmap must contain a format 4 or 12 subtable");
        let mut covered = std::collections::BTreeSet::new();
        match be16(off) {
            4 => {
                let seg_x2 = be16(off + 6);
                let seg = seg_x2 / 2;
                for i in 0..seg {
                    let start = be16(off + 14 + 2 * seg + 2 * i) as u32;
                    let end = be16(off + 14 + 2 * i).min(0xFFFF) as u32;
                    covered.extend(start..=end);
                }
            }
            12 => {
                let groups = be32(off + 12) as usize;
                for g in 0..groups {
                    let rec = off + 16 + 12 * g;
                    let (start, end) = (be32(rec), be32(rec + 4));
                    // The FA table's ranges are small; keep the scan bounded
                    // anyway in case of a pathological group.
                    if end - start < 1 << 20 {
                        covered.extend(start..=end);
                    }
                }
            }
            other => panic!("unexpected cmap format {other}"),
        }
        covered
    }

    #[test]
    fn every_icon_codepoint_exists_in_the_embedded_font() {
        let covered = covered_codepoints();
        let table: &[(&str, char)] = &[
            ("GAMEPAD", GAMEPAD),
            ("PLAY", PLAY),
            ("STOP", STOP),
            ("SPINNER", SPINNER),
            ("ACTIVITY", ACTIVITY),
            ("FOLDER", FOLDER),
            ("FOLDER_OPEN", FOLDER_OPEN),
            ("SLIDERS", SLIDERS),
            ("GEAR", GEAR),
            ("FLAME", FLAME),
            ("TERMINAL", TERMINAL),
            ("PLUG", PLUG),
            ("PLUG_CIRCLE_XMARK", PLUG_CIRCLE_XMARK),
            ("ROTATE", ROTATE),
            ("CIRCLE_EXCLAMATION", CIRCLE_EXCLAMATION),
            ("CHECK", CHECK),
            ("CHEVRON_DOWN", CHEVRON_DOWN),
            ("LINK", LINK),
            ("DATABASE", DATABASE),
            ("COPY", COPY),
            ("SQUARE_TERMINAL", SQUARE_TERMINAL),
        ];
        for (name, code) in table {
            assert!(
                covered.contains(&(*code as u32)),
                "{name} (U+{:04X}) is missing from fa-solid-900.ttf — it would render as tofu",
                *code as u32
            );
        }
    }
}
