//! File-viewer registry and the center Viewer panel.
//!
//! Viewers are matched declaratively — `/`-separated `key=value` spec
//! clauses (`ext=`, `magic=`, `bin=`) parsed by [`ViewerSpec`], with no
//! `if ext == …` chains — ported from the egui frontend (`src/viewers.rs`).
//! A matching [`Viewer`] carries an [`OpenFn`] that builds the gpui view
//! for the open file; built-ins cover RmlUi documents (structural tree),
//! raster images, SVG (through gpui's native `img`), text and a binary
//! info card. The [`ViewerBus`] entity lets the Files panel request a
//! file open; [`ViewersPanel`] subscribes to it, sniffs the file and
//! shows the resolved view, titled with the rel path.

use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::{ActiveTheme, Icon, IconName, h_flex, v_flex};
use gpui_kit::*;
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SyntectColor, StyleModifier, Theme as SyntectTheme, ThemeItem, ThemeSettings,
};
use syntect::parsing::SyntaxSet;

use crate::rml as rml_doc;
use deerpunk_studio::assets::{AssetKind, fmt_size};

use crate::state::StudioState;
use crate::theme;

// ------------------------------------------------------------------ registry

/// Bytes of the file head kept for magic matching + the binary sniff.
pub const HEAD_CAP: usize = 8 * 1024;

/// Everything a viewer open callback needs about the open file.
pub struct ViewCtx<'a> {
    /// Absolute path on disk.
    pub path: &'a Path,
    /// Display path relative to the gamedir.
    pub rel: &'a str,
    /// First `HEAD_CAP` bytes of the file (shorter for small files).
    pub head: &'a [u8],
    /// True when the head contains a NUL byte (binary-content heuristic).
    pub is_binary: bool,
    /// File size in bytes.
    pub size: u64,
    /// Asset-scan classification, when the file lives in the gamedir.
    pub kind: Option<AssetKind>,
}

impl ViewCtx<'_> {
    /// Lowercase extension without the leading dot ("" when absent).
    pub fn ext(&self) -> String {
        self.path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    /// Lossy UTF-8 decode of the head bytes.
    pub fn head_text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.head)
    }
}

/// Viewer open callback: builds the view shown for an open file.
pub type OpenFn = fn(&ViewCtx<'_>, &mut Window, &mut App) -> AnyView;

/// A registered viewer: display name + open callback.
#[derive(Clone, Copy)]
pub struct Viewer {
    pub name: &'static str,
    pub open: OpenFn,
}

/// Declarative viewer selector. Clauses are `/`-separated `key=value` pairs;
/// every present clause must match (AND). Within a clause, commas are
/// alternatives (OR):
///
/// - `ext=toml,json` — file extension (case-insensitive, no dot)
/// - `magic=89504E470D0A1A0A,FFD8FF` — head starts with any hex-byte prefix
/// - `bin=1` / `bin=0` — content sniff: binary (NUL byte in head) or text
///
/// An empty spec matches nothing (and is rejected by [`ViewerSpec::parse`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViewerSpec {
    exts: Vec<String>,
    magics: Vec<Vec<u8>>,
    binary: Option<bool>,
}

impl ViewerSpec {
    /// Parse a spec string like `ext=png,jpg/magic=89504E470D0A1A0A/bin=0`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut out = ViewerSpec::default();
        let mut any = false;
        for clause in spec.split('/') {
            let clause = clause.trim();
            if clause.is_empty() {
                return Err(format!("empty clause in `{spec}`"));
            }
            let (key, value) = clause
                .split_once('=')
                .ok_or_else(|| format!("clause `{clause}` has no `=`"))?;
            if value.is_empty() {
                return Err(format!("clause `{clause}` has an empty value"));
            }
            match key {
                "ext" => {
                    for e in value.split(',') {
                        let e = e.trim().trim_start_matches('.').to_ascii_lowercase();
                        if e.is_empty() {
                            return Err(format!("empty extension in `{clause}`"));
                        }
                        out.exts.push(e);
                    }
                }
                "magic" => {
                    for m in value.split(',') {
                        let m = m.trim();
                        if m.len() % 2 != 0 {
                            return Err(format!("odd-length hex magic `{m}`"));
                        }
                        let bytes = m
                            .bytes()
                            .map(|b| {
                                (b as char)
                                    .to_digit(16)
                                    .map(|d| d as u8)
                                    .ok_or_else(|| format!("bad hex digit in magic `{m}`"))
                            })
                            .collect::<Result<Vec<u8>, String>>()?;
                        // Hex digit pairs -> bytes.
                        let bytes = bytes
                            .chunks(2)
                            .map(|p| (p[0] << 4) | p[1])
                            .collect::<Vec<u8>>();
                        if bytes.is_empty() {
                            return Err(format!("empty magic in `{clause}`"));
                        }
                        out.magics.push(bytes);
                    }
                }
                "bin" => {
                    out.binary = Some(
                        value
                            .parse::<u8>()
                            .ok()
                            .filter(|v| *v <= 1)
                            .ok_or_else(|| format!("bin must be 0 or 1, got `{value}`"))?
                            == 1,
                    );
                }
                other => return Err(format!("unknown clause key `{other}`")),
            }
            any = true;
        }
        if !any {
            return Err("spec has no clauses".into());
        }
        Ok(out)
    }

    /// True when every clause of this spec holds for the file described by
    /// `ctx`. An empty spec matches nothing.
    pub fn matches(&self, ctx: &ViewCtx<'_>) -> bool {
        let mut any = false;
        if !self.exts.is_empty() {
            any = true;
            if !self.exts.iter().any(|e| e == &ctx.ext()) {
                return false;
            }
        }
        if !self.magics.is_empty() {
            any = true;
            if !self.magics.iter().any(|m| ctx.head.starts_with(m)) {
                return false;
            }
        }
        if let Some(bin) = self.binary {
            any = true;
            if ctx.is_binary != bin {
                return false;
            }
        }
        any
    }
}

