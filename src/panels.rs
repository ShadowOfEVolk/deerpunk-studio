//! Tab renderers: file-based project panels + live collection panels ported
//! from the deerpunk-inspector UI (list shell, Inspector detail, JsonView).

use eframe::egui;
use serde_json::Value;

use crate::app::{StudioApp, Tab};
use crate::assets::AssetKind;
use crate::net::ConnState;
use crate::protocol::{self, ComponentSnapshot};
use crate::theme;

pub fn render(ui: &mut egui::Ui, tab: &Tab, app: &mut StudioApp) {
    match tab {
        Tab::Project => project_panel(ui, app),
        Tab::Assets => assets_panel(ui, app),
        Tab::Textures => textures_panel(ui, app),
        Tab::Cvars => cvars_panel(ui, app),
        Tab::Raw => raw_panel(ui, app),
        Tab::Console => console_panel(ui, app),
        Tab::Live(id) => collection_panel(ui, app, id),
        Tab::Detail => detail_panel(ui, app),
    }
}

fn kv_row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.strong(key);
        ui.label(value);
    });
}

// ---------------------------------------------------------------- file panels

fn project_panel(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(project) = app.project.clone() else {
        ui.centered_and_justified(|ui| {
            ui.weak("no project open — File → Open Game Folder…");
        });
        return;
    };
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.heading("deerpunk.toml");
        kv_row(ui, "project root", &project.root.display().to_string());
        kv_row(ui, "project.name", &project.name);
        kv_row(
            ui,
            "engine.gamedir",
            &format!(
                "{} {}",
                project.gamedir.display(),
                if project.gamedir_valid() { "(ok)" } else { "(missing)" }
            ),
        );
        if project.launch_commands.is_empty() {
            kv_row(ui, "launch.commands", "[]");
        } else {
            for cmd in &project.launch_commands {
                kv_row(ui, "launch.commands", cmd);
            }
        }
        for (key, value) in &project.extras {
            kv_row(ui, key, value);
        }
        ui.add_space(12.0);
        ui.heading("gameinfo.txt");
        if let Some(info) = &app.gameinfo {
            kv_row(ui, "gamefolder", &info.gamefolder);
            kv_row(ui, "title", info.window_title());
            kv_row(ui, "basedir", &info.basedir);
            kv_row(ui, "icon", &info.icon);
            kv_row(ui, "version", &info.version);
            let mut extras: Vec<_> = info.extras.iter().collect();
            extras.sort();
            for (k, v) in extras {
                kv_row(ui, k, v);
            }
        } else {
            ui.weak("gamedir not resolved");
        }
    });
}

fn assets_panel(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(scan) = &app.scan else {
        ui.centered_and_justified(|ui| {
            ui.weak(if app.scan_rx.is_some() { "scanning…" } else { "no project open" });
        });
        return;
    };

    ui.horizontal(|ui| {
        ui.label("🔍");
        ui.add(
            egui::TextEdit::singleline(&mut app.assets_filter)
                .hint_text("filter by path…")
                .desired_width(240.0),
        );
        egui::ComboBox::from_id_salt("assets_kind")
            .selected_text(app.assets_kind.map(|k| k.label()).unwrap_or("All kinds"))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut app.assets_kind, None, "All kinds");
                for kind in &crate::app::KINDS {
                    ui.selectable_value(&mut app.assets_kind, Some(*kind), kind.label());
                }
            });
    });
    ui.separator();

    let filter = app.assets_filter.to_lowercase();
    let kind = app.assets_kind;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        egui::Grid::new("assets_table")
            .striped(true)
            .num_columns(4)
            .spacing([12.0, 2.0])
            .show(ui, |ui| {
                for entry in &scan.entries {
                    let path = entry.rel.display().to_string();
                    if !filter.is_empty() && !path.to_lowercase().contains(&filter) {
                        continue;
                    }
                    if let Some(k) = kind {
                        if entry.kind != k {
                            continue;
                        }
                    }
                    let name = entry
                        .rel
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default();
                    let dir = entry
                        .rel
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    ui.monospace(name);
                    ui.weak(dir);
                    ui.weak(entry.kind.label());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(crate::assets::fmt_size(entry.size));
                    });
                    ui.end_row();
                }
            });
    });
}

