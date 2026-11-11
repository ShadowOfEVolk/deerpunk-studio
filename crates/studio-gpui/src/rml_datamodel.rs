//! DataModel map for the Rmlui panel: which model fields the open `.rml`
//! document consumes (inputs) and renders (outputs), classified from the
//! parsed tree ([`crate::rml`]). Pure logic — no UI: the panel renders the
//! map, tests assert the classification.
//!
//! Classification:
//! - **inputs** — `data-event-*` attributes (event name → handler field,
//!   the engine's Lua hook) and `data-for` (iteration over a collection);
//! - **outputs** — `{{field}}` mustaches in element attribute values
//!   (bound classes, styles, attributes) and in text runs.
//!
//! Live field *values* are not available (RmlUi does not enumerate a data
//! model outward, and the studio injects no `--state` model) — this is the
//! structural map only.

use std::collections::BTreeMap;

use crate::rml::{Child, Doc, Node};

/// One binding endpoint. `field` is the model field (or the raw `data-for`
/// iteration expression), `element` the owning element's label
/// (`tag#id.class`), `detail` the usage context (event name, `for`,
/// attribute name, or `text`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub field: String,
    pub element: String,
    pub detail: String,
}

/// Combined per-field usage across the whole document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldUse {
    pub field: String,
    pub input: bool,
    pub output: bool,
}

/// Static binding map of one document, deterministic order (document order
/// for bindings, alphabetical for the field rollup).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DataModel {
    pub inputs: Vec<Binding>,
    pub outputs: Vec<Binding>,
    pub fields: Vec<FieldUse>,
}

impl DataModel {
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.outputs.is_empty()
    }
}

/// Extract the binding map from a parsed document.
pub fn extract(doc: &Doc) -> DataModel {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    walk(&doc.root, &mut inputs, &mut outputs);

    let mut fields: BTreeMap<String, FieldUse> = BTreeMap::new();
    for b in &inputs {
        fields
            .entry(b.field.clone())
            .or_insert_with(|| FieldUse {
                field: b.field.clone(),
                input: false,
                output: false,
            })
            .input = true;
    }
    for b in &outputs {
        fields
            .entry(b.field.clone())
            .or_insert_with(|| FieldUse {
                field: b.field.clone(),
                input: false,
                output: false,
            })
            .output = true;
    }
    DataModel {
        inputs,
        outputs,
        fields: fields.into_values().collect(),
    }
}

/// Classify one element's attributes and walk its subtree.
fn walk(node: &Node, inputs: &mut Vec<Binding>, outputs: &mut Vec<Binding>) {
    let label = label(node);
    for (key, value) in &node.attrs {
        if let Some(event) = key.strip_prefix("data-event-") {
            inputs.push(Binding {
                field: value.clone(),
                element: label.clone(),
                detail: event.to_string(),
            });
        } else if key == "data-for" {
            inputs.push(Binding {
                field: value.clone(),
                element: label.clone(),
                detail: "for".into(),
            });
        } else if let Some(fields) = bound_fields(value) {
            for field in fields {
                outputs.push(Binding {
                    field,
                    element: label.clone(),
                    detail: key.clone(),
                });
            }
        }
    }
    for child in &node.children {
        match child {
            Child::Elem(nested) => walk(nested, inputs, outputs),
            Child::Text(text) => {
                for field in bound_fields(text).unwrap_or_default() {
                    outputs.push(Binding {
                        field,
                        element: label.clone(),
                        detail: "text".into(),
                    });
                }
            }
        }
    }
}

/// `tag`, plus `#id` and the first class when present — `button#play.btn`.
fn label(node: &Node) -> String {
    let mut out = node.tag.clone();
    if let Some((_, id)) = node.attrs.iter().find(|(k, _)| k == "id") {
        out.push('#');
        out.push_str(id);
    }
    if let Some((_, classes)) = node.attrs.iter().find(|(k, _)| k == "class") {
        if let Some(first) = classes.split_whitespace().next() {
            out.push('.');
            out.push_str(first);
        }
    }
    out
}