struct Entry {
    spec: ViewerSpec,
    viewer: Viewer,
}

fn entry(spec: &str, name: &'static str, open: OpenFn) -> Entry {
    let spec = ViewerSpec::parse(spec)
        .unwrap_or_else(|e| panic!("viewer `{name}`: bad spec `{spec}`: {e}"));
    Entry {
        spec,
        viewer: Viewer { name, open },
    }
}

/// The registry, seeded with the built-ins in priority order: specific
/// specs before the text/binary catch-alls, so integrator `register` calls
/// (which append) still win over the catch-alls but lose to the built-ins.
static ENTRIES: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(|| {
    Mutex::new(vec![
        entry("ext=rml", "RmlUi", rml_view),
        entry(
            "ext=png,jpg,jpeg,bmp,gif,ico,tiff,tif,webp,qoi,tga,ppm,pgm,pnm,hdr,dds",
            "image",
            image_view,
        ),
        // Same raster set for extensionless / mislabeled files.
        entry("magic=89504E470D0A1A0A,FFD8FF", "image", image_view),
        entry("ext=svg", "svg", svg_view),
        entry("bin=0", "text", text_view),
        entry("bin=1", "binary", binary_view),
    ])
});

/// Register a viewer under `spec`. Registration order is priority: the
/// first registered viewer whose spec matches a file wins, so specific
/// specs must be registered before catch-alls.
///
/// Panics on a malformed spec — registration is startup-time code and a
/// bad spec is a programmer error. Use [`ViewerSpec::parse`] to validate
/// user-provided specs.
pub fn register(spec: &str, name: &'static str, open: OpenFn) {
    let entry = entry(spec, name, open);
    ENTRIES.lock().unwrap().push(entry);
}

/// First registered viewer matching `ctx`, if any.
pub fn resolve(ctx: &ViewCtx<'_>) -> Option<Viewer> {
    ENTRIES
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.spec.matches(ctx))
        .map(|e| e.viewer)
}

/// The binary card is the fallback when no spec matches.
const FALLBACK_VIEWER: Viewer = Viewer {
    name: "binary",
    open: binary_view,
};

// --------------------------------------------------------------------- sniff

/// Sniffed file head plus its binary verdict.
#[derive(Clone)]
pub struct Sniff {
    pub head: Vec<u8>,
    pub is_binary: bool,
}

/// Read the first [`HEAD_CAP`] bytes of `path` and decide whether the file
/// is binary (git-style heuristic: a NUL byte in the head). Never fails:
/// read errors yield an empty head and the binary verdict.
pub fn sniff(path: &Path) -> Sniff {
    let mut head = vec![0u8; HEAD_CAP];
    let n = std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    head.truncate(n);
    Sniff {
        is_binary: head.contains(&0),
        head,
    }
}

// ------------------------------------------------------------------ builtins

/// Monospace text styling resolved from the active gpui-kit theme.
fn mono(cx: &App) -> SharedString {
    cx.theme().mono_font_family.clone()
}

fn mono_px(cx: &App) -> Pixels {
    cx.theme().mono_font_size
}

/// `key value` row used by the summary/info cards.
fn kv_row(key: &str, value: &str, cx: &App) -> AnyElement {
    h_flex()
        .gap_2()
        .child(div().text_color(theme::TEXT).child(format!("{key}:")))
        .child(
            div()
                .font_family(mono(cx))
                .text_color(theme::TEXT_DIM)
                .child(value.to_string()),
        )
        .into_any_element()
}