fn textures_panel(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(scan) = app.scan.clone() else {
        ui.centered_and_justified(|ui| ui.weak("no project open"));
        return;
    };
    ui.horizontal(|ui| {
        ui.label("🔍");
        ui.add(
            egui::TextEdit::singleline(&mut app.textures_filter)
                .hint_text("filter textures…")
                .desired_width(240.0),
        );
    });
    ui.separator();
    let filter = app.textures_filter.to_lowercase();
    let gamedir = app
        .project
        .as_ref()
        .map(|p| p.gamedir.clone())
        .unwrap_or_default();
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        egui::Grid::new("textures_grid").spacing([8.0, 8.0]).show(ui, |ui| {
            let mut col = 0;
            for entry in &scan.entries {
                if entry.kind != AssetKind::Texture {
                    continue;
                }
                let path = entry.rel.display().to_string();
                if !filter.is_empty() && !path.to_lowercase().contains(&filter) {
                    continue;
                }
                let abs = gamedir.join(&entry.rel);
                let uri = format!("file://{}", abs.display());
                ui.vertical(|ui| {
                    ui.add(
                        egui::Image::new(egui::ImageSource::Uri(uri.into()))
                            .fit_to_exact_size(egui::vec2(64.0, 64.0)),
                    );
                    ui.weak(entry.rel.file_name().and_then(|s| s.to_str()).unwrap_or("?"));
                });
                col += 1;
                if col % 8 == 0 {
                    ui.end_row();
                }
            }
            if col % 8 != 0 {
                ui.end_row();
            }
        });
    });
}

fn cvars_panel(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(project) = &app.project else {
        ui.centered_and_justified(|ui| ui.weak("no project open"));
        return;
    };
    let path = project.gamedir.join("config/cvars_catalog.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        ui.centered_and_justified(|ui| {
            ui.weak(format!("no cvars_catalog.json at {}", path.display()));
        });
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        ui.colored_label(theme::YELLOW, "cvars_catalog.json: parse error");
        return;
    };
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    ui.horizontal(|ui| {
        ui.label("🔍");
        ui.add(
            egui::TextEdit::singleline(&mut app.cvars_filter)
                .hint_text("filter cvars…")
                .desired_width(240.0),
        );
        ui.weak(format!("{} cvars", entries.len()));
    });
    ui.separator();

    let filter = app.cvars_filter.to_lowercase();
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        egui::Grid::new("cvars_table")
            .striped(true)
            .num_columns(5)
            .spacing([16.0, 2.0])
            .show(ui, |ui| {
                for entry in &entries {
                    let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
                    if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
                        continue;
                    }
                    let category = entry.get("category").and_then(Value::as_str).unwrap_or("");
                    let ty = entry.get("type").and_then(Value::as_str).unwrap_or("");
                    let default = entry.get("default").and_then(Value::as_str).unwrap_or("");
                    let label = entry.get("label").and_then(Value::as_str).unwrap_or("");
                    let description = entry.get("description").and_then(Value::as_str).unwrap_or("");
                    ui.monospace(name);
                    ui.weak(category);
                    ui.weak(ty);
                    ui.monospace(default);
                    ui.vertical(|ui| {
                        if !label.is_empty() {
                            ui.label(label);
                        }
                        if !description.is_empty() {
                            ui.weak(description);
                        }
                    });
                    ui.end_row();
                }
            });
    });
}

fn raw_panel(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(project) = &app.project else {
        ui.centered_and_justified(|ui| ui.weak("no project open"));
        return;
    };
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.monospace(&project.raw);
    });
}

fn console_panel(ui: &mut egui::Ui, app: &mut StudioApp) {
    egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
        let lines = app.console_lines.clone();
        for line in lines {
            ui.monospace(line);
        }
    });
}

// -------------------------------------------------------------- live panels

fn collection_panel(ui: &mut egui::Ui, app: &mut StudioApp, id: &str) {
    let Some(result) = app.lists.get(id).cloned() else {
        match app.conn_state {
            ConnState::Connected => ui.centered_and_justified(|ui| ui.weak("no data — refresh")),
            _ => ui.centered_and_justified(|ui| ui.weak("not connected")),
        };
        return;
    };

    let label = app
        .collections
        .iter()
        .find(|c| c.id == *id)
        .map(|c| c.label.clone())
        .unwrap_or_else(|| id.to_string());
    let get_mode = app
        .collections
        .iter()
        .find(|c| c.id == *id)
        .map(|c| c.modes.contains(&protocol::CollectionMode::Get))
        .unwrap_or(false);
    let id_field = app
        .collections
        .iter()
        .find(|c| c.id == *id)
        .map(|c| c.id_field.clone())
        .unwrap_or_else(|| "index".into());
    let rows = protocol::list_rows(&result);

    ui.horizontal(|ui| {
        ui.heading(&label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(format!(
                "{} items · refreshed {}",
                rows.len(),
                app.last_refresh.elapsed().as_secs()
            ));
            if ui.button("↻").clicked() {
                app.request_refresh();
            }
        });
    });
    ui.separator();

    if rows.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.weak("snapshot — see Inspector detail");
        });
        json_view(ui, "snapshot", 0, &result);
        return;
    }

    let filter_state = app.filters.entry(id.to_string()).or_default();
    ui.horizontal(|ui| {
        ui.label("🔍");
        ui.add(
            egui::TextEdit::singleline(filter_state)
                .hint_text("filter…")
                .desired_width(240.0),
        );
    });

    let keys = protocol::row_keys(&rows);
    let filter = filter_state.to_lowercase();

    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        egui::Grid::new(format!("live_{id}_table"))
            .striped(true)
            .num_columns(keys.len().max(1))
            .spacing([16.0, 2.0])
            .show(ui, |ui| {
                for row in &rows {
                    let first = keys
                        .first()
                        .and_then(|k| row.get(k.as_str()))
                        .map(protocol::cell)
                        .unwrap_or_default();
                    let name = row.get("name").and_then(Value::as_str).unwrap_or("");
                    if !filter.is_empty() && !format!("{first} {name}").to_lowercase().contains(&filter) {
                        continue;
                    }
                    let row_index = row.get(id_field.as_str()).map(protocol::cell);
                    let selected = app
                        .selection
                        .as_ref()
                        .map(|s| s.col == id && row_index.as_deref().map(|v| v == s.index_str).unwrap_or(false))
                        .unwrap_or(false);
                    let response = ui.selectable_label(selected, name_or_first(name, &first));
                    for key in keys.iter().skip(1) {
                        let v = row.get(key.as_str()).map(protocol::cell).unwrap_or_default();
                        ui.monospace(v);
                    }
                    ui.end_row();
                    if response.clicked() {
                        if let Some(index) = row_index {
                            if get_mode {
                                app.select(id.to_string(), index);
                            } else {
                                // List-only collection: show the row itself.
                                app.detail = Some(row.clone());
                            }
                        }
                    }
                }
            });
    });
}