/// Fields referenced by `{{ … }}` mustaches in `s`, in order, deduped;
/// `None` when the text binds nothing.
fn bound_fields(s: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("{{") {
        let after = &rest[i + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        let field = after[..end].trim();
        if !field.is_empty() && !out.iter().any(|f| f == field) {
            out.push(field.to_string());
        }
        rest = &after[end + 2..];
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rml;

    fn model(src: &str) -> DataModel {
        extract(&rml::parse(src).expect("fixture parses"))
    }

    #[test]
    fn classifies_inputs_outputs_and_fields() {
        let m = model(
            r#"<rml><head><title>Hud</title></head>
            <body data-model="hud">
              <div id="root" class="hud">
                <button id="play" class="btn" data-event-click="on_play" data-event-mouseover="on_hover">{{label}}</button>
                <div data-for="item in items">{{item.name}}</div>
                <span data-class-active="{{flag}}">count {{count}} of {{count}}</span>
                <img src="x.png"/>
              </div>
            </body></rml>"#,
        );

        // Inputs: two event hooks + one iteration, document order.
        assert_eq!(
            m.inputs,
            vec![
                Binding {
                    field: "on_play".into(),
                    element: "button#play.btn".into(),
                    detail: "click".into()
                },
                Binding {
                    field: "on_hover".into(),
                    element: "button#play.btn".into(),
                    detail: "mouseover".into()
                },
                Binding {
                    field: "item in items".into(),
                    element: "div".into(),
                    detail: "for".into()
                },
            ]
        );

        // Outputs: text mustaches plus the bound class attribute; `{{count}}`
        // twice dedupes to one endpoint; plain attrs (`src`) bind nothing.
        assert_eq!(
            m.outputs,
            vec![
                Binding {
                    field: "label".into(),
                    element: "button#play.btn".into(),
                    detail: "text".into()
                },
                Binding {
                    field: "item.name".into(),
                    element: "div".into(),
                    detail: "text".into()
                },
                Binding {
                    field: "flag".into(),
                    element: "span".into(),
                    detail: "data-class-active".into()
                },
                Binding {
                    field: "count".into(),
                    element: "span".into(),
                    detail: "text".into()
                },
            ]
        );

        // Fields: alphabetical, input/output flags merged across usages.
        assert_eq!(
            m.fields,
            vec![
                FieldUse {
                    field: "count".into(),
                    input: false,
                    output: true
                },
                FieldUse {
                    field: "flag".into(),
                    input: false,
                    output: true
                },
                FieldUse {
                    field: "item in items".into(),
                    input: true,
                    output: false
                },
                FieldUse {
                    field: "item.name".into(),
                    input: false,
                    output: true
                },
                FieldUse {
                    field: "label".into(),
                    input: false,
                    output: true
                },
                FieldUse {
                    field: "on_hover".into(),
                    input: true,
                    output: false
                },
                FieldUse {
                    field: "on_play".into(),
                    input: true,
                    output: false
                },
            ]
        );
    }

    #[test]
    fn mustache_whitespace_and_unclosed_are_tolerated() {
        // `{{ flag }}` trims to `flag`; an unclosed `{{` binds nothing.
        let m = model(r#"<rml><body><p data-visible="{{ flag }}">{{ open }} {{ }}</p></body></rml>"#);
        assert_eq!(
            m.outputs,
            vec![
                Binding {
                    field: "flag".into(),
                    element: "p".into(),
                    detail: "data-visible".into()
                },
                Binding {
                    field: "open".into(),
                    element: "p".into(),
                    detail: "text".into()
                },
            ]
        );
    }

    #[test]
    fn document_without_bindings_is_empty() {
        let m = model(r#"<rml><body><div id="a">static text</div></body></rml>"#);
        assert!(m.is_empty());
        assert!(m.fields.is_empty());
    }
}
