//! View renderers: sidebar (resource lists) + center (content/detail) per
//! rail view, plus the bottom console panel. Ported from the deerpunk-inspector
//! UI (list shell, Inspector detail, JsonView).

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use eframe::egui;
use serde_json::Value;

use crate::ansi::{self, Stream};
use crate::app::{OpenFile, Scenario, StudioApp, View};
use crate::assets::{AssetKind, ScanResult, fmt_size};
use crate::net::ConnState;
use crate::profiles::RunProfile;
use crate::project::Project;
use crate::protocol::{self, ComponentSnapshot};
use crate::runner::StepState;
use crate::theme;
use egui::containers::collapsing_header::CollapsingState;
use egui_phosphor::variants::regular as ph;

/// Files larger than this render truncated in the text viewer.
const TEXT_RENDER_CAP: usize = 2 * 1024 * 1024;
/// Maximum number of simultaneously open file tabs.
const MAX_OPEN_TABS: usize = 12;
/// Extensions rendered as monospace text.
const TEXT_VIEWER_EXTS: [&str; 9] = [
    "toml", "json", "cfg", "txt", "md", "log", "rc", "wgsl", "hlsl",
];
/// Extensions rendered as images.
const IMAGE_VIEWER_EXTS: [&str; 3] = ["png", "jpg", "jpeg"];

/// Sidebar content for the active view (resource lists).
pub fn sidebar(ui: &mut egui::Ui, app: &mut StudioApp, view: &View) {
    match view {
        View::Files => files_sidebar(ui, app),
        View::DiskAssets => assets_sidebar(ui, app),
        View::Textures => textures_sidebar(ui, app),
        View::Cvars => cvars_sidebar(ui, app),
        View::Raw => raw_sidebar(ui),
        View::LaunchSettings => launch_settings_sidebar(ui, app),
        View::Live(id) => collection_sidebar(ui, app, id),
    }
}

/// Center content for the active view.
pub fn center(ui: &mut egui::Ui, app: &mut StudioApp, view: &View) {
    match view {
        View::Files => files_center(ui, app),
        View::DiskAssets => assets_center(ui, app),
        View::Textures => textures_center(ui, app),
        View::Cvars => cvars_center(ui, app),
        View::Raw => raw_center(ui, app),
        View::LaunchSettings => launch_settings_center(ui, app),
        View::Live(_) => detail_panel(ui, app),
    }
}

// --------------------------------------------------------------- files view

fn files_sidebar(ui: &mut egui::Ui, app: &mut StudioApp) {
    let Some(project) = app.project.clone() else {
        sidebar_header(ui, "Files", "no project");
        ui.weak("open a project to browse its files");
        return;
    };
    let (tree, files) = build_file_tree(&project, app.scan.as_ref());
    sidebar_header(ui, "Files", format!("{files} files"));

    let filter_state = app.filters.entry("files".into()).or_default();
    filter_box(ui, filter_state, "filter files…");
    let filter = filter_state.to_lowercase();
    ui.add_space(2.0);

    let mut tree = tree;
    if !filter.is_empty() {
        tree.children.retain_mut(|child| prune_tree(child, &filter));
    }
    let active_path = app
        .active_tab
        .and_then(|i| app.open_tabs.get(i))
        .map(|f| f.path.clone());
    let gd_slash = slash(&gamedir_rel(&project));
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            render_tree_node(
                ui,
                app,
                &project.root,
                &gd_slash,
                &tree,
                0,
                active_path.as_deref(),
            );
        });
}

/// One node of the project file tree; `children` is empty for files.
struct TreeNode {
    name: String,
    /// Path relative to the project root, `/`-separated (root itself is "").
    rel: String,
    dir: bool,
    children: Vec<TreeNode>,
}

impl TreeNode {
    fn new(name: &str, rel: String, dir: bool) -> Self {
        Self {
            name: name.to_owned(),
            rel,
            dir,
            children: Vec::new(),
        }
    }

