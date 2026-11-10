//! RmlUi document parser: `.rml` markup is XML (a `<rml>`/`<template>` root
//! with `<head>` holding `<title>`, `<link type="text/rcss">` and inline
//! `<style>`/`<script>` blocks, and a `<body>` of nested elements). Pure
//! parsing — no UI: the viewer in [`crate::viewers`] renders the tree.

use quick_xml::Reader;
use quick_xml::events::Event;

// ------------------------------------------------------------------ parsing

/// One element of the parsed RML document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Child>,
}

/// An element or a run of character data inside an element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Child {
    Elem(Node),
    Text(String),
}

/// Parsed RmlUi document plus the bits the viewer highlights.
///
/// Whitespace handling: text is trimmed and whitespace-only runs are dropped,
/// both in the tree and in the `title` / `styles` / `scripts` collections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    pub root: Node,
    /// Text of the first `<title>`, if any.
    pub title: Option<String>,
    /// `href` of every `<link>` (style sheets), in document order.
    pub sheets: Vec<String>,
    /// Inner text of every inline `<style>` block, in document order.
    pub styles: Vec<String>,
    /// Inner text of every inline `<script>` block, in document order.
    pub scripts: Vec<String>,
}

/// Parse an RmlUi document (XML with `<rml>` / `<template>` roots).
///
/// Well-formedness is checked lightly: element tags must nest correctly
/// (mismatched or stray close tags are errors), there must be exactly one
/// root element, and every open element must be closed. Comments,
/// declarations and DOCTYPE are ignored. Entities are decoded in text and
/// attribute values.
pub fn parse(src: &str) -> Result<Doc, String> {
    let mut reader = Reader::from_str(src);
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut title: Option<String> = None;
    let mut sheets: Vec<String> = Vec::new();
    let mut styles: Vec<String> = Vec::new();
    let mut scripts: Vec<String> = Vec::new();
    // quick-xml splits character data at `&ref;` into Text + GeneralRef
    // chunks; buffer them so an entity in the middle keeps the text one run.
    let mut pending = String::new();

    loop {
        let ev = reader.read_event().map_err(|e| e.to_string())?;
        match &ev {
            Event::Start(e) | Event::Empty(e) => {
                flush_text(
                    &mut pending,
                    &mut stack,
                    &mut title,
                    &mut styles,
                    &mut scripts,
                );
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let mut attrs: Vec<(String, String)> = Vec::new();
                for a in e.attributes().flatten() {
                    attrs.push((
                        String::from_utf8_lossy(a.key.as_ref()).into_owned(),
                        xml_unescape(&String::from_utf8_lossy(&a.value)),
                    ));
                }
                if tag == "link" {
                    if let Some((_, href)) = attrs.iter().find(|(k, _)| k == "href") {
                        sheets.push(href.clone());
                    }
                }
                if matches!(ev, Event::Start(_)) {
                    stack.push(Node {
                        tag,
                        attrs,
                        children: Vec::new(),
                    });
                } else {
                    attach(
                        Node {
                            tag,
                            attrs,
                            children: Vec::new(),
                        },
                        &mut stack,
                        &mut root,
                    )?;
                }
            }
            Event::End(e) => {
                flush_text(
                    &mut pending,
                    &mut stack,
                    &mut title,
                    &mut styles,
                    &mut scripts,
                );
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let node = stack
                    .pop()
                    .ok_or_else(|| format!("close tag </{tag}> with no open element"))?;
                if node.tag != tag {
                    return Err(format!(
                        "mismatched close tag </{tag}> — expected </{}>",
                        node.tag
                    ));
                }
                attach(node, &mut stack, &mut root)?;
            }
            Event::Text(t) => {
                pending.push_str(&xml_unescape(&String::from_utf8_lossy(t.as_ref())));
            }
            Event::GeneralRef(r) => {
                // Deref gives the content between `&` and `;`: numeric refs
                // resolve via the reader, the five predefined entities here,
                // unknown ones stay literal.
                let name = String::from_utf8_lossy(r.as_ref()).into_owned();
                let decoded = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    _ => match name.as_str() {
                        "amp" => "&".to_string(),
                        "lt" => "<".to_string(),
                        "gt" => ">".to_string(),
                        "quot" => "\"".to_string(),
                        "apos" => "'".to_string(),
                        _ => format!("&{name};"),
                    },
                };
                pending.push_str(&decoded);
            }
            Event::CData(c) => {
                pending.push_str(&String::from_utf8_lossy(c.as_ref()));
            }
            Event::Eof => {
                if let Some(top) = stack.last() {
                    return Err(format!("unclosed element <{}>", top.tag));
                }
                return Ok(Doc {
                    root: root.ok_or_else(|| "empty document".to_string())?,
                    title,
                    sheets,
                    styles,
                    scripts,
                });
            }
            // Comments, `<?xml …?>`, DOCTYPE, PIs — no impact on the view.
            _ => {}
        }
    }
}

