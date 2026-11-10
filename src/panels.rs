//! View renderers: sidebar (resource lists) + center (content/detail) per
//! rail view, plus the bottom console panel. Ported from the deerpunk-inspector
//! UI (list shell, Inspector detail, JsonView).

use eframe::egui;
use serde_json::Value;

use crate::ansi::{self, Stream};
use crate::app::{ProjectDoc, StudioApp, View};
use crate::assets::AssetKind;
use crate::net::ConnState;
use crate::protocol::{self, ComponentSnapshot};
use crate::runner::StepState;
use egui_phosphor::variants::regular as ph;
use crate::theme;

/// Sidebar content for the active view (resource lists).
pub fn sidebar(ui: &mut egui::Ui, app: &mut StudioApp, view: &View) {
    match view {
        View::Project => project_sidebar(ui, app),
        View::DiskAssets => assets_sidebar(ui, app),
        View::Textures => textures_sidebar(ui, app),
        View::Cvars => cvars_sidebar(ui, app),
        View::Raw => raw_sidebar(ui),
        View::Live(id) => collection_sidebar(ui, app, id),
    }
}

/// Center content for the active view.
pub fn center(ui: &mut egui::Ui, app: &mut StudioApp, view: &View) {
    match view {
        View::Project => project_center(ui, app),
        View::DiskAssets => assets_center(ui, app),
        View::Textures => textures_center(ui, app),
        View::Cvars => cvars_center(ui, app),
        View::Raw => raw_center(ui, app),
        View::Live(_) => detail_panel(ui, app),
    }
}

fn kv_row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.strong(key);
        ui.label(value);
    });
}

fn sidebar_header(ui: &mut egui::Ui, title: &str, count: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.strong(egui::RichText::new(title).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(theme::small(count));
        });
    });
    ui.separator();
}

fn filter_box(ui: &mut egui::Ui, text: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.label(theme::small(ph::MAGNIFYING_GLASS));
        ui.add(
            egui::TextEdit::singleline(text)
                .hint_text(egui::RichText::new(hint).small().color(theme::FAINT))
                .desired_width(ui.available_width() - 20.0),
        );
    });
}

fn sidebar_row(ui: &mut egui::Ui, selected: bool, text: egui::RichText) -> egui::Response {
    ui.add(
        egui::Button::new(text.size(13.0))
            .fill(if selected { theme::ACCENT } else { egui::Color32::TRANSPARENT })
            .stroke(if selected {
                egui::Stroke::NONE
            } else {
                egui::Stroke::NONE
            })
            .min_size(egui::vec2(ui.available_width(), 18.0)),
    )
}

fn selected_text_color(selected: bool) -> egui::Color32 {
    if selected {
        egui::Color32::WHITE
    } else {
        theme::TEXT_DIM
    }
}

// ------------------------------------------------------------- project view

fn project_sidebar(ui: &mut egui::Ui, app: &mut StudioApp) {
    sidebar_header(ui, "Project", if app.project.is_some() { "ready" } else { "none" });
    for (doc, label) in [
        (ProjectDoc::Summary, "Summary"),
        (ProjectDoc::Toml, "deerpunk.toml"),
        (ProjectDoc::GameInfo, "gameinfo.txt"),
    ] {
        let selected = app.project_doc == doc;
        if sidebar_row(
            ui,
            selected,
            egui::RichText::new(label).color(selected_text_color(selected)),
        )
        .clicked()
        {
            app.project_doc = doc;
        }
    }
}

fn project_center(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(project) = app.project.clone() else {
        ui.centered_and_justified(|ui| {
            ui.weak("no project open — Open… or drag a folder here");
        });
        return;
    };
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        match app.project_doc {
            ProjectDoc::Summary => {
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
                kv_row(ui, "engine.path", &project.engine_dir.display().to_string());
                for lib in &project.gamelibs {
                    kv_row(
                        ui,
                        "launch.gamelibs",
                        &format!("{} → {} ({})", lib.package, lib.artifact, lib.slot),
                    );
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
                }
            }
            ProjectDoc::Toml => {
                ui.monospace(&project.raw);
            }
            ProjectDoc::GameInfo => {
                if let Some(info) = &app.gameinfo {
                    kv_row(ui, "gamefolder", &info.gamefolder);
                    kv_row(ui, "title", info.window_title());
                    kv_row(ui, "basedir", &info.basedir);
                    kv_row(ui, "icon", &info.icon);
                    kv_row(ui, "version", &info.version);
                    kv_row(ui, "startmap", &info.startmap);
                    kv_row(ui, "trainmap", &info.trainmap);
                    kv_row(ui, "gamedll", &info.gamedll);
                    let mut extras: Vec<_> = info.extras.iter().collect();
                    extras.sort();
                    for (k, v) in extras {
                        kv_row(ui, k, v);
                    }
                } else {
                    ui.weak("gamedir not resolved");
                }
            }
        }
    });
}

// -------------------------------------------------------------- assets view