    /// Find or create the direct child folder `name`.
    fn child_dir(&mut self, name: &str) -> &mut TreeNode {
        if let Some(i) = self.children.iter().position(|c| c.dir && c.name == name) {
            return &mut self.children[i];
        }
        let rel = if self.rel.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{}", self.rel, name)
        };
        self.children.push(TreeNode::new(name, rel, true));
        self.children.last_mut().expect("just pushed")
    }

    /// Find or create the nested folder chain `path` below `self`.
    fn dir_at(&mut self, path: &Path) -> &mut TreeNode {
        let mut cur = self;
        for comp in path.components() {
            let name = comp.as_os_str().to_string_lossy().into_owned();
            cur = cur.child_dir(&name);
        }
        cur
    }

    /// Sort children folders-first, then files, A→Z, recursively.
    fn sort(&mut self) {
        self.children
            .sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
        for child in &mut self.children {
            child.sort();
        }
    }
}

/// Build the file tree: project-root files plus the scanned gamedir subtree.
/// Returns the root node and the number of files it contains.
fn build_file_tree(project: &Project, scan: Option<&ScanResult>) -> (TreeNode, usize) {
    let mut root = TreeNode::new(&project.name, String::new(), true);

    // Root-level files (deerpunk.toml, run.sh, …) — the scan only covers the
    // gamedir, so these come straight from the filesystem.
    if let Ok(entries) = std::fs::read_dir(&project.root) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = name.clone();
            root.children.push(TreeNode::new(&name, rel, false));
        }
    }
    let mut files = root.children.len();

    let gd_rel = gamedir_rel(project);
    for entry in scan.into_iter().flat_map(|s| &s.entries) {
        let under = gd_rel.join(&entry.rel);
        let dir = under.parent().unwrap_or(Path::new(""));
        let target = if dir.as_os_str().is_empty() {
            &mut root
        } else {
            root.dir_at(dir)
        };
        let name = under
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        target
            .children
            .push(TreeNode::new(&name, slash(&under), false));
        files += 1;
    }
    root.sort();
    (root, files)
}

/// The gamedir as a path relative to the project root (e.g. `gamedir`).
fn gamedir_rel(project: &Project) -> PathBuf {
    project
        .gamedir
        .strip_prefix(&project.root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| PathBuf::from(project.gamedir.file_name().unwrap_or_default()))
}

/// `/`-separated display form of a path.
fn slash(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Drop non-matching files and folders left without matching descendants.
fn prune_tree(node: &mut TreeNode, filter: &str) -> bool {
    if !node.dir {
        return node.rel.to_lowercase().contains(filter);
    }
    node.children.retain_mut(|child| prune_tree(child, filter));
    !node.children.is_empty()
}

/// Phosphor glyph for a file, chosen by extension.
fn file_glyph(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "toml" | "json" | "cfg" | "txt" | "md" | "log" | "rc" => ph::FILE_TEXT,
        "png" | "jpg" | "jpeg" => ph::IMAGE,
        "wgsl" | "hlsl" => ph::FILE_CODE,
        "dylib" | "dll" | "so" => ph::PACKAGE,
        _ => ph::FILE,
    }
}

/// Flat 18px tree row: left-aligned label at `indent`, hover fill, accent
/// fill + white text when selected. Unlike `sidebar_row` (a Button, which
/// centers its label) this keeps parent/child indentation legible.
fn tree_row(
    ui: &mut egui::Ui,
    indent: f32,
    selected: bool,
    label: &str,
    color: egui::Color32,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), egui::Sense::click());
    let bg = if selected {
        Some(theme::ACCENT)
    } else if resp.hovered() {
        Some(theme::HOVER)
    } else {
        None
    };
    if let Some(bg) = bg {
        ui.painter().rect_filled(rect, 2.0, bg);
    }
    ui.painter().text(
        egui::pos2(rect.left() + indent, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        if selected {
            egui::Color32::WHITE
        } else {
            color
        },
    );
    resp
}

fn render_tree_node(
    ui: &mut egui::Ui,
    app: &mut StudioApp,
    root: &Path,
    gd_slash: &str,
    node: &TreeNode,
    depth: usize,
    active_path: Option<&Path>,
) {
    let indent = 8.0 + depth as f32 * 12.0;
    if node.dir {
        // Open/closed state persists in egui memory keyed by the folder path.
        // Root and the gamedir start expanded, deeper folders collapsed.
        let id = egui::Id::new(("files_tree_open", &node.rel));
        let default_open = node.rel.is_empty() || node.rel == gd_slash;
        let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
        let glyph = if state.is_open() {
            ph::FOLDER_OPEN
        } else {
            ph::FOLDER_SIMPLE
        };
        let label = format!("{glyph} {}", node.name);
        let mut open = state.is_open();
        if tree_row(ui, indent, false, &label, selected_text_color(false)).clicked() {
            open = !open;
        }
        if open != state.is_open() {
            state.set_open(open);
            state.store(ui.ctx());
        }
        if open {
            for child in &node.children {
                render_tree_node(ui, app, root, gd_slash, child, depth + 1, active_path);
            }
        }
    } else {
        let abs = root.join(&node.rel);
        let selected = active_path == Some(abs.as_path());
        let label = format!("{} {}", file_glyph(&node.name), node.name);
        if tree_row(ui, indent, selected, &label, selected_text_color(selected)).clicked() {
            open_file_tab(app, abs, node.rel.clone());
        }
    }
}