/// Raster-image viewer: decoded once at open time into a [`RenderImage`],
/// shown at native size (downscaled only when larger than the pane).
fn image_view(ctx: &ViewCtx<'_>, _: &mut Window, cx: &mut App) -> AnyView {
    let decoded = std::fs::read(ctx.path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| decode_render_image(&bytes));
    let (render, error, dims) = match decoded {
        Ok(decoded) => {
            let dims = decoded.dims;
            (Some(Arc::new(decoded.image)), None, Some(dims))
        }
        Err(e) => (None, Some(e), None),
    };
    cx.new(|_| ImageView {
        render,
        error,
        dims,
        rel: ctx.rel.to_owned().into(),
        size: ctx.size,
    })
    .into()
}

struct DecodedImage {
    image: RenderImage,
    dims: (u32, u32),
}

fn decode_render_image(bytes: &[u8]) -> Result<DecodedImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let dims = (img.width(), img.height());
    let image = RenderImage::new([image::Frame::new(img.to_rgba8())]);
    Ok(DecodedImage { image, dims })
}

struct ImageView {
    render: Option<Arc<RenderImage>>,
    error: Option<String>,
    dims: Option<(u32, u32)>,
    rel: SharedString,
    size: u64,
}

impl Render for ImageView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let body = if let Some(decoded) = self.render.clone() {
            div()
                .flex_1()
                .min_h_0()
                .child(img(decoded).object_fit(ObjectFit::ScaleDown))
        } else {
            div().text_color(theme::YELLOW).child(format!(
                "cannot decode {}: {}",
                self.rel,
                self.error.clone().unwrap_or_default()
            ))
        };
        let dims = self
            .dims
            .map(|(w, h)| format!("{w}×{h} px"))
            .unwrap_or_default();
        v_flex().size_full().p_2().gap_2().child(body).child(
            div()
                .text_size(px(12.))
                .text_color(theme::FAINT)
                .child(format!(
                    "{} bytes · {} · {dims}",
                    self.size,
                    fmt_size(self.size)
                )),
        )
    }
}

/// SVG viewer: rendered through gpui's `img` file loader, which rasterizes
/// SVG natively; scaled to the pane size rather than a native size.
fn svg_view(ctx: &ViewCtx<'_>, _: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| SvgView {
        path: ctx.path.to_owned(),
        rel: ctx.rel.to_owned().into(),
        size: ctx.size,
    })
    .into()
}

struct SvgView {
    path: PathBuf,
    rel: SharedString,
    size: u64,
}

impl Render for SvgView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_2()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(img(self.path.clone()).object_fit(ObjectFit::ScaleDown)),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::FAINT)
                    .child(format!("{} bytes · {}", self.size, fmt_size(self.size))),
            )
    }
}

// ------------------------------------------------------- syntax highlighting

/// syntect syntax set: the default Sublime syntaxes from the built-in dump,
/// loaded once per process (`default-fancy` build — pure-Rust regex engine).
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Scope selector → (palette color, italic): the studio look for standard
/// syntect scopes — violet accent keywords, muted italic comments, the
/// JsonView hues for strings, numbers, language constants and names.
const SCOPE_STYLES: &[(&str, Hsla, bool)] = &[
    ("comment", theme::MUTED, true),
    ("keyword, storage", theme::ACCENT, false),
    ("string", theme::J_STRING, false),
    ("constant.numeric", theme::J_NUMBER, false),
    (
        "constant.language, constant.character.escape",
        theme::J_BOOL,
        false,
    ),
    (
        "entity.name, support.function, variable.function, support.type, support.class",
        theme::J_KEY,
        false,
    ),
];

/// [`SCOPE_STYLES`] as a syntect theme, so [`HighlightLines`] scores scope
/// selectors exactly like a Sublime color scheme would. Base foreground is
/// the plain text color, so unmatched code looks unchanged.
static STUDIO_THEME: LazyLock<SyntectTheme> = LazyLock::new(|| SyntectTheme {
    name: Some("deerpunk studio".into()),
    settings: ThemeSettings {
        foreground: Some(syntect_color(theme::TEXT)),
        ..Default::default()
    },
    scopes: SCOPE_STYLES
        .iter()
        .map(|&(selectors, color, italic)| ThemeItem {
            scope: selectors.parse().expect("valid scope selector"),
            style: StyleModifier {
                foreground: Some(syntect_color(color)),
                background: None,
                font_style: italic.then_some(syntect::highlighting::FontStyle::ITALIC),
            },
        })
        .collect(),
    ..Default::default()
});

