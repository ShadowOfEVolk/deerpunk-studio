//! Live-detail view (center pane of the Live zone): the selected entity as
//! keyvalue cards over a collapsible JSON tree, or the raw payload when it
//! is not an entity. Ported from the egui reference (src/panels.rs
//! `detail_panel`, `component_card`, `json_view`, `scalar_value`; src/theme.rs
//! `badge` and the `J_*` syntax colors — their gpui counterparts live in
//! [`crate::theme`]).
//!
//! Rendering is virtualized: the collapse state folds the JSON tree into a
//! flat vector of visible [`Row`]s and a `uniform_list` lays out only the
//! on-screen range — a multi-thousand-property entity costs the same per
//! frame as a small one. Rows have one fixed height; long scalar values
//! truncate with an ellipsis (monospace single line, like the egui
//! original's one-line rows).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui_kit::component::{ActiveTheme, Icon, IconName, h_flex, v_flex};
use gpui_kit::*;
use serde_json::Value;

use deerpunk_studio::protocol::{ComponentSnapshot, EntityGetResult};

use crate::theme;

/// egui theme's `J_PUNCT`: same `#808080` hex as `J_NULL`, and the gpui
/// palette keeps one gray for both.
const J_PUNCT: Hsla = theme::J_NULL;

/// Uniform slot height for every list row — `uniform_list` lays all rows
/// out with one measured height, and a 22px slot comfortably fits the
/// 13px mono line (the egui original was one line per entry too).
const ROW_H: f32 = 22.;
/// Transparent strip inside a card's first/last row slot; stacked over the
/// neighboring card's strip it recreates the old `gap_1` (4px) between
/// component cards.
const CARD_STRIP: f32 = 2.;
/// Left padding of list content (the old container's `p_2`).
const BASE_INDENT: f32 = 8.;
/// Indent per JSON nesting level (the old per-group `pl(14.)`).
const LEVEL_INDENT: f32 = 14.;

/// Which slice of a component card's frame a row carries. The egui `Frame`
/// (CARD fill, 1px border, 8px radius around the whole card) is rebuilt
/// across the row run: edges cap/round the box, middles carry fill and side
/// borders only.
#[derive(Clone, Copy, PartialEq, Debug)]
enum CardEdge {
    /// The card's only row (closed card, or degenerate empty body).
    Single,
    First,
    Middle,
    Last,
}

/// One visible row of the flattened detail tree.
#[derive(Clone)]
struct Row {
    kind: RowKind,
    /// Left padding: `BASE_INDENT` + `LEVEL_INDENT` × nesting level.
    indent: f32,
    /// Card frame membership (`None` outside component cards).
    card: Option<CardEdge>,
}

#[derive(Clone)]
enum RowKind {
    /// Component card title: chevron + slug (egui `component_card` header).
    CardHeader { id: String, slug: String, color: Hsla, open: bool },
    /// Collapsible JSON group: chevron + colored label (egui
    /// `CollapsingHeader`), object keys in `J_KEY`, array indices in gray.
    Group { id: String, label: String, color: Hsla, open: bool },
    /// `key : value` scalar line (egui `kv_row`; array entries drop the
    /// colon). Long values ellipsize on one mono line.
    Kv { key: String, key_color: Hsla, colon: bool, value: Value },
    /// A bare scalar (the raw payload's root, or scalar component data).
    Scalar(Value),
    /// Tag component: badge + muted "tag".
    Tag { slug: String },
    /// Muted note under an unregistered component's card.
    Note,
}

/// Build the Live-zone detail view for `detail`: a scrolling keyvalue table
/// of the entity's components with monospace values and collapsible JSON
/// groups, prefab names resolved through `prefab_map`. Payloads that are not
/// an [`EntityGetResult`] fall back to the raw JSON tree; `Value::Null`
/// renders the centered "Select an item" placeholder (egui parity).
///
/// Each call builds a fresh view (default-open collapse state) — callers
/// should create it when the selection changes and keep the returned view,
/// not call this every frame.
pub fn detail(
    detail: &Value,
    prefab_map: &HashMap<u32, String>,
    window: &mut Window,
    cx: &mut App,
) -> AnyView {
    let _ = window;
    cx.new(|_| LiveDetail::new(detail, prefab_map)).into()
}