/// Focus the tab for `path`, or open a new one (capped at MAX_OPEN_TABS,
/// dropping the oldest non-active beyond the cap).
fn open_file_tab(app: &mut StudioApp, path: PathBuf, rel: String) {
    if let Some(i) = app.open_tabs.iter().position(|f| f.path == path) {
        app.active_tab = Some(i);
        return;
    }
    while app.open_tabs.len() >= MAX_OPEN_TABS {
        let active = app.active_tab;
        let victim = (0..app.open_tabs.len())
            .find(|&i| Some(i) != active)
            .unwrap_or(0);
        app.open_tabs.remove(victim);
    }
    app.open_tabs.push(OpenFile { path, rel });
    app.active_tab = Some(app.open_tabs.len() - 1);
}

/// Remove a tab; keep the remaining selection sensible (same slot, clamped).
fn close_tab(app: &mut StudioApp, index: usize) {
    app.open_tabs.remove(index);
    let next = match app.active_tab {
        Some(a) if a > index => Some(a - 1),
        Some(a) if a == index => Some(a.min(app.open_tabs.len().saturating_sub(1))),
        other => other,
    };
    app.active_tab = if app.open_tabs.is_empty() { None } else { next };
}

fn files_center(ui: &mut egui::Ui, app: &mut StudioApp) {
    if app.open_tabs.is_empty() {
        ui.centered_and_justified(|ui| ui.weak("open a file from the browser"));
        return;
    }

    // Tab strip: flat filename tabs, active tab with a lighter fill.
    // The close button is laid out inside the tab frame, right-aligned.
    let mut focus: Option<usize> = None;
    let mut close: Option<usize> = None;
    egui::ScrollArea::horizontal()
        .scroll_bar_visibility(egui::containers::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            for (i, tab) in app.open_tabs.iter().enumerate() {
                let active = app.active_tab == Some(i);
                let name = tab
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| tab.rel.clone());
                // Truncate long filenames so tabs stay compact.
                let name = if name.chars().count() > 18 {
                    format!("{}…", name.chars().take(18).collect::<String>())
                } else {
                    name
                };
                let text =
                    egui::RichText::new(format!("{} {name}", file_glyph(&name))).color(if active {
                        theme::TEXT
                    } else {
                        theme::TAB_INACTIVE
                    });

                let mut frame = egui::Frame::new()
                    .fill(if active {
                        theme::FIELD
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .corner_radius(2.0)
                    .inner_margin(egui::Margin::symmetric(8, 2))
                    .begin(ui);

                let mut label: Option<egui::Response> = None;
                {
                    let inner = &mut frame.content_ui;
                    inner.horizontal(|inner| {
                        label = Some(inner.add(egui::Label::new(text).sense(egui::Sense::click())));
                        let x = inner.add(egui::Button::new(theme::small(ph::X)).frame(false));
                        let x = x.on_hover_cursor(egui::CursorIcon::PointingHand);
                        if x.hovered() {
                            // Brighten the small close glyph on hover.
                            inner.painter()
                                .rect_filled(x.rect, 2.0, egui::Color32::from_white_alpha(25));
                        }
                        if x.clicked() {
                            close = Some(i);
                        }
                    });
                }

                let resp = frame.allocate_space(ui);
                if !active && resp.hovered() {
                    frame.frame.fill = theme::FIELD.gamma_multiply(0.5);
                }
                frame.paint(ui);

                let label = label.expect("tab label added above");
                if label.clicked() {
                    focus = Some(i);
                }
                if label.middle_clicked() {
                    close = Some(i);
                }
            }
        });
    ui.separator();

    if let Some(i) = focus {
        app.active_tab = Some(i);
    }
    if let Some(i) = close {
        close_tab(app, i);
    }

    let Some(index) = app.active_tab else {
        ui.centered_and_justified(|ui| ui.weak("open a file from the browser"));
        return;
    };
    let Some(tab) = app.open_tabs.get(index).cloned() else {
        return;
    };
    file_viewer(ui, app, &tab);
}

