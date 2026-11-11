//! Files panel (left dock): the scanned gamedir as a tree. The flat
//! [`ScanResult`] entry list is folded into a path tree of gpui-kit
//! `TreeItem`s; clicking a file asks the [`ViewerBus`] to open it in the
//! Viewer panel (gpui-kit's tree expands/collapses folder rows on click by
//! itself). The tree rebuilds when `state.scan` changes; the folders the
//! user explicitly expanded/collapsed are remembered across rebuilds.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{Icon, IconName, h_flex};
use gpui_kit::*;

use deerpunk_studio::assets::AssetEntry;

use crate::state::StudioState;
use crate::theme;
use crate::viewers::ViewerBus;

pub struct FilesPanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    bus: Entity<ViewerBus>,
    tree: Entity<TreeState>,
    /// Tree-item id (rel path, lossy string) → path relative to the gamedir,
    /// used by click handlers to resolve the absolute file path.
    paths: Arc<HashMap<SharedString, PathBuf>>,
    /// Tree-item id → the user's explicit expansion toggle. Applied on every
    /// rebuild so folders stay opened/closed across rescans; ids whose paths
    /// no longer exist are dropped. Unlisted ids use the default (top level
    /// expanded, deeper levels collapsed).
    user_expanded: HashMap<SharedString, bool>,
    last_entries: Option<Vec<AssetEntry>>,
}

impl FilesPanel {
    pub fn new(state: Entity<StudioState>, bus: Entity<ViewerBus>, cx: &mut Context<Self>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        cx.observe(&state, |this, _, cx| this.sync_scan(cx))
            .detach();
        // The kit tree toggles folders on click and reports it; record the
        // user's intent so the next rebuild (new scan) re-applies it.
        cx.subscribe(&tree, |this, _, event, _| match event {
            TreeEvent::Expanded(id) => {
                this.user_expanded.insert(id.clone(), true);
            }
            TreeEvent::Collapsed(id) => {
                this.user_expanded.insert(id.clone(), false);
            }
        })
        .detach();
        Self {
            focus_handle: cx.focus_handle(),
            state,
            bus,
            tree,
            paths: Arc::default(),
            user_expanded: HashMap::new(),
            last_entries: None,
        }
    }

    /// Rebuild the tree when the scan changed. The pump notifies the state
    /// every 50 ms; the entries are compared (and only cloned on change) so
    /// steady-state notifications are cheap.
    fn sync_scan(&mut self, cx: &mut Context<Self>) {
        let changed = {
            let state = self.state.read(cx);
            state.scan.as_ref().map(|s| s.entries.as_slice()) != self.last_entries.as_deref()
        };
        if !changed {
            return;
        }
        let entries = self.state.read(cx).scan.as_ref().map(|s| s.entries.clone());
        let (items, paths) = build_tree(entries.as_deref(), &self.user_expanded);
        self.last_entries = entries;
        self.paths = Arc::new(paths);
        // Paths that vanished with the new scan drop their stale toggle.
        self.user_expanded.retain(|id, _| self.paths.contains_key(id));
        self.tree.update(cx, |tree, cx| tree.set_items(items, cx));
        cx.notify();
    }
}

/// Fold the flat scan list into a path tree of `TreeItem`s. Returns the
/// items plus the id → rel-path map used by click handlers. Top-level
/// folders start expanded, deeper ones collapsed; `user_expanded` overrides
/// the default per folder (see [`FilesPanel::new`]'s subscription).
fn build_tree(
    entries: Option<&[AssetEntry]>,
    user_expanded: &HashMap<SharedString, bool>,
) -> (Vec<TreeItem>, HashMap<SharedString, PathBuf>) {
    let Some(entries) = entries else {
        return (Vec::new(), HashMap::new());
    };
    let mut root = DirNode::default();
    for entry in entries {
        root.insert(&entry.rel);
    }
    let mut paths = HashMap::new();
    let items = root.to_items(Path::new(""), 0, user_expanded, &mut paths);
    (items, paths)
}

/// Intermediate path tree built from the flat scan list; directories keep
/// sorted order via `BTreeMap`, files keep the scan's sorted order.
#[derive(Default)]
struct DirNode {
    dirs: BTreeMap<String, DirNode>,
    files: Vec<String>,
}

impl DirNode {
    fn insert(&mut self, rel: &Path) {
        let mut comps = rel.components();
        let Some(first) = comps.next() else {
            return;
        };
        let name = first.as_os_str().to_string_lossy().into_owned();
        let rest: PathBuf = comps.collect();
        if rest.as_os_str().is_empty() {
            self.files.push(name);
        } else {
            self.dirs.entry(name).or_default().insert(&rest);
        }
    }

    fn to_items(
        &self,
        prefix: &Path,
        depth: usize,
        user_expanded: &HashMap<SharedString, bool>,
        paths: &mut HashMap<SharedString, PathBuf>,
    ) -> Vec<TreeItem> {
        let mut items = Vec::new();
        for (name, node) in &self.dirs {
            let rel = prefix.join(name);
            let id = id_of(&rel, paths);
            let children = node.to_items(&rel, depth + 1, user_expanded, paths);
            let expanded = user_expanded.get(&id).copied().unwrap_or(depth == 0);
            items.push(TreeItem::new(id, name.clone()).expanded(expanded).children(children));
        }
        for name in &self.files {
            let rel = prefix.join(name);
            let id = id_of(&rel, paths);
            items.push(TreeItem::new(id, name.clone()));
        }
        items
    }
}

/// Stable tree-item id for `rel`, recording the exact rel path for clicks.
fn id_of(rel: &Path, paths: &mut HashMap<SharedString, PathBuf>) -> SharedString {
    let id: SharedString = rel.to_string_lossy().into_owned().into();
    paths.entry(id.clone()).or_insert_with(|| rel.to_path_buf());
    id
}