/// Attach a finished node to its parent, or make it the document root.
fn attach(node: Node, stack: &mut [Node], root: &mut Option<Node>) -> Result<(), String> {
    if let Some(top) = stack.last_mut() {
        top.children.push(Child::Elem(node));
    } else if root.is_none() {
        *root = Some(node);
    } else {
        return Err("multiple root elements".to_string());
    }
    Ok(())
}

/// File the buffered text run into the enclosing element, capturing `title`,
/// inline `<style>` and `<script>` content along the way. Text is trimmed and
/// whitespace-only runs are dropped.
fn flush_text(
    pending: &mut String,
    stack: &mut [Node],
    title: &mut Option<String>,
    styles: &mut Vec<String>,
    scripts: &mut Vec<String>,
) {
    let text = pending.trim();
    if text.is_empty() {
        pending.clear();
        return;
    }
    let Some(top) = stack.last_mut() else {
        pending.clear();
        return;
    };
    let text = text.to_string();
    match top.tag.as_str() {
        "title" if title.is_none() => *title = Some(text.clone()),
        "style" => styles.push(text.clone()),
        "script" => scripts.push(text.clone()),
        _ => {}
    }
    top.children.push(Child::Text(text));
    pending.clear();
}

/// Decode the five predefined XML entities plus numeric character refs
/// (`&#65;`, `&#x41;`). Unknown or malformed refs stay literal — the repo
/// pins quick-xml with `default-features = false`, so its `unescape` is not
/// available.
fn xml_unescape(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(end) = after.find(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let ent = &after[..end];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                let num =
                    if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        ent.strip_prefix('#').and_then(|d| d.parse::<u32>().ok())
                    };
                num.and_then(char::from_u32)
            }
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_sample_doc() {
        let src = r#"<rml><head><title>Hud</title><link type="text/rcss" href="hud.rcss"/><style>body{top:0}</style></head><body><div id="root" class="hud main"><button class="btn">Play &amp; quit</button><img src="x.png"/></div></body></rml>"#;
        let doc = parse(src).expect("sample doc parses");

        assert_eq!(doc.title.as_deref(), Some("Hud"));
        assert_eq!(doc.sheets, vec!["hud.rcss"]);
        assert_eq!(doc.styles, vec!["body{top:0}"]);
        assert!(doc.scripts.is_empty());

        assert_eq!(doc.root.tag, "rml");
        // head: link + style are collected AND kept in the tree.
        let head = match doc.root.children.as_slice() {
            [Child::Elem(head), Child::Elem(_)] if head.tag == "head" => head,
            other => panic!("expected [head, body], got {other:?}"),
        };
        let (link, style) = match head.children.as_slice() {
            [Child::Elem(t), Child::Elem(l), Child::Elem(s)]
                if t.tag == "title" && l.tag == "link" && s.tag == "style" =>
            {
                assert_eq!(t.children, vec![Child::Text("Hud".into())]);
                (l, s)
            }
            other => panic!("expected [title, link, style] in head, got {other:?}"),
        };
        assert_eq!(
            link.attrs,
            vec![
                ("type".into(), "text/rcss".into()),
                ("href".into(), "hud.rcss".into())
            ]
        );
        assert_eq!(style.children, vec![Child::Text("body{top:0}".into())]);

        // body: div#root.hud.main wraps a button (entity-decoded text) and a
        // self-closing img kept as a childless element.
        let body = match doc.root.children.as_slice() {
            [_, Child::Elem(body)] if body.tag == "body" => body,
            other => panic!("expected body as second root child, got {other:?}"),
        };
        let div = match body.children.as_slice() {
            [Child::Elem(div)] if div.tag == "div" => div,
            other => panic!("expected a single div in body, got {other:?}"),
        };
        assert_eq!(
            div.attrs,
            vec![
                ("id".into(), "root".into()),
                ("class".into(), "hud main".into())
            ]
        );
        let (button, img) = match div.children.as_slice() {
            [Child::Elem(b), Child::Elem(i)] if b.tag == "button" && i.tag == "img" => (b, i),
            other => panic!("expected [button, img] in div, got {other:?}"),
        };
        assert_eq!(button.attrs, vec![("class".into(), "btn".into())]);
        assert_eq!(button.children, vec![Child::Text("Play & quit".into())]);
        assert_eq!(img.attrs, vec![("src".into(), "x.png".into())]);
        assert!(img.children.is_empty());
    }

    #[test]
    fn mismatched_close_tag_is_err() {
        assert!(parse("<a><b></a>").is_err());
    }

    #[test]
    fn empty_input_is_err() {
        assert!(parse("").is_err());
    }

    #[test]
    fn unclosed_element_is_err() {
        assert!(parse("<a><b>").is_err());
    }
}