fn file_viewer(ui: &mut egui::Ui, app: &mut StudioApp, tab: &OpenFile) {
    let ext = tab
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if IMAGE_VIEWER_EXTS.contains(&ext.as_str()) {
        image_viewer(ui, tab);
    } else if TEXT_VIEWER_EXTS.contains(&ext.as_str()) {
        text_viewer(ui, tab);
    } else {
        binary_viewer(ui, app, tab);
    }
}

#[derive(Clone)]
struct CachedText {
    mtime: Option<SystemTime>,
    content: String,
    truncated: bool,
    /// Cached read failure (unreadable files are not retried every frame).
    error: Option<String>,
}

#[derive(Clone, Default)]
struct TextCache {
    map: HashMap<PathBuf, CachedText>,
}

/// Monospace text viewer; the file is read only when its mtime changed
/// (content cached in egui memory), capped at TEXT_RENDER_CAP. Cache access
/// stays inside short `data_mut` scopes — rendering under the context lock
/// would deadlock.
fn text_viewer(ui: &mut egui::Ui, tab: &OpenFile) {
    let mtime = std::fs::metadata(&tab.path).and_then(|m| m.modified()).ok();
    let cache_id = egui::Id::new("file_text_cache");
    let ctx = ui.ctx().clone();
    // Load (or refresh) the cache entry; read errors are cached too.
    ctx.data_mut(|data| {
        let cache = data.get_temp_mut_or_default::<TextCache>(cache_id);
        if cache.map.get(&tab.path).is_some_and(|c| c.mtime == mtime) {
            return;
        }
        let entry = match std::fs::read(&tab.path) {
            Ok(bytes) => {
                let truncated = bytes.len() > TEXT_RENDER_CAP;
                let cut = if truncated {
                    &bytes[..TEXT_RENDER_CAP]
                } else {
                    &bytes[..]
                };
                let content = String::from_utf8_lossy(cut).into_owned();
                CachedText {
                    mtime,
                    content,
                    truncated,
                    error: None,
                }
            }
            Err(e) => CachedText {
                mtime,
                content: String::new(),
                truncated: false,
                error: Some(e.to_string()),
            },
        };
        if cache.map.len() > 64 {
            cache.map.clear();
        }
        cache.map.insert(tab.path.clone(), entry);
    });
    // Render from a copy — no context lock held while drawing.
    let cached = ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<TextCache>(cache_id)
            .map
            .get(&tab.path)
            .cloned()
    });
    let Some(cached) = cached else { return };
    if let Some(e) = &cached.error {
        ui.colored_label(theme::RED, format!("cannot read {}: {e}", tab.rel));
        return;
    }
    if cached.truncated {
        ui.colored_label(
            theme::YELLOW,
            format!(
                "file truncated to {} for display",
                fmt_size(TEXT_RENDER_CAP as u64)
            ),
        );
    }
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        ui.add(
            egui::Label::new(theme::mono_text(&cached.content))
                .wrap_mode(egui::TextWrapMode::Extend),
        );
    });
}

fn image_viewer(ui: &mut egui::Ui, tab: &OpenFile) {
    let size = std::fs::metadata(&tab.path).map(|m| m.len()).unwrap_or(0);
    let uri = format!("file://{}", tab.path.display());
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        ui.add_space(4.0);
        // Show at native size, downscaled only when larger than the pane —
        // `max_size` alone would blow 16px icons up to the full area.
        let avail = ui.available_size();
        let display = match image_dims(&tab.path) {
            Some((w, h)) if w > 0 && h > 0 => {
                let native = egui::vec2(w as f32, h as f32);
                native * (avail.x / native.x).min(avail.y / native.y).min(1.0)
            }
            _ => egui::vec2(64.0, 64.0),
        };
        ui.add(egui::Image::new(egui::ImageSource::Uri(uri.into())).fit_to_exact_size(display));
        ui.add_space(6.0);
        ui.weak(match image_dims(&tab.path) {
            Some((w, h)) => format!("{size} bytes · {} · {w}×{h} px", fmt_size(size)),
            None => format!("{size} bytes · {}", fmt_size(size)),
        });
    });
}