/// Extensions missing from the default syntax set, highlighted with the
/// closest relative it has (game-dev sources: shaders, RCSS, Luau, TOML).
const EXT_ALIASES: &[(&str, &str)] = &[
    ("toml", "yaml"), // INI-like: YAML colors keys, strings, numbers, comments
    ("rcss", "css"),
    ("glsl", "cpp"),
    ("vert", "cpp"),
    ("frag", "cpp"),
    ("geom", "cpp"),
    ("comp", "cpp"),
    ("luau", "lua"),
];

/// Head fully highlighted once at open: text plus per-range styling, cached
/// in the view entity so re-renders cost nothing.
struct Highlighted {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
}

/// Line-by-line syntect highlighting of the head. `None` when the language
/// is unknown (extension not in the default syntax set, after aliasing) —
/// the caller falls back to plain monospace text. A line the highlighter
/// chokes on keeps the rest plain instead of failing the whole view.
fn highlight_head(text: &str, ext: &str) -> Option<Highlighted> {
    let ext = EXT_ALIASES
        .iter()
        .find(|(from, _)| *from == ext)
        .map_or(ext, |&(_, to)| to);
    let syntax = SYNTAXES.find_syntax_by_extension(ext)?;
    let mut lines = HighlightLines::new(syntax, &STUDIO_THEME);

    let mut out = Highlighted {
        text: String::new(),
        highlights: Vec::new(),
    };
    let mut rest = text.split_inclusive('\n');
    loop {
        let Some(line) = rest.next() else { break };
        let Ok(runs) = lines.highlight_line(line, &SYNTAXES) else {
            // Pathological line (fancy-regex failure): it and everything
            // after stays unstyled.
            out.text.push_str(line);
            for line in rest {
                out.text.push_str(line);
            }
            break;
        };
        for (style, chunk) in runs {
            let hl = HighlightStyle {
                color: Some(hsl((
                    style.foreground.r,
                    style.foreground.g,
                    style.foreground.b,
                ))),
                font_style: syntect::highlighting::FontStyle::ITALIC
                    .intersects(style.font_style)
                    .then_some(FontStyle::Italic),
                ..Default::default()
            };
            let start = out.text.len();
            out.text.push_str(chunk);
            let end = out.text.len();
            if end == start {
                continue;
            }
            // Merge adjacent runs with identical styling.
            match out.highlights.last_mut() {
                Some((range, last)) if range.end == start && *last == hl => range.end = end,
                _ => out.highlights.push((start..end, hl)),
            }
        }
    }
    Some(out)
}

/// Palette color → opaque syntect `Color`.
fn syntect_color(c: Hsla) -> SyntectColor {
    let rgb = c.to_rgb();
    let u = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    SyntectColor {
        r: u(rgb.r),
        g: u(rgb.g),
        b: u(rgb.b),
        a: 255,
    }
}

/// syntect foreground → gpui color (opaque sRGB).
fn hsl((r, g, b): (u8, u8, u8)) -> Hsla {
    rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into()
}

/// Monospace text viewer showing the sniffed head in a scrollable pane,
/// syntax-highlighted when the extension matches a known language.
fn text_view(ctx: &ViewCtx<'_>, _: &mut Window, cx: &mut App) -> AnyView {
    let text = ctx.head_text().into_owned();
    // Highlight once at open — the head never changes afterwards.
    let highlighted = highlight_head(&text, &ctx.ext());
    cx.new(|_| TextView {
        content: text.into(),
        highlighted,
        truncated: ctx.head.len() >= HEAD_CAP,
    })
    .into()
}

struct TextView {
    content: SharedString,
    /// Cached syntax runs; `None` → plain monospace fallback.
    highlighted: Option<Highlighted>,
    truncated: bool,
}

impl Render for TextView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut pane = v_flex().size_full();
        if self.truncated {
            pane = pane.child(
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(12.))
                    .text_color(theme::YELLOW)
                    .child(format!(
                        "head truncated to {} — content beyond this point is not shown",
                        fmt_size(HEAD_CAP as u64)
                    )),
            );
        }
        let body = match &self.highlighted {
            Some(highlighted) => StyledText::new(highlighted.text.clone())
                .with_highlights(highlighted.highlights.clone())
                .into_any_element(),
            None => self.content.clone().into_any_element(),
        };
        pane.child(
            div()
                .id("text-view-scroll")
                .flex_1()
                .min_h_0()
                .overflow_scroll()
                .child(
                    div()
                        .p_2()
                        .whitespace_nowrap()
                        .font_family(mono(cx))
                        .text_size(mono_px(cx))
                        .text_color(theme::TEXT)
                        .child(body),
                ),
        )
    }
}

/// Info card for files without a preview. Also the registry's fallback:
/// [`ViewersPanel`] shows it when no spec matches.
fn binary_view(ctx: &ViewCtx<'_>, _: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| BinaryView {
        rel: ctx.rel.to_owned().into(),
        abs: ctx.path.display().to_string(),
        size: ctx.size,
        kind: ctx.kind.map(|k| k.label()),
    })
    .into()
}