pub struct LiveDetail {
    entity: Option<EntityGetResult>,
    /// Display classname: prefab_map by `prefabId`, else the legacy
    /// `prefab` string (egui detail_panel header).
    classname: Option<String>,
    /// The untouched payload for the raw-JSON fallback.
    raw: Value,
    /// Ids of expanded groups, prefilled with the egui defaults:
    /// `default_open(depth < 2)` for JSON groups, every component card open.
    open: HashSet<String>,
}

impl LiveDetail {
    /// The chevron click handler body for card and JSON-group rows: flip
    /// the id's membership in the open set and re-render.
    pub(crate) fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        self.flip_open(id);
        cx.notify();
    }

    /// The context-free half of [`Self::toggle`] — what the unit tests walk.
    fn flip_open(&mut self, id: &str) {
        if !self.open.remove(id) {
            self.open.insert(id.to_owned());
        }
    }

    /// Build the view from a payload with default-open collapse state
    /// (egui depth defaults). Crate-visible: `detail()` wraps it, and the
    /// headless tests render it directly.
    pub(crate) fn new(detail: &Value, prefab_map: &HashMap<u32, String>) -> Self {
        let mut open = HashSet::new();
        // The fallback tree's defaults (egui `json_view(ui, "detail", 0, ..)`).
        collect_open(detail, "detail", 0, &mut open);
        let entity = serde_json::from_value::<EntityGetResult>(detail.clone()).ok();
        if let Some(entity) = &entity {
            for (i, comp) in entity.components.iter().enumerate() {
                // egui component cards: CollapsingHeader `default_open(true)`.
                open.insert(format!("comp_{i}"));
                if let ComponentSnapshot::Data { data, .. } = comp {
                    collect_open(data, &format!("comp_{i}"), 0, &mut open);
                }
            }
        }
        let classname = entity
            .as_ref()
            .and_then(|e| resolve_classname(e.prefab_id, e.prefab.as_deref(), prefab_map));
        Self {
            entity,
            classname,
            raw: detail.clone(),
            open,
        }
    }

    /// The visible rows of the detail tree: cards and open JSON groups
    /// contribute their children, closed ones stop at the chevron — the
    /// payload never becomes elements all at once, `uniform_list` lays out
    /// only the on-screen range.
    fn flatten(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        match &self.entity {
            None => self.flatten_json(&self.raw, "detail", 0, &mut rows),
            Some(entity) => {
                for (i, comp) in entity.components.iter().enumerate() {
                    self.flatten_component(i, comp, &mut rows);
                }
            }
        }
        rows
    }

    /// One component's rows (egui `component_card`): a Tag renders a plain
    /// badge row; Data/Unregistered render a collapsible card whose frame is
    /// stamped onto the row run (`First`/`Middle`…`Last`).
    fn flatten_component(&self, i: usize, comp: &ComponentSnapshot, rows: &mut Vec<Row>) {
        let slug = comp.slug().to_string();
        let (title_color, note, data) = match comp {
            ComponentSnapshot::Tag { slug, .. } => {
                rows.push(Row {
                    kind: RowKind::Tag { slug: slug.clone() },
                    indent: BASE_INDENT,
                    card: None,
                });
                return;
            }
            ComponentSnapshot::Data { data, .. } if is_unregistered(data) => {
                (theme::TEXT_DIM, true, None)
            }
            ComponentSnapshot::Data { data, .. } => (theme::TEXT, false, Some(data)),
            ComponentSnapshot::Unregistered { .. } => (theme::TEXT_DIM, true, None),
        };
        let id = format!("comp_{i}");
        let open = self.open.contains(&id);
        let mut header = Row {
            kind: RowKind::CardHeader {
                id: id.clone(),
                slug,
                color: title_color,
                open,
            },
            indent: BASE_INDENT,
            card: None,
        };
        match (open, note, data) {
            // Unregistered body: a single muted note row.
            (true, true, _) => {
                header.card = Some(CardEdge::First);
                rows.push(header);
                rows.push(Row {
                    kind: RowKind::Note,
                    indent: BASE_INDENT,
                    card: Some(CardEdge::Last),
                });
            }
            (true, false, Some(data)) => {
                header.card = Some(CardEdge::First);
                rows.push(header);
                let mut body = Vec::new();
                self.flatten_json(data, &id, 1, &mut body);
                if body.is_empty() {
                    // Degenerate data (e.g. an empty object): the header
                    // row carries the whole card frame.
                    rows.last_mut().unwrap().card = Some(CardEdge::Single);
                } else {
                    for row in &mut body {
                        row.card = Some(CardEdge::Middle);
                    }
                    body.last_mut().unwrap().card = Some(CardEdge::Last);
                    rows.extend(body);
                }
            }
            // Closed card: the title row carries the whole frame.
            _ => {
                header.card = Some(CardEdge::Single);
                rows.push(header);
            }
        }
    }

    /// Walk one JSON value's *visible* rows (egui `json_view` walk): object
    /// keys in `J_KEY`, array indices in punct gray, containers become
    /// collapsible groups (children only while open), scalars render inline.
    /// `level` drives the indent — the old per-group `pl(14.)` chain.
    fn flatten_json(&self, v: &Value, salt: &str, level: usize, rows: &mut Vec<Row>) {
        let indent = BASE_INDENT + LEVEL_INDENT * level as f32;
        match v {
            Value::Object(map) => {
                for (k, val) in map {
                    match val {
                        Value::Object(_) | Value::Array(_) => {
                            let id = format!("{salt}_{k}");
                            let open = self.open.contains(&id);
                            rows.push(Row {
                                kind: RowKind::Group {
                                    id: id.clone(),
                                    label: k.clone(),
                                    color: theme::J_KEY,
                                    open,
                                },
                                indent,
                                card: None,
                            });
                            if open {
                                self.flatten_json(val, &id, level + 1, rows);
                            }
                        }
                        scalar => rows.push(Row {
                            kind: RowKind::Kv {
                                key: k.clone(),
                                key_color: theme::J_KEY,
                                colon: true,
                                value: scalar.clone(),
                            },
                            indent,
                            card: None,
                        }),
                    }
                }
            }
            Value::Array(items) => {
                for (i, val) in items.iter().enumerate() {
                    match val {
                        Value::Object(_) | Value::Array(_) => {
                            let id = format!("{salt}_a{i}");
                            let open = self.open.contains(&id);
                            rows.push(Row {
                                kind: RowKind::Group {
                                    id: id.clone(),
                                    label: format!("[{i}]"),
                                    color: J_PUNCT,
                                    open,
                                },
                                indent,
                                card: None,
                            });
                            if open {
                                self.flatten_json(val, &id, level + 1, rows);
                            }
                        }
                        // egui array scalars render without the colon.
                        scalar => rows.push(Row {
                            kind: RowKind::Kv {
                                key: format!("[{i}]"),
                                key_color: J_PUNCT,
                                colon: false,
                                value: scalar.clone(),
                            },
                            indent,
                            card: None,
                        }),
                    }
                }
            }
            scalar => rows.push(Row {
                kind: RowKind::Scalar(scalar.clone()),
                indent,
                card: None,
            }),
        }
    }
}