/// Pixel dimensions from the PNG IHDR or JPEG SOF header (no full decode).
fn image_dims(path: &Path) -> Option<(u32, u32)> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let read = file.read(&mut buf).ok()?;
    buf.truncate(read);
    let png_sig = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if buf.starts_with(&png_sig) {
        if buf.len() < 24 {
            return None;
        }
        let w = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let h = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);
        return Some((w, h));
    }
    jpeg_dims(&buf)
}

fn jpeg_dims(b: &[u8]) -> Option<(u32, u32)> {
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 9 <= b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let sof = (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker);
        if sof {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// Info card for files without a preview (dylib, maps, …).
fn binary_viewer(ui: &mut egui::Ui, app: &mut StudioApp, tab: &OpenFile) {
    let size = std::fs::metadata(&tab.path).map(|m| m.len()).unwrap_or(0);
    let kind = scan_entry_kind(app, &tab.path);
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0, theme::CARD_BORDER))
        .corner_radius(8.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(file_glyph(&tab.rel)).color(theme::ACCENT_TEXT));
                ui.strong(theme::mono_text(&tab.rel));
            });
            ui.add_space(2.0);
            kv_row(ui, "Absolute path", &tab.path.display().to_string());
            kv_row(ui, "Size", &fmt_size(size));
            if let Some(kind) = kind {
                kv_row(ui, "Kind", kind.label());
            }
            ui.add_space(2.0);
            ui.weak("binary — no preview");
        });
}

/// AssetKind of a gamedir file, looked up in the scan (None for root files).
fn scan_entry_kind(app: &StudioApp, path: &Path) -> Option<AssetKind> {
    let project = app.project.as_ref()?;
    let rel = path.strip_prefix(&project.gamedir).ok()?;
    app.scan
        .as_ref()?
        .entries
        .iter()
        .find(|e| e.rel == rel)
        .map(|e| e.kind)
}

// ------------------------------------------------------- launch settings view

fn launch_settings_sidebar(ui: &mut egui::Ui, app: &mut StudioApp) {
    sidebar_header(
        ui,
        "Launch",
        format!("{}·{}", app.profiles.order.len(), app.scenarios.len()),
    );
    ui.add_space(2.0);
    ui.label(theme::small("PROFILES"));
    for id in &app.profiles.order {
        let name = app
            .profiles
            .map
            .get(id)
            .map(|p| p.label.as_str())
            .unwrap_or(id);
        let selected = app.active_profile == *id;
        let text = egui::RichText::new(format!("{} {name}", ph::GEAR))
            .color(selected_text_color(selected));
        ui.add_sized([ui.available_width(), 18.0], egui::Label::new(text));
    }
    ui.add_space(6.0);
    ui.label(theme::small("SCENARIOS"));
    for scenario in &app.scenarios {
        let selected = app.active_scenario == scenario.id;
        let text = egui::RichText::new(format!("{} {}", ph::CARET_RIGHT, scenario.label))
            .color(selected_text_color(selected));
        ui.add_sized([ui.available_width(), 18.0], egui::Label::new(text));
    }
    ui.add_space(6.0);
    if app.project.is_some() {
        ui.label(theme::small(
            ".deerpunk/profiles.toml · .deerpunk/scenarios.toml",
        ));
    }
}

/// Read viewer for launch profiles + scenarios (`.deerpunk/*.toml`); the
/// files keep file order. Real editing happens in an external editor for v1.
fn launch_settings_center(ui: &mut egui::Ui, app: &mut StudioApp) {
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.heading("Launch settings");
            match app.project.as_ref() {
                Some(project) => {
                    ui.weak("read-only view — edit these files in your editor:");
                    ui.label(theme::small(
                        project
                            .root
                            .join(".deerpunk/profiles.toml")
                            .display()
                            .to_string(),
                    ));
                    ui.label(theme::small(
                        project
                            .root
                            .join(".deerpunk/scenarios.toml")
                            .display()
                            .to_string(),
                    ));
                }
                None => {
                    ui.weak("no project open — showing built-in defaults");
                }
            }
            ui.add_space(8.0);

            ui.label(egui::RichText::new("PROFILES").small().color(theme::MUTED));
            ui.separator();
            for id in &app.profiles.order {
                if let Some(profile) = app.profiles.map.get(id) {
                    profile_card(ui, id, profile, app.active_profile == *id);
                    ui.add_space(4.0);
                }
            }

            ui.add_space(8.0);
            ui.label(egui::RichText::new("SCENARIOS").small().color(theme::MUTED));
            ui.separator();
            for scenario in &app.scenarios {
                scenario_card(ui, scenario, app.active_scenario == scenario.id);
                ui.add_space(4.0);
            }
        });
}