struct BinaryView {
    rel: SharedString,
    abs: String,
    size: u64,
    kind: Option<&'static str>,
}

impl Render for BinaryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut card = v_flex().gap_1().p_2();
        card = card
            .bg(theme::CARD)
            .border_1()
            .border_color(theme::BORDER)
            .rounded(px(8.))
            .child(
                h_flex().items_center().gap_2().child(
                    Icon::new(IconName::File)
                        .text_color(theme::ACCENT)
                        .size(px(14.)),
                ),
            )
            .child(
                div()
                    .font_family(mono(cx))
                    .text_color(theme::TEXT)
                    .child(self.rel.clone()),
            )
            .child(kv_row("Absolute path", &self.abs, cx))
            .child(kv_row("Size", &fmt_size(self.size), cx))
            .children(self.kind.map(|k| kv_row("Kind", k, cx)))
            .child(div().text_color(theme::FAINT).child("binary — no preview"));
        v_flex().size_full().p_2().child(card)
    }
}

/// RmlUi structural viewer: summary card, element tree and RCSS/JS blocks,
/// or a raw source fallback for files that are not well-formed XML.
fn rml_view(ctx: &ViewCtx<'_>, _: &mut Window, cx: &mut App) -> AnyView {
    let src = ctx.head_text().into_owned();
    match rml_doc::parse(&src) {
        Err(e) => cx
            .new(|_| RmlSourceView {
                warning: format!("not well-formed XML: {e} — showing source"),
                content: src.into(),
                truncated: ctx.head.len() >= HEAD_CAP,
            })
            .into(),
        Ok(doc) => cx
            .new(|_| RmlDocView::new(doc, ctx.rel.to_owned().into(), ctx.size))
            .into(),
    }
}

/// Chevron indicator for collapsible rows and blocks.
fn chevron(expanded: bool) -> Icon {
    Icon::new(if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .text_color(theme::MUTED)
    .size(px(13.))
}

/// Value of the first attribute named `k` on a parsed RML node.
fn attr<'a>(node: &'a rml_doc::Node, k: &str) -> Option<&'a str> {
    node.attrs
        .iter()
        .find(|(key, _)| key == k)
        .map(|(_, v)| v.as_str())
}

struct RmlSourceView {
    warning: String,
    content: SharedString,
    truncated: bool,
}

impl Render for RmlSourceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut pane = v_flex().size_full().p_2().gap_2().child(
            div()
                .text_size(px(12.))
                .text_color(theme::YELLOW)
                .child(self.warning.clone()),
        );
        if self.truncated {
            pane = pane.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::YELLOW)
                    .child(format!(
                        "head truncated to {} — content beyond this point is not shown",
                        fmt_size(HEAD_CAP as u64)
                    )),
            );
        }
        pane.child(
            div()
                .id("rml-source-scroll")
                .flex_1()
                .min_h_0()
                .overflow_scroll()
                .child(
                    div()
                        .whitespace_nowrap()
                        .font_family(mono(cx))
                        .text_size(mono_px(cx))
                        .text_color(theme::TEXT)
                        .child(self.content.clone()),
                ),
        )
    }
}

struct RmlDocView {
    doc: rml_doc::Doc,
    rel: SharedString,
    size: u64,
    /// Ids of expanded tree nodes and code blocks; elements at depth < 2
    /// start open, like the egui `CollapsingHeader` tree.
    open: HashSet<String>,
}

impl RmlDocView {
    fn new(doc: rml_doc::Doc, rel: SharedString, size: u64) -> Self {
        let mut open = HashSet::new();
        collect_open(&doc.root, "r".to_string(), 0, &mut open);
        Self {
            doc,
            rel,
            size,
            open,
        }
    }