fn assets_sidebar(ui: &mut egui::Ui, app: &mut StudioApp) {
    let total = app.scan.as_ref().map(|s| s.entries.len()).unwrap_or(0);
    sidebar_header(ui, "Disk assets", format!("{total} files"));
    filter_box(ui, &mut app.assets_filter, "filter by path…");
    ui.add_space(4.0);
    let current_kind = app.assets_kind;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        let mut row = |ui: &mut egui::Ui, k: Option<AssetKind>, label: &str, count: usize| {
            let selected = app.assets_kind == k;
            let text = egui::RichText::new(format!("{label}  {count}"))
                .color(selected_text_color(selected));
            if sidebar_row(ui, selected, text).clicked() {
                app.assets_kind = k;
            }
        };
        row(ui, None, "All", total);
        for kind in &crate::app::KINDS {
            let count = app
                .scan
                .as_ref()
                .map(|s| s.entries.iter().filter(|e| e.kind == *kind).count())
                .unwrap_or(0);
            if count > 0 || current_kind == Some(*kind) {
                row(ui, Some(*kind), kind.label(), count);
            }
        }
    });
}

fn assets_center(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(scan) = &app.scan else {
        ui.centered_and_justified(|ui| {
            ui.weak(if app.scan_rx.is_some() { "scanning…" } else { "no project open" });
        });
        return;
    };

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

// ----------------------------------------------------------- textures view

fn textures_sidebar(ui: &mut egui::Ui, app: &mut StudioApp) {
    let count = app
        .scan
        .as_ref()
        .map(|s| s.entries.iter().filter(|e| e.kind == AssetKind::Texture).count())
        .unwrap_or(0);
    sidebar_header(ui, "Textures", format!("{count}"));
    filter_box(ui, &mut app.textures_filter, "filter textures…");
    ui.add_space(4.0);
    let Some(scan) = app.scan.clone() else {
        return;
    };
    let filter = app.textures_filter.to_lowercase();
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        for entry in &scan.entries {
            if entry.kind != AssetKind::Texture {
                continue;
            }
            let path = entry.rel.display().to_string();
            if !filter.is_empty() && !path.to_lowercase().contains(&filter) {
                continue;
            }
            let name = entry.rel.display().to_string();
            let selected = app.selected_texture.as_deref() == Some(name.as_str());
            let label_text = name.clone();
            if sidebar_row(
                ui,
                selected,
                egui::RichText::new(label_text).color(selected_text_color(selected)),
            )
            .clicked()
            {
                app.selected_texture = Some(name);
            }
        }
    });
}

fn textures_center(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(selected) = app.selected_texture.clone() else {
        ui.centered_and_justified(|ui| ui.weak("select a texture in the sidebar"));
        return;
    };
    let Some(project) = app.project.clone() else {
        return;
    };
    let abs = project.gamedir.join(&selected);
    let uri = format!("file://{}", abs.display());
    ui.heading(&selected);
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        ui.add_space(8.0);
        ui.add(
            egui::Image::new(egui::ImageSource::Uri(uri.into()))
                .max_size(ui.available_size()),
        );
    });
}

// -------------------------------------------------------------- cvars view

fn cvars_sidebar(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(project) = &app.project else {
        sidebar_header(ui, "Cvars", "no project");
        return;
    };
    let path = project.gamedir.join("config/cvars_catalog.json");
    let entries = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("entries").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    sidebar_header(ui, "Cvars", format!("{}", entries.len()));

    // Categories with counts.
    let mut categories: Vec<(String, usize)> = Vec::new();
    for entry in &entries {
        let cat = entry.get("category").and_then(Value::as_str).unwrap_or("");
        if let Some(slot) = categories.iter_mut().find(|(c, _)| c == cat) {
            slot.1 += 1;
        } else {
            categories.push((cat.to_string(), 1));
        }
    }
    categories.sort();
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        let mut row = |ui: &mut egui::Ui, cat: Option<&str>, label: &str, count: usize| {
            let selected = app.cvars_category.as_deref() == cat;
            let text =
                egui::RichText::new(format!("{label}  {count}")).color(selected_text_color(selected));
            if sidebar_row(ui, selected, text).clicked() {
                app.cvars_category = cat.map(str::to_string);
            }
        };
        row(ui, None, "All", entries.len());
        for (cat, count) in &categories {
            row(ui, Some(cat.as_str()), cat, *count);
        }
    });
}