impl EventEmitter<PanelEvent> for FilesPanel {}

impl Focusable for FilesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for FilesPanel {
    fn panel_name(&self) -> &'static str {
        "FilesPanel"
    }
}

impl Panel for FilesPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Files"
    }
}

impl Render for FilesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.last_entries.is_none() {
            return div()
                .size_full()
                .p_3()
                .text_color(theme::TEXT_DIM)
                .child("no scan — open a project");
        }
        let gamedir = self
            .state
            .read(cx)
            .project
            .as_ref()
            .map(|p| p.gamedir.clone());
        let bus = self.bus.clone();
        let paths = self.paths.clone();
        div().size_full().child(tree(
            &self.tree,
            move |ix, entry, _selected, _window, _cx| {
                let id = entry.item().id.clone();
                let label = entry.item().label.clone();
                let depth = entry.depth();
                let folder = entry.is_folder();
                let expanded = entry.is_expanded();
                let icon = if folder {
                    Icon::new(if expanded {
                        IconName::FolderOpen
                    } else {
                        IconName::FolderClosed
                    })
                    .text_color(theme::ACCENT)
                    .size(px(12.5))
                } else {
                    Icon::new(IconName::File)
                        .text_color(theme::TEXT_DIM)
                        .size(px(12.5))
                };
                // Compact editor rows: 12.5px label, tight padding, small
                // indent steps. Refinements override ListItem's built-in
                // `py_1().px_3().text_base()` (applied via `refine_style`).
                let selector_id = id.clone();
                // Headless tests look rows up by rel path; the selector is a
                // no-op without gpui's test-support. `id` itself is still
                // needed below for the file-click handler.
                let mut row = ListItem::new(("files-row", ix))
                    .text_size(px(12.5))
                    .py_0p5()
                    .px_2()
                    .debug_selector(move || format!("files-row-{}", selector_id))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .pl(px(4. + depth as f32 * 10.))
                            .child(icon)
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(label),
                            ),
                    );
                if !folder {
                    let bus = bus.clone();
                    let rel = paths.get(&id).cloned();
                    let gamedir = gamedir.clone();
                    row = row.on_click(move |_, _, cx| {
                        if let (Some(gamedir), Some(rel)) = (gamedir.clone(), rel.clone()) {
                            bus.update(cx, |bus, cx| bus.open(gamedir.join(rel), cx));
                        }
                    });
                }
                row
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::files::build_tree;

    use deerpunk_studio::assets::{AssetEntry, AssetKind};
    use gpui_kit::SharedString;

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    fn entry(rel: &str) -> AssetEntry {
        AssetEntry {
            rel: PathBuf::from(rel),
            kind: AssetKind::Texture,
            size: 1,
            pack: None,
        }
    }

    /// A flat scan list folds into nested tree items: directories first,
    /// files second, top-level dirs expanded, deeper dirs collapsed, and
    /// every item id maps back to its exact rel path.
    #[test]
    fn flat_scan_becomes_path_tree() {
        let entries = [
            entry("a.txt"),
            entry("textures/blocks/stone.png"),
            entry("textures/ui.png"),
        ];
        let (items, paths) = build_tree(Some(&entries), &HashMap::new());

        let [textures, a] = items.as_slice() else {
            panic!("expected [textures, a.txt], got {} items", items.len());
        };
        assert_eq!(textures.label, "textures");
        assert_eq!(a.label, "a.txt");
        assert!(textures.is_expanded());

        let [blocks, ui] = textures.children.as_slice() else {
            panic!("expected [blocks, ui.png] under textures");
        };
        assert_eq!(blocks.label, "blocks");
        assert_eq!(ui.label, "ui.png");
        assert!(!blocks.is_expanded());

        let [stone] = blocks.children.as_slice() else {
            panic!("expected [stone.png] under blocks");
        };
        assert_eq!(stone.label, "stone.png");

        for (id, rel) in [("a.txt", "a.txt"), ("textures/ui.png", "textures/ui.png")] {
            assert_eq!(
                paths.get(SharedString::from(id).as_ref()).map(Path::new),
                Some(Path::new(rel)),
                "id `{id}` must map to rel `{rel}`"
            );
        }
    }

    /// No scan yields no items.
    #[test]
    fn missing_scan_yields_empty_tree() {
        let (items, paths) = build_tree(None, &HashMap::new());
        assert!(items.is_empty());
        assert!(paths.is_empty());
    }

    /// An explicit user toggle overrides the default for its folder: a
    /// deeper folder marked expanded opens, a top-level folder marked
    /// collapsed stays shut. The override is keyed by rel path and leaves
    /// other folders at their defaults.
    #[test]
    fn user_toggles_override_default_expansion() {
        let entries = [
            entry("a.txt"),
            entry("textures/blocks/stone.png"),
            entry("textures/ui.png"),
        ];
        let expanded = [(
            SharedString::from("textures/blocks"),
            true,
        )]
        .into_iter()
        .collect();
        let (items, _) = build_tree(Some(&entries), &expanded);
        let [textures, _] = items.as_slice() else {
            panic!("expected [textures, a.txt]");
        };
        let [blocks, _] = textures.children.as_slice() else {
            panic!("expected [blocks, ui.png] under textures");
        };
        assert!(blocks.is_expanded(), "user-expanded deeper folder opens");

        let collapsed = [(SharedString::from("textures"), false)]
            .into_iter()
            .collect();
        let (items, _) = build_tree(Some(&entries), &collapsed);
        let [textures, _] = items.as_slice() else {
            panic!("expected [textures, a.txt]");
        };
        assert!(!textures.is_expanded(), "user-collapsed top folder stays shut");
    }
}