    /// Summary card: file name, title, root element, size, sheet counts.
    fn header_card(&self, cx: &App) -> AnyElement {
        let mut root = self.doc.root.tag.clone();
        if let Some(t) = attr(&self.doc.root, "template") {
            root.push_str(&format!(" template=\"{t}\""));
        }
        let sheets = self.doc.sheets.len();
        let styles = self.doc.styles.len();
        v_flex()
            .gap_1()
            .bg(theme::CARD)
            .border_1()
            .border_color(theme::BORDER)
            .rounded(px(8.))
            .p_2()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(IconName::FileText)
                            .text_color(theme::ACCENT)
                            .size(px(14.)),
                    )
                    .child(
                        div()
                            .font_family(mono(cx))
                            .text_color(theme::TEXT)
                            .child(self.rel.clone()),
                    ),
            )
            .child(kv_row(
                "Title",
                self.doc.title.as_deref().unwrap_or("—"),
                cx,
            ))
            .child(kv_row("Root", &root, cx))
            .child(kv_row(
                "Size",
                &format!("{} bytes · {}", self.size, fmt_size(self.size)),
                cx,
            ))
            .child(kv_row(
                "Styles",
                &format!(
                    "{} style sheet{} · {} inline",
                    sheets,
                    if sheets == 1 { "" } else { "s" },
                    styles
                ),
                cx,
            ))
            .into_any_element()
    }

    /// Recursive element tree; text runs render as small dim mono labels.
    fn render_node(
        &self,
        node: &rml_doc::Node,
        path: &str,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.open.contains(path);
        let mut header = h_flex().items_center().gap_1().pl(px(depth as f32 * 14.));
        if node.children.is_empty() {
            header = header.child(div().w(px(14.)));
        } else {
            header = header.child(chevron(expanded));
        }
        header = header
            .child(
                div()
                    .font_family(mono(cx))
                    .text_color(theme::J_KEY)
                    .child(node.tag.clone()),
            )
            .children(attr(node, "id").map(|id| {
                div()
                    .font_family(mono(cx))
                    .text_color(theme::ACCENT)
                    .child(format!("#{id}"))
            }))
            .children({
                let classes: String = attr(node, "class")
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(|c| format!(".{c}"))
                    .collect();
                (!classes.is_empty()).then(|| {
                    div()
                        .font_family(mono(cx))
                        .text_color(theme::MUTED)
                        .child(classes)
                })
            });

        let mut wrap = v_flex();
        if node.children.is_empty() {
            wrap = wrap.child(header);
        } else {
            let p = path.to_string();
            wrap = wrap.child(
                header
                    .id(ElementId::Name(format!("rml-n-{path}").into()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.open.remove(&p) {
                            this.open.insert(p.clone());
                        }
                        cx.notify();
                    })),
            );
        }
        if expanded {
            let mut kids = v_flex();
            for (i, child) in node.children.iter().enumerate() {
                match child {
                    rml_doc::Child::Elem(n) => {
                        kids =
                            kids.child(self.render_node(n, &format!("{path}.{i}"), depth + 1, cx));
                    }
                    rml_doc::Child::Text(t) => {
                        kids = kids.child(
                            div()
                                .pl(px(depth as f32 * 14. + 14.))
                                .font_family(mono(cx))
                                .text_size(px(12.))
                                .text_color(theme::TEXT_DIM)
                                .child(t.clone()),
                        );
                    }
                }
            }
            wrap = wrap.child(kids);
        }
        wrap.into_any_element()
    }

    /// Collapsible `<style>` / `<script>` block rendered as monospace source.
    fn code_block(
        &self,
        id: &str,
        title: String,
        body: &str,
        color: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open.contains(id);
        let header = h_flex()
            .items_center()
            .gap_1()
            .child(chevron(open))
            .child(div().font_family(mono(cx)).text_color(color).child(title));
        let mut block = v_flex().gap_1().child(
            header
                .id(ElementId::Name(format!("rml-h-{id}").into()))
                .on_click({
                    let key = id.to_string();
                    cx.listener(move |this, _, _, cx| {
                        if !this.open.remove(&key) {
                            this.open.insert(key.clone());
                        }
                        cx.notify();
                    })
                }),
        );
        if open {
            block = block.child(
                div()
                    .id(ElementId::Name(format!("rml-b-{id}").into()))
                    .max_h(px(300.))
                    .overflow_y_scroll()
                    .font_family(mono(cx))
                    .text_size(px(12.))
                    .text_color(color)
                    .whitespace_nowrap()
                    .child(body.to_string()),
            );
        }
        block.into_any_element()
    }
}

/// Pre-open every element at depth < 2 that has children.
fn collect_open(node: &rml_doc::Node, path: String, depth: usize, open: &mut HashSet<String>) {
    if node.children.is_empty() {
        return;
    }
    if depth < 2 {
        open.insert(path.clone());
    }
    for (i, child) in node.children.iter().enumerate() {
        if let rml_doc::Child::Elem(n) = child {
            collect_open(n, format!("{path}.{i}"), depth + 1, open);
        }
    }
}

impl Render for RmlDocView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = v_flex()
            .p_2()
            .gap_2()
            .child(self.header_card(cx))
            .child(self.render_node(&self.doc.root, "r", 0, cx));
        for (i, style) in self.doc.styles.iter().enumerate() {
            content = content.child(self.code_block(
                &format!("rml-style-{i}"),
                format!("style #{}", i + 1),
                style,
                theme::J_STRING,
                cx,
            ));
        }
        for (i, script) in self.doc.scripts.iter().enumerate() {
            content = content.child(self.code_block(
                &format!("rml-script-{i}"),
                format!("script #{}", i + 1),
                script,
                theme::TEXT_DIM,
                cx,
            ));
        }
        div()
            .id("rml-doc-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(content)
    }
}