impl Render for LiveDetail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // No selection: the caller passes Value::Null (egui's centered
        // "Select an item" placeholder).
        if self.entity.is_none() && self.raw.is_null() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(dim("Select an item"))
                .into_any_element();
        }
        let view = cx.entity();
        let rows = Arc::new(self.flatten());
        let list = uniform_list("detail-rows", rows.len().max(1), move |range, _, cx| {
            range
                .map(|ix| match rows.get(ix) {
                    Some(row) => render_row(row, &view, cx),
                    // Keeps the measured slot when the list is empty.
                    None => div().h(px(ROW_H)).into_any_element(),
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0()
        .w_full();

        let Some(entity) = self.entity.clone() else {
            // Not an entity payload: the raw JSON tree (egui detail_panel's
            // else branch) in the same virtualized list.
            return v_flex().size_full().p_2().child(list).into_any_element();
        };

        // ● green · mono name · "· prefab" · "#index" · "N components".
        let mut header = h_flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .child(div().text_size(px(10.)).text_color(theme::GREEN).child("●"))
            .child(
                div()
                    .font_family(mono(cx))
                    .text_size(mono_px(cx))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::TEXT)
                    .child(entity.name.clone()),
            );
        if let Some(classname) = &self.classname {
            header = header.child(dim(format!("· {classname}")));
        }
        header = header
            .child(dim(format!("#{}", entity.index)))
            .child(dim(format!("{} components", entity.components.len())));

        v_flex()
            .size_full()
            .child(header)
            .child(div().h(px(1.)).w_full().bg(theme::BORDER))
            .child(list)
            .into_any_element()
    }
}

/// Element for one visible row (only the on-screen range is built). Card
/// edges reconstruct the egui `Frame` across a component's row run: the
/// First slot keeps a transparent strip on top and the Last one at the
/// bottom — stacked they preserve the old `gap_1` (4px) between cards.
fn render_row(row: &Row, view: &Entity<LiveDetail>, cx: &App) -> AnyElement {
    let box_h = match row.card {
        Some(CardEdge::First | CardEdge::Last) => ROW_H - CARD_STRIP,
        _ => ROW_H,
    };
    let mut slot = div().h(px(ROW_H)).w_full().overflow_hidden();
    if row.card == Some(CardEdge::First) {
        slot = slot.pt(px(CARD_STRIP));
    }
    let mut boxed = div()
        .h(px(box_h))
        .w_full()
        .flex()
        .items_center()
        .overflow_hidden();
    if let Some(edge) = row.card {
        boxed = boxed.bg(theme::CARD).border_x_1().border_color(theme::BORDER);
        boxed = match edge {
            CardEdge::Single => boxed.border_1().rounded(px(8.)),
            CardEdge::First => boxed.border_t_1().rounded_t(px(8.)),
            CardEdge::Last => boxed.border_b_1().rounded_b(px(8.)),
            CardEdge::Middle => boxed,
        };
    }
    let line: AnyElement = match &row.kind {
        RowKind::CardHeader { id, slug, color, open } => {
            header_line(row.indent, *open, slug.clone(), *color, id, view, cx)
        }
        RowKind::Group { id, label, color, open } => {
            header_line(row.indent, *open, label.clone(), *color, id, view, cx)
        }
        RowKind::Kv { key, key_color, colon, value } => {
            let mut line = h_flex()
                .items_center()
                .gap_1()
                .h_full()
                .pl(px(row.indent))
                .pr(px(2.))
                .overflow_hidden()
                .child(mono_span(key.clone(), *key_color, cx).flex_none());
            if *colon {
                line = line.child(mono_span(":", J_PUNCT, cx).flex_none());
            }
            // One mono line: long values ellipsize (the egui original drew
            // single-line rows too).
            line.child(
                mono_span(scalar_text(value), scalar_color(value), cx)
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis(),
            )
            .into_any_element()
        }
        RowKind::Scalar(value) => h_flex()
            .items_center()
            .h_full()
            .pl(px(row.indent))
            .pr(px(2.))
            .overflow_hidden()
            .child(mono_span(scalar_text(value), scalar_color(value), cx))
            .into_any_element(),
        RowKind::Tag { slug } => h_flex()
            .items_center()
            .gap_2()
            .h_full()
            .pl(px(row.indent))
            .overflow_hidden()
            .child(badge(slug, cx))
            .child(dim("tag"))
            .into_any_element(),
        RowKind::Note => h_flex()
            .items_center()
            .h_full()
            .pl(px(row.indent))
            .overflow_hidden()
            .child(dim("no properties registered"))
            .into_any_element(),
    };
    slot.child(boxed.child(line)).into_any_element()
}

/// Chevron + mono label line; clicking toggles `id` through the view (the
/// card/group header shape, egui `CollapsingHeader`).
fn header_line(
    indent: f32,
    open: bool,
    label: String,
    color: Hsla,
    id: &str,
    view: &Entity<LiveDetail>,
    cx: &App,
) -> AnyElement {
    h_flex()
        .items_center()
        .gap_1()
        .h_full()
        .pl(px(indent))
        .pr(px(2.))
        .overflow_hidden()
        .child(chevron(open))
        .child(mono_span(label, color, cx).flex_none())
        .id(ElementId::Name(SharedString::from(id.to_owned())))
        .cursor_pointer()
        .on_click({
            let view = view.clone();
            let id = id.to_owned();
            move |_, _, cx| view.update(cx, |this, cx| this.toggle(&id, cx))
        })
        .into_any_element()
}

/// Display classname: prefabMap classname for `prefabId`, else the legacy
/// string `prefab` (egui detail_panel header resolution).
fn resolve_classname(
    prefab_id: Option<u32>,
    prefab: Option<&str>,
    map: &HashMap<u32, String>,
) -> Option<String> {
    prefab_id
        .and_then(|id| map.get(&id).cloned())
        .or_else(|| prefab.map(str::to_string))
}

/// True when the engine marked a component as having no registered inspect
/// properties (`{"_unregistered": true}` as the whole `data` object).
/// Ported from egui `is_unregistered`.
fn is_unregistered(data: &Value) -> bool {
    data.as_object()
        .is_some_and(|o| o.len() == 1 && o.get("_unregistered") == Some(&Value::Bool(true)))
}

/// Prefill [`LiveDetail::open`] the egui way: a JSON group renders expanded
/// while the depth it sits at is < 2 (`default_open(depth < 2)`), regardless
/// of whether its ancestors start closed — egui evaluates the default when
/// the group first becomes visible. Group ids follow the egui `id_salt`
/// scheme: `{salt}_{key}` under objects, `{salt}_a{i}` under arrays.
fn collect_open(v: &Value, salt: &str, depth: usize, open: &mut HashSet<String>) {
    let children: Vec<(String, &Value)> = match v {
        Value::Object(map) => map
            .iter()
            .map(|(k, val)| (format!("{salt}_{k}"), val))
            .collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, val)| (format!("{salt}_a{i}"), val))
            .collect(),
        _ => return,
    };
    for (id, child) in children {
        if matches!(child, Value::Object(_) | Value::Array(_)) {
            if depth < 2 {
                open.insert(id.clone());
            }
            collect_open(child, &id, depth + 1, open);
        }
    }
}