fn profile_card(ui: &mut egui::Ui, id: &str, profile: &RunProfile, active: bool) {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0, theme::CARD_BORDER))
        .corner_radius(8.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::mono_text(id));
                ui.weak(&profile.label);
                if active {
                    theme::badge(ui, "active", theme::ACCENT);
                }
            });
            kv_row(ui, "Engine profile", &profile.engine_profile);
            kv_row(ui, "Game profile", &profile.game_profile);
            kv_row(ui, "Engine features", &joined(&profile.engine_features));
            kv_row(ui, "Game features", &joined(&profile.game_features));
            if profile.env.is_empty() {
                kv_row(ui, "Env", "—");
            } else {
                for (key, value) in &profile.env {
                    kv_row(ui, &format!("Env · {key}"), value);
                }
            }
        });
}

fn scenario_card(ui: &mut egui::Ui, scenario: &Scenario, active: bool) {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0, theme::CARD_BORDER))
        .corner_radius(8.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::mono_text(&scenario.id));
                ui.weak(&scenario.label);
                if active {
                    theme::badge(ui, "active", theme::ACCENT);
                }
            });
            kv_row(ui, "Args", &joined(&scenario.args));
        });
}

/// Comma-joined list, or an em dash placeholder for an empty one.
fn joined(items: &[String]) -> String {
    if items.is_empty() {
        "—".into()
    } else {
        items.join(", ")
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
            .fill(if selected {
                theme::ACCENT
            } else {
                egui::Color32::TRANSPARENT
            })
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

// -------------------------------------------------------------- assets view

fn assets_sidebar(ui: &mut egui::Ui, app: &mut StudioApp) {
    let total = app.scan.as_ref().map(|s| s.entries.len()).unwrap_or(0);
    sidebar_header(ui, "Disk assets", format!("{total} files"));
    filter_box(ui, &mut app.assets_filter, "filter by path…");
    ui.add_space(4.0);
    let current_kind = app.assets_kind;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
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
            ui.weak(if app.scan_rx.is_some() {
                "scanning…"
            } else {
                "no project open"
            });
        });
        return;
    };

    let filter = app.assets_filter.to_lowercase();
    let kind = app.assets_kind;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
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
        .map(|s| {
            s.entries
                .iter()
                .filter(|e| e.kind == AssetKind::Texture)
                .count()
        })
        .unwrap_or(0);
    sidebar_header(ui, "Textures", format!("{count}"));
    filter_box(ui, &mut app.textures_filter, "filter textures…");
    ui.add_space(4.0);
    let Some(scan) = app.scan.clone() else {
        return;
    };
    let filter = app.textures_filter.to_lowercase();
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
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
        ui.add(egui::Image::new(egui::ImageSource::Uri(uri.into())).max_size(ui.available_size()));
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
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            let mut row = |ui: &mut egui::Ui, cat: Option<&str>, label: &str, count: usize| {
                let selected = app.cvars_category.as_deref() == cat;
                let text = egui::RichText::new(format!("{label}  {count}"))
                    .color(selected_text_color(selected));
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
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
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
                        let description = entry
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("");
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
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.monospace(&project.raw);
        });
}

// -------------------------------------------------------------- live views

fn collection_sidebar(ui: &mut egui::Ui, app: &mut StudioApp, id: &str) {
    let Some(result) = app.lists.get(id).cloned() else {
        sidebar_header(
            ui,
            id,
            match app.conn_state {
                ConnState::Connected => "no data",
                _ => "not connected",
            },
        );
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
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            for row in &rows {
                let first = keys
                    .first()
                    .and_then(|k| row.get(k.as_str()))
                    .map(protocol::cell)
                    .unwrap_or_default();
                let name = row.get("name").and_then(Value::as_str).unwrap_or("");
                if !filter.is_empty() && !format!("{first} {name}").to_lowercase().contains(&filter)
                {
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
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                for comp in &entity.components {
                    component_card(ui, comp);
                    ui.add_space(4.0);
                }
            });
    } else {
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
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
        let hint = if running {
            "console command…"
        } else {
            "engine not running"
        };
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
