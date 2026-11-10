//! Live-detail view (center pane of the Live zone): the selected entity as
//! keyvalue cards over a collapsible JSON tree, or the raw payload when it
//! is not an entity. Ported from the egui reference (src/panels.rs
//! `detail_panel`, `component_card`, `json_view`, `scalar_value`; src/theme.rs
//! `badge` and the `J_*` syntax colors — their gpui counterparts live in
//! [`crate::theme`]).

use std::collections::{HashMap, HashSet};

use gpui_kit::component::{ActiveTheme, Icon, IconName, h_flex, v_flex};
use gpui_kit::*;
use serde_json::Value;

use deerpunk_studio::protocol::{ComponentSnapshot, EntityGetResult};

use crate::theme;

/// egui theme's `J_PUNCT`: same `#808080` hex as `J_NULL`, and the gpui
/// palette keeps one gray for both.
const J_PUNCT: Hsla = theme::J_NULL;

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
    fn new(detail: &Value, prefab_map: &HashMap<u32, String>) -> Self {
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

    /// Recursive JSON renderer with inspector syntax colors — the egui
    /// `json_view` walk: object keys in `J_KEY`, array indices in punct gray,
    /// container values become collapsible groups, scalars render inline.
    fn json_view(
        &self,
        id_salt: &str,
        depth: usize,
        v: &Value,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut out = v_flex().gap_1();
        match v {
            Value::Object(map) => {
                for (k, val) in map {
                    match val {
                        Value::Object(_) | Value::Array(_) => {
                            out = out.child(self.group(
                                &format!("{id_salt}_{k}"),
                                depth,
                                k.clone(),
                                theme::J_KEY,
                                val,
                                cx,
                            ))
                        }
                        scalar => {
                            out = out.child(kv_row((k.clone(), theme::J_KEY), true, scalar, cx))
                        }
                    }
                }
            }
            Value::Array(items) => {
                for (i, val) in items.iter().enumerate() {
                    match val {
                        Value::Object(_) | Value::Array(_) => {
                            out = out.child(self.group(
                                &format!("{id_salt}_a{i}"),
                                depth,
                                format!("[{i}]"),
                                J_PUNCT,
                                val,
                                cx,
                            ))
                        }
                        // egui array scalars render without the colon.
                        scalar => {
                            out = out.child(kv_row((format!("[{i}]"), J_PUNCT), false, scalar, cx))
                        }
                    }
                }
            }
            scalar => out = out.child(scalar_span(scalar, cx)),
        }
        out.into_any_element()
    }

    /// One collapsible container entry: chevron + colored label, children
    /// indented while expanded (egui `CollapsingHeader` shape).
    fn group(
        &self,
        id: &str,
        depth: usize,
        label: String,
        color: Hsla,
        val: &Value,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open.contains(id);
        let header = h_flex()
            .items_center()
            .gap_1()
            .child(chevron(open))
            .child(mono_span(label, color, cx));
        let mut wrap = v_flex().child(
            header
                .id(ElementId::Name(SharedString::from(id.to_owned())))
                .cursor_pointer()
                .on_click(cx.listener({
                    let id = id.to_owned();
                    move |this, _, _, cx| {
                        if !this.open.remove(&id) {
                            this.open.insert(id.clone());
                        }
                        cx.notify();
                    }
                })),
        );
        if open {
            wrap = wrap.child(
                v_flex()
                    .pl(px(14.))
                    .child(self.json_view(id, depth + 1, val, cx)),
            );
        }
        wrap.into_any_element()
    }

    /// Component card (egui `component_card`): collapsible slug title over
    /// the component's JSON tree; unregistered components render muted.
    fn component_card(
        &self,
        i: usize,
        comp: &ComponentSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match comp {
            ComponentSnapshot::Tag { slug, .. } => h_flex()
                .items_center()
                .gap_2()
                .child(badge(slug, cx))
                .child(dim("tag"))
                .into_any_element(),
            ComponentSnapshot::Data { slug, data, .. } if is_unregistered(data) => self.card(
                i,
                slug,
                theme::TEXT_DIM,
                dim("no properties registered"),
                cx,
            ),
            ComponentSnapshot::Data { slug, data, .. } => {
                let id = format!("comp_{i}");
                self.card(i, slug, theme::TEXT, self.json_view(&id, 0, data, cx), cx)
            }
            ComponentSnapshot::Unregistered { .. } => self.card(
                i,
                comp.slug(),
                theme::TEXT_DIM,
                dim("no properties registered"),
                cx,
            ),
        }
    }

    /// The card frame: `CARD` fill, 1px border, 8px radius, inner margin 8
    /// (egui `Frame` around `component_card` bodies).
    fn card(
        &self,
        i: usize,
        slug: &str,
        title_color: Hsla,
        body: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = format!("comp_{i}");
        let open = self.open.contains(&id);
        let mut card = v_flex()
            .gap_1()
            .bg(theme::CARD)
            .border_1()
            .border_color(theme::BORDER)
            .rounded(px(8.))
            .p_2()
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(chevron(open))
                    .child(mono_span(slug.to_string(), title_color, cx))
                    .id(ElementId::Name(SharedString::from(id.clone())))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.open.remove(&id) {
                            this.open.insert(id.clone());
                        }
                        cx.notify();
                    })),
            );
        if open {
            card = card.child(body);
        }
        card.into_any_element()
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
        let Some(entity) = self.entity.clone() else {
            // Not an entity payload: the raw JSON tree (egui detail_panel's
            // else branch).
            return div()
                .id("live-detail-scroll")
                .size_full()
                .overflow_y_scroll()
                .child(
                    v_flex()
                        .p_2()
                        .gap_1()
                        .child(self.json_view("detail", 0, &self.raw, cx)),
                )
                .into_any_element();
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

        let mut cards = v_flex().p_2().gap_1();
        for (i, comp) in entity.components.iter().enumerate() {
            cards = cards.child(self.component_card(i, comp, cx));
        }

        v_flex()
            .size_full()
            .child(header)
            .child(div().h(px(1.)).w_full().bg(theme::BORDER))
            .child(
                div()
                    .id("live-detail-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(cards),
            )
            .into_any_element()
    }
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

/// `key [:] value` line for a scalar entry, all monospace.
fn kv_row(key: (String, Hsla), colon: bool, value: &Value, cx: &App) -> AnyElement {
    let mut row = h_flex().gap_1().child(mono_span(key.0, key.1, cx));
    if colon {
        row = row.child(mono_span(":", J_PUNCT, cx));
    }
    row.child(scalar_span(value, cx)).into_any_element()
}

fn scalar_span(v: &Value, cx: &App) -> AnyElement {
    mono_span(scalar_text(v), scalar_color(v), cx).into_any_element()
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
    use std::collections::{HashMap, HashSet};

    use gpui_kit::Hsla;
    use serde_json::json;

    use super::{
        LiveDetail, collect_open, is_unregistered, resolve_classname, scalar_color, scalar_text,
    };
    use crate::theme;

    /// Hsla carries no `PartialEq`; channel-wise equality is exact for
    /// compile-time constants.
    fn same(a: Hsla, b: Hsla) -> bool {
        a.h == b.h && a.s == b.s && a.l == b.l && a.a == b.a
    }

    #[test]
    fn scalar_text_renders_json_scalars() {
        assert_eq!(scalar_text(&json!("hello")), "\"hello\"");
        assert_eq!(scalar_text(&json!(42)), "42");
        assert_eq!(scalar_text(&json!(1.5)), "1.5");
        assert_eq!(scalar_text(&json!(true)), "true");
        assert_eq!(scalar_text(&json!(false)), "false");
        assert_eq!(scalar_text(&json!(null)), "null");
    }

    #[test]
    fn scalar_color_follows_json_type() {
        assert!(same(scalar_color(&json!("x")), theme::J_STRING));
        assert!(same(scalar_color(&json!(1)), theme::J_NUMBER));
        assert!(same(scalar_color(&json!(true)), theme::J_BOOL));
        assert!(same(scalar_color(&json!(null)), theme::J_NULL));
        // The catch-all (egui J_PUNCT): non-scalars that slip through.
        assert!(same(scalar_color(&json!({"a": 1})), theme::J_NULL));
    }

    #[test]
    fn is_unregistered_needs_exact_marker() {
        assert!(is_unregistered(&json!({"_unregistered": true})));
        assert!(!is_unregistered(&json!({"_unregistered": false})));
        assert!(!is_unregistered(&json!({"_unregistered": true, "hp": 1})));
        assert!(!is_unregistered(&json!({"hp": 1})));
    }

    #[test]
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

    #[test]
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

    #[test]
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
}