/// Scalar value text (egui `scalar_value`): strings quoted, numbers/bools
/// verbatim, `null`, anything else through serde's rendering.
fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => format!("\"{s}\""),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        _ => v.to_string(),
    }
}

/// Scalar value color by JSON type (egui `scalar_value`).
fn scalar_color(v: &Value) -> Hsla {
    match v {
        Value::String(_) => theme::J_STRING,
        Value::Number(_) => theme::J_NUMBER,
        Value::Bool(_) => theme::J_BOOL,
        Value::Null => theme::J_NULL,
        _ => J_PUNCT,
    }
}

fn chevron(expanded: bool) -> Icon {
    Icon::new(if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .text_color(theme::MUTED)
    .size(px(13.))
}

fn mono(cx: &App) -> SharedString {
    cx.theme().mono_font_family.clone()
}

fn mono_px(cx: &App) -> Pixels {
    cx.theme().mono_font_size
}

fn mono_span(text: impl Into<SharedString>, color: Hsla, cx: &App) -> Div {
    div()
        .font_family(mono(cx))
        .text_size(mono_px(cx))
        .text_color(color)
        .child(text.into())
}

/// Small dim text (egui `ui.weak`).
fn dim(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(theme::TEXT_DIM)
        .child(text.into())
}

/// Small pill badge: accent-tinted fill, mono label in the accent color
/// (egui `theme::badge`, alpha `0x22 ≈ 0.13`).
fn badge(text: &str, cx: &App) -> Div {
    div()
        .font_family(mono(cx))
        .text_size(mono_px(cx))
        .text_color(theme::ACCENT)
        .bg(theme::ACCENT.alpha(0.13))
        .rounded(px(2.))
        .px(px(6.))
        .py(px(2.))
        .child(text.to_string())
}

#[cfg(test)]
mod tests {
    // All `#[test]`s below are spelled `#[::core::prelude::v1::test]`: the
    // file's `use gpui_kit::*` glob (test-support on) re-exports gpui's
    // `test` proc macro, which would otherwise shadow the builtin and fail
    // macro expansion (same reason tests/ui.rs imports Kit types
    // explicitly).
    use std::collections::{HashMap, HashSet};

    use gpui_kit::Hsla;
    use serde_json::{Value, json};

    use super::{
        CardEdge, LiveDetail, Row, RowKind, collect_open, is_unregistered, resolve_classname,
        scalar_color, scalar_text,
    };
    use crate::theme;

    /// Hsla carries no `PartialEq`; channel-wise equality is exact for
    /// compile-time constants.
    fn same(a: Hsla, b: Hsla) -> bool {
        a.h == b.h && a.s == b.s && a.l == b.l && a.a == b.a
    }

    #[::core::prelude::v1::test]
    fn scalar_text_renders_json_scalars() {
        assert_eq!(scalar_text(&json!("hello")), "\"hello\"");
        assert_eq!(scalar_text(&json!(42)), "42");
        assert_eq!(scalar_text(&json!(1.5)), "1.5");
        assert_eq!(scalar_text(&json!(true)), "true");
        assert_eq!(scalar_text(&json!(false)), "false");
        assert_eq!(scalar_text(&json!(null)), "null");
    }

    #[::core::prelude::v1::test]
    fn scalar_color_follows_json_type() {
        assert!(same(scalar_color(&json!("x")), theme::J_STRING));
        assert!(same(scalar_color(&json!(1)), theme::J_NUMBER));
        assert!(same(scalar_color(&json!(true)), theme::J_BOOL));
        assert!(same(scalar_color(&json!(null)), theme::J_NULL));
        // The catch-all (egui J_PUNCT): non-scalars that slip through.
        assert!(same(scalar_color(&json!({"a": 1})), theme::J_NULL));
    }

    #[::core::prelude::v1::test]
    fn is_unregistered_needs_exact_marker() {
        assert!(is_unregistered(&json!({"_unregistered": true})));
        assert!(!is_unregistered(&json!({"_unregistered": false})));
        assert!(!is_unregistered(&json!({"_unregistered": true, "hp": 1})));
        assert!(!is_unregistered(&json!({"hp": 1})));
    }

    #[::core::prelude::v1::test]
    fn resolve_classname_prefers_prefab_map() {
        let map: HashMap<u32, String> = [(7u32, "player".to_string())].into_iter().collect();
        // Map hit wins over the legacy string.
        assert_eq!(
            resolve_classname(Some(7), Some("npc"), &map).as_deref(),
            Some("player")
        );
        // Unknown id falls back to the legacy string.
        assert_eq!(
            resolve_classname(Some(9), Some("npc"), &map).as_deref(),
            Some("npc")
        );
        // Neither → none.
        assert_eq!(resolve_classname(None, None, &map), None);
        assert_eq!(resolve_classname(Some(9), None, &map), None);
    }

    #[::core::prelude::v1::test]
    fn collect_open_matches_egui_depth_defaults() {
        let v = json!({
            "a": {
                "b": {
                    "c": { "d": 1 }
                }
            },
            "arr": [
                [1, 2]
            ],
            "s": "x"
        });
        let mut open = HashSet::new();
        collect_open(&v, "detail", 0, &mut open);
        // Groups at depth 0 and 1 default open…
        for id in ["detail_a", "detail_arr", "detail_a_b", "detail_arr_a0"] {
            assert!(open.contains(id), "missing default-open id {id}");
        }
        // …depth 2 stays closed (egui `default_open(depth < 2)`).
        assert!(!open.contains("detail_a_b_c"));
        // Scalars never become group ids.
        assert_eq!(open.len(), 4);
    }

    #[::core::prelude::v1::test]
    fn new_prefills_component_cards() {
        let v = json!({
            "index": 3,
            "name": "chicken",
            "prefabId": 7,
            "components": [
                { "slug": "health", "data": { "hp": 10, "flags": { "regen": true } } },
                { "slug": "mover", "_unregistered": true }
            ]
        });
        let map: HashMap<u32, String> = [(7u32, "player".to_string())].into_iter().collect();
        let view = LiveDetail::new(&v, &map);
        let entity = view.entity.expect("entity payload must parse");
        assert_eq!(entity.name, "chicken");
        assert_eq!(entity.components.len(), 2);
        // Header classname through the prefab map.
        assert_eq!(view.classname.as_deref(), Some("player"));
        // Every card starts open; card JSON groups follow depth defaults.
        assert!(view.open.contains("comp_0"));
        assert!(view.open.contains("comp_1"));
        assert!(view.open.contains("comp_0_flags"));
        assert!(!view.open.contains("comp_0_flags_regen"));
        // The fallback tree salts are prefilled too.
        assert!(view.open.contains("detail_components"));
    }

    fn kind_name(row: &Row) -> &'static str {
        match &row.kind {
            RowKind::CardHeader { .. } => "card",
            RowKind::Group { .. } => "group",
            RowKind::Kv { .. } => "kv",
            RowKind::Scalar(_) => "scalar",
            RowKind::Tag { .. } => "tag",
            RowKind::Note => "note",
        }
    }

    /// The flattened visible rows follow the collapse state: a closed card
    /// or group removes exactly its descendants, reopening restores them.
    #[::core::prelude::v1::test]
    fn flatten_visible_rows_follow_collapse_state() {
        let v = json!({
            "index": 3,
            "name": "chicken",
            "prefabId": 7,
            "components": [
                { "slug": "health", "data": { "hp": 10, "flags": { "regen": true } } },
                { "slug": "fx", "kind": "tag" }
            ]
        });
        let mut view = LiveDetail::new(&v, &HashMap::new());

        // Default (depth < 2 open): health card = header + hp + flags group
        // + regen kv, then the tag row.
        let rows = view.flatten();
        let kinds: Vec<&str> = rows.iter().map(kind_name).collect();
        assert_eq!(
            kinds,
            vec!["card", "kv", "group", "kv", "tag"],
            "unexpected default row kinds: {kinds:?}"
        );
        // Card frame stamps run First..Last over the card's rows, the tag
        // row carries no frame.
        assert_eq!(rows[0].card, Some(CardEdge::First));
        assert_eq!(rows[3].card, Some(CardEdge::Last));
        assert_eq!(rows[4].card, None);
        // Body rows sit one level in (the old pl(14) under the card box).
        assert!((rows[1].indent - (8. + 14.)).abs() < f32::EPSILON);

        // Chevron click on the card title (comp_0): body rows drop out, the
        // header row carries the whole frame.
        view.flip_open("comp_0");
        let rows = view.flatten();
        assert_eq!(rows.len(), 2);
        assert_eq!(kind_name(&rows[0]), "card");
        assert_eq!(rows[0].card, Some(CardEdge::Single));
        assert_eq!(kind_name(&rows[1]), "tag");

        // Reopen restores the same rows.
        view.flip_open("comp_0");
        assert_eq!(view.flatten().len(), 5);

        // Chevron click on a nested group hides only its children.
        view.flip_open("comp_0_flags");
        let rows = view.flatten();
        let kinds: Vec<&str> = rows.iter().map(kind_name).collect();
        assert_eq!(kinds, vec!["card", "kv", "group", "tag"]);
    }

    /// Non-entity payloads flatten as the raw tree (egui `json_view` "detail"
    /// salts): object keys keep their kv shape, the root scalar renders bare.
    #[::core::prelude::v1::test]
    fn flatten_raw_fallback_tree() {
        let object = LiveDetail::new(&json!({"a": 1, "list": [true, "s"]}), &HashMap::new());
        let kinds: Vec<&str> = object.flatten().iter().map(kind_name).collect();
        assert_eq!(kinds, vec!["kv", "group", "kv", "kv"]);

        let scalar = LiveDetail::new(&json!("bare"), &HashMap::new());
        let rows = scalar.flatten();
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0].kind, RowKind::Scalar(Value::String(s)) if s == "bare"));
    }
}