// ---------------------------------------------------------------- viewer bus

/// Event emitted by [`ViewerBus`]: a request to open this absolute path.
pub struct OpenFile(pub PathBuf);

/// Request bus between the Files panel and the Viewer panel: `open` emits
/// [`OpenFile`]; the viewer panel subscribes and shows the file.
pub struct ViewerBus;

impl ViewerBus {
    pub fn new() -> Self {
        Self
    }

    /// Ask the viewer panel to open `path` (absolute).
    pub fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        cx.emit(OpenFile(path));
    }
}

impl EventEmitter<OpenFile> for ViewerBus {}

// -------------------------------------------------------------- viewer panel

/// File the viewer panel is currently pointed at.
#[derive(Clone)]
struct OpenTarget {
    path: PathBuf,
    rel: SharedString,
}

/// A built viewer view, memoized until the file's mtime changes.
struct BuiltView {
    mtime: Option<SystemTime>,
    view: AnyView,
}

/// Viewer panel (center tab): sniffs the file opened through [`ViewerBus`],
/// resolves a viewer from the registry and shows its view. The panel title
/// is the file's rel path.
pub struct ViewersPanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    bus: Entity<ViewerBus>,
    _subscription: Subscription,
    open: Option<OpenTarget>,
    built: Option<BuiltView>,
}

impl ViewersPanel {
    pub fn new(state: Entity<StudioState>, bus: Entity<ViewerBus>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&bus, |this, _, event: &OpenFile, cx| {
            // `.rml` is routed by the Workspace to the live RmlUi preview
            // panel; the generic viewer tab leaves those files alone.
            if event
                .0
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("rml"))
            {
                return;
            }
            let path = event.0.clone();
            let rel = display_rel(&path, this.state.read(cx));
            this.open = Some(OpenTarget { path, rel });
            this.built = None;
            cx.notify();
        });
        Self {
            focus_handle: cx.focus_handle(),
            state,
            bus,
            _subscription: subscription,
            open: None,
            built: None,
        }
    }

    /// The memoized view, rebuilt when the file changed or a new one opened:
    /// sniff the head, classify via the scan, resolve a viewer and build.
    fn ensure_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyView> {
        let target = self.open.clone()?;
        let mtime = mtime_of(&target.path);
        if let Some(built) = &self.built
            && built.mtime == mtime
        {
            return Some(built.view.clone());
        }

        let sniff = sniff(&target.path);
        let size = std::fs::metadata(&target.path)
            .map(|m| m.len())
            .unwrap_or(0);
        let kind = {
            let state = self.state.read(cx);
            state.scan.as_ref().and_then(|scan| {
                let gamedir = &state.project.as_ref()?.gamedir;
                let rel = target.path.strip_prefix(gamedir).ok()?;
                scan.entries
                    .iter()
                    .find(|e| e.rel.as_path() == rel)
                    .map(|e| e.kind)
            })
        };
        let head = sniff.head;
        let is_binary = sniff.is_binary;
        let ctx = ViewCtx {
            path: &target.path,
            rel: &target.rel,
            head: &head,
            is_binary,
            size,
            kind,
        };
        let viewer = resolve(&ctx).unwrap_or(FALLBACK_VIEWER);
        let view = (viewer.open)(&ctx, window, cx);
        self.built = Some(BuiltView {
            mtime,
            view: view.clone(),
        });
        Some(view)
    }
}

/// Display path of `path` relative to the gamedir, falling back to the
/// file name / full path when the file is not under the gamedir.
fn display_rel(path: &Path, state: &StudioState) -> SharedString {
    let rel = state
        .project
        .as_ref()
        .and_then(|p| path.strip_prefix(&p.gamedir).ok())
        .map(|p| p.display().to_string());
    rel.or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| path.display().to_string())
        .into()
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl EventEmitter<PanelEvent> for ViewersPanel {}

impl Focusable for ViewersPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for ViewersPanel {
    fn panel_name(&self) -> &'static str {
        "ViewersPanel"
    }
}

impl Panel for ViewersPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.open
            .as_ref()
            .map(|t| t.rel.clone())
            .unwrap_or_else(|| "Viewer".into())
    }
}

impl Render for ViewersPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.ensure_view(window, cx) {
            Some(view) => div().size_full().child(view),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::FAINT)
                .child("open a file from the Files panel"),
        }
    }
}

#[cfg(test)]
mod tests {
    // NOT `use super::*`: the `gpui_kit::*` glob in the parent re-exports a
    // `test` proc-macro that shadows the builtin `#[test]` attribute.
    use crate::viewers::{ViewCtx, ViewerSpec};
    use gpui_kit::FontStyle;
    use std::path::{Path, PathBuf};