fn cvars_center(ui: &mut egui::Ui, app: &mut StudioApp) {
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

    let filter = app.cvars_filter.to_lowercase();
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        egui::Grid::new("cvars_table")
            .striped(true)
            .num_columns(5)
            .spacing([16.0, 2.0])
            .show(ui, |ui| {
                for entry in &entries {
                    let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
                    let category = entry.get("category").and_then(Value::as_str).unwrap_or("");
                    if let Some(want) = &app.cvars_category {
                        if category != want {
                            continue;
                        }
                    }
                    if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
                        continue;
                    }
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

// ---------------------------------------------------------------- raw view

fn raw_sidebar(ui: &mut egui::Ui) {
    sidebar_header(ui, "deerpunk.toml", "raw");
    ui.weak("raw file contents");
}

fn raw_center(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(project) = &app.project else {
        ui.centered_and_justified(|ui| ui.weak("no project open"));
        return;
    };
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.monospace(&project.raw);
    });
}

// -------------------------------------------------------------- live views

fn collection_sidebar(ui: &mut egui::Ui, app: &mut StudioApp, id: &str) {
    let Some(result) = app.lists.get(id).cloned() else {
        sidebar_header(ui, id, match app.conn_state {
            ConnState::Connected => "no data",
            _ => "not connected",
        });
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
    sidebar_header(ui, &label, format!("{}", rows.len()));

    let filter_state = app.filters.entry(id.to_string()).or_default();
    filter_box(ui, filter_state, "filter…");
    ui.add_space(2.0);

    let keys = protocol::row_keys(&rows);
    let filter = filter_state.to_lowercase();
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
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
            let index_str = row_index.clone().unwrap_or_default();
            let selected = app
                .selection
                .as_ref()
                .map(|s| s.col == id && s.index_str == index_str)
                .unwrap_or(false);
            if sidebar_row(
                ui,
                selected,
                egui::RichText::new(name_or_first(name, &first))
                    .color(selected_text_color(selected)),
            )
            .clicked()
            {
                if get_mode {
                    app.select(id.to_string(), index_str);
                } else {
                    app.detail = Some(row.clone());
                }
            }
        }
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

// ----------------------------------------------------------------- console

pub fn console_panel(ui: &mut egui::Ui, app: &mut StudioApp) {
    // Pre_run step strip.
    if !app.steps.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for step in &app.steps {
                let (glyph, color) = match &step.state {
                    StepState::Pending => ("…", theme::FAINT),
                    StepState::Running => ("▶", theme::ACCENT),
                    StepState::Ok => ("✓", theme::GREEN),
                    StepState::Failed(_) => ("✗", theme::RED),
                };
                ui.label(egui::RichText::new(glyph).small().color(color));
                ui.label(theme::small(&step.name));
                if let StepState::Failed(e) = &step.state {
                    ui.label(egui::RichText::new(e.clone()).small().color(theme::RED));
                }
                ui.label(theme::small("·"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new(theme::small("clear")).frame(false))
                    .clicked()
                {
                    app.console = Stream::new(5000);
                }
            });
        });
        ui.separator();
    }

    // Terminal stream (PTY output, ANSI colors applied). The explicit height
    // bound keeps the resizable bottom panel from growing to fit the stream:
    // `auto_shrink(false)` alone would expand the panel to the full window.
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let pending = !app.console.pending().is_empty();
    let total = app.console.lines.len() + usize::from(pending);
    let stream_height = (ui.available_height() - 3.0 * row_h).max(row_h);
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .max_height(stream_height)
        .stick_to_bottom(true)
        .show_rows(ui, row_h, total.max(1), |ui, range| {
            for i in range {
                let line = if i < app.console.lines.len() {
                    &app.console.lines[i]
                } else if pending {
                    app.console.pending()
                } else {
                    continue;
                };
                render_console_line(ui, line);
            }
        });

    // Console command input (engine reads lines from stdin).
    ui.separator();
    ui.horizontal(|ui| {
        let running = app.run_state == crate::runner::RunState::Running;
        ui.label(theme::mono_colored(">", theme::MUTED));
        let hint = if running { "console command…" } else { "engine not running" };
        let resp = ui.add(
            egui::TextEdit::singleline(&mut app.input)
                .hint_text(egui::RichText::new(hint).color(theme::FAINT))
                .desired_width(f32::INFINITY)
                .text_color(theme::CONSOLE_TEXT),
        );
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if enter && running && !app.input.trim().is_empty() {
            app.runner.input(app.input.trim().to_string());
            app.input.clear();
        }
    });
}

fn render_console_line(ui: &mut egui::Ui, line: &[ansi::Span]) {
    let mut job = egui::text::LayoutJob::default();
    for span in line {
        let mut color = span
            .style
            .color
            .map(|(r, g, b)| theme::console_soften(egui::Color32::from_rgb(r, g, b)))
            .unwrap_or(theme::CONSOLE_TEXT);
        if span.style.dim {
            // Widen before multiplying: u8 * 6 overflows for components > 42.
            let dim = |v: u8| ((v as u16) * 6 / 10) as u8;
            color = egui::Color32::from_rgb(dim(color.r()), dim(color.g()), dim(color.b()));
        }
        job.append(
            &span.text,
            0.0,
            egui::TextFormat::simple(egui::FontId::monospace(13.0), color),
        );
    }
    if job.is_empty() {
        job.append(
            " ",
            0.0,
            egui::TextFormat::simple(egui::FontId::monospace(13.0), theme::CONSOLE_TEXT),
        );
    }
    ui.add(egui::Label::new(job));
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