fn name_or_first(name: &str, first: &str) -> String {
    if name.is_empty() {
        first.to_string()
    } else {
        name.to_string()
    }
}

pub fn detail_panel(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(detail) = app.detail.clone() else {
        ui.centered_and_justified(|ui| {
            ui.weak("Select an item");
        });
        return;
    };

    // Entity detail gets the Inspector treatment; anything else renders as JSON.
    if let Ok(entity) = serde_json::from_value::<protocol::EntityGetResult>(detail.clone()) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("●").size(10.0).color(theme::GREEN));
            ui.strong(theme::mono_text(&entity.name));
            ui.weak(format!("#{}", entity.index));
            ui.weak(format!("{} components", entity.components.len()));
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for comp in &entity.components {
                component_card(ui, comp);
                ui.add_space(4.0);
            }
        });
    } else {
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            json_view(ui, "detail", 0, &detail);
        });
    }
}

fn component_card(ui: &mut egui::Ui, comp: &ComponentSnapshot) {
    match comp {
        ComponentSnapshot::Tag { slug, .. } => {
            ui.horizontal(|ui| {
                theme::badge(ui, slug, theme::ACCENT);
                ui.weak("tag");
            });
        }
        ComponentSnapshot::Data { slug, data, .. } => {
            egui::Frame::new()
                .fill(theme::CARD)
                .stroke(egui::Stroke::new(1.0, theme::CARD_BORDER))
                .corner_radius(8.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.collapsing(theme::mono_text(slug), |ui| {
                        json_view(ui, &format!("comp_{slug}"), 0, data);
                    });
                });
        }
    }
}

// ------------------------------------------------------------------ JsonView

/// Recursive JSON renderer with inspector syntax colors.
pub fn json_view(ui: &mut egui::Ui, id_salt: &str, depth: usize, v: &Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let label = theme::mono_colored(k, theme::J_KEY);
                match val {
                    Value::Object(_) | Value::Array(_) => {
                        let open = depth < 2;
                        egui::CollapsingHeader::new(label)
                            .id_salt(format!("{id_salt}_{k}"))
                            .default_open(open)
                            .show(ui, |ui| {
                                json_view(ui, &format!("{id_salt}_{k}"), depth + 1, val);
                            });
                    }
                    scalar => {
                        ui.horizontal(|ui| {
                            ui.label(label);
                            ui.label(theme::mono_colored(":", theme::J_PUNCT));
                            scalar_value(ui, scalar);
                        });
                    }
                }
            }
        }
        Value::Array(items) => {
            for (i, val) in items.iter().enumerate() {
                let label = theme::mono_colored(format!("[{i}]"), theme::J_PUNCT);
                match val {
                    Value::Object(_) | Value::Array(_) => {
                        egui::CollapsingHeader::new(label)
                            .id_salt(format!("{id_salt}_a{i}"))
                            .default_open(depth < 2)
                            .show(ui, |ui| {
                                json_view(ui, &format!("{id_salt}_a{i}"), depth + 1, val);
                            });
                    }
                    scalar => {
                        ui.horizontal(|ui| {
                            ui.label(label);
                            scalar_value(ui, scalar);
                        });
                    }
                }
            }
        }
        scalar => scalar_value(ui, scalar),
    }
}

fn scalar_value(ui: &mut egui::Ui, v: &Value) {
    let (text, color) = match v {
        Value::String(s) => (format!("\"{s}\""), theme::J_STRING),
        Value::Number(n) => (n.to_string(), theme::J_NUMBER),
        Value::Bool(b) => (b.to_string(), theme::J_BOOL),
        Value::Null => ("null".into(), theme::J_NULL),
        _ => (v.to_string(), theme::J_PUNCT),
    };
    ui.label(theme::mono_colored(text, color));
}