    fn vc<'a>(path: &'a Path, head: &'a [u8], is_binary: bool) -> ViewCtx<'a> {
        ViewCtx {
            path,
            rel: "file",
            head,
            is_binary,
            size: head.len() as u64,
            kind: None,
        }
    }

    fn p(name: &str) -> PathBuf {
        PathBuf::from(format!("/tmp/file.{name}"))
    }

    /// Ported spec logic: well-formed clauses parse, garbage is rejected,
    /// clauses AND across `/` and OR within `,`, extensions ignore case
    /// and a leading dot, and the empty spec matches nothing.
    #[test]
    fn spec_parse_and_match() {
        let s = ViewerSpec::parse("ext=png,jpg/magic=89504E470D0A1A0A,FFD8FF/bin=0").unwrap();
        assert_eq!(s.exts, ["png", "jpg"]);
        assert_eq!(
            s.magics,
            [
                vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                vec![0xFF, 0xD8, 0xFF]
            ]
        );
        assert_eq!(s.binary, Some(false));

        for bad in [
            "",
            "ext",
            "ext=",
            "foo=1",
            "bin=2",
            "magic=895", // odd hex length
            "magic=ZZ",  // bad hex digit
            "ext=a,,b",  // empty alternative
            "magic=",    // empty value
        ] {
            assert!(ViewerSpec::parse(bad).is_err(), "`{bad}` should not parse");
        }

        // OR within a clause.
        let s = ViewerSpec::parse("ext=toml,json").unwrap();
        assert!(s.matches(&vc(&p("json"), b"{}", false)));
        assert!(!s.matches(&vc(&p("png"), b"{}", false)));
        // AND across clauses.
        let s = ViewerSpec::parse("ext=png/bin=1").unwrap();
        assert!(!s.matches(&vc(&p("png"), b"xx", false)));
        // Magic prefix.
        let s = ViewerSpec::parse("magic=89504E47").unwrap();
        assert!(s.matches(&vc(&p("dat"), &[0x89, 0x50, 0x4E, 0x47, 0x00], true)));
        assert!(!s.matches(&vc(&p("dat"), &[0x00, 0x89, 0x50], true)));
        // Case/dot-insensitive extensions.
        let s = ViewerSpec::parse("ext=PNG").unwrap();
        assert!(s.matches(&vc(&p("png"), b"", false)));
        // Empty spec matches nothing.
        assert!(!ViewerSpec::default().matches(&vc(&p("txt"), b"hi", false)));
    }

    /// Language detection: core extensions resolve against the default
    /// syntax set, game-dev extensions resolve through the alias table,
    /// unknown ones fall back to plain monospace.
    #[test]
    fn syntax_resolution() {
        for ext in [
            "rs", "cpp", "h", "lua", "json", "yaml", "xml", "html", "css", "py", "sh",
        ] {
            assert!(
                super::SYNTAXES.find_syntax_by_extension(ext).is_some(),
                "`{ext}` should resolve in the default syntax set"
            );
        }
        for ext in [
            "toml", "rcss", "glsl", "vert", "frag", "geom", "comp", "luau",
        ] {
            assert!(
                super::highlight_head("# x\n", ext).is_some(),
                "`{ext}` should alias to a known syntax"
            );
        }
        // Not in the default set, no alias → plain mono.
        assert!(super::highlight_head("hello", "rml").is_none());
        assert!(super::highlight_head("hello", "").is_none());
    }

    /// Runs preserve the text byte-for-byte, cover it in order, style
    /// comment/keyword/string/number distinctly and italicize comments.
    #[test]
    fn highlight_head_runs() {
        // `//`, not `#`: `#` is attribute syntax in Rust, not a comment.
        let src = "// hi\nfn main() { let x = 1; \"s\" }\n";
        let out = super::highlight_head(src, "rs").expect("rust syntax");
        assert_eq!(out.text, src);

        let mut pos = 0;
        let mut styles = Vec::new();
        for (range, hl) in &out.highlights {
            assert_eq!(range.start, pos, "runs must cover the text in order");
            pos = range.end;
            if !styles.iter().any(|s| s == hl) {
                styles.push(*hl);
            }
        }
        assert_eq!(pos, out.text.len(), "runs must reach the end");
        assert!(
            styles.len() >= 3,
            "comment/keyword/string/number should style differently"
        );

        let (_, comment) = out
            .highlights
            .iter()
            .find(|(r, _)| r.contains(&0))
            .expect("a run covering the comment");
        assert!(
            matches!(comment.font_style, Some(FontStyle::Italic)),
            "comments should render italic"
        );
    }
}
