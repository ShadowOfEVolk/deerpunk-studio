//! Cvars panel (center tab): the project's `cvars_catalog.json` as a
//! searchable, category-filtered virtualized table. Ported from the egui
//! view (src/panels.rs `cvars_sidebar` + `cvars_center`). The egui version
//! re-read and re-parsed the catalog every frame; here it is cached in the
//! panel and re-read only when `state.project` changes. The search input on
//! top is new in this port (egui kept `app.cvars_filter` state but never
//! rendered an editor for it); the left category list carries the egui
//! sidebar over (`All` + one row per category with its count).

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Sizable, Size, h_flex, v_flex};
use gpui_kit::*;

use serde_json::Value;

use crate::fa;
use crate::state::StudioState;
use crate::theme;

/// Catalog location inside the gamedir (egui parity).
const CATALOG_REL: &str = "config/cvars_catalog.json";
/// Row density: a fixed 40px row holds the two-line label/description cell
/// (`Size::Size` → 4px cell padding → 32px of content for 12.5px + 11px
/// lines at 1.2 line height).
const ROW_HEIGHT: Pixels = px(40.);
/// Compact editor text sizes (the files/console band).
const CELL_FONT_SIZE: f32 = 12.5;
const DESCRIPTION_FONT_SIZE: f32 = 11.;
/// Width of the folded-in egui sidebar (category list).
const CATEGORY_COL_WIDTH: f32 = 192.;

/// One `cvars_catalog.json` entry, defaulted like the egui port
/// (`entry.get(..).and_then(Value::as_str).unwrap_or("")`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CvarEntry {
    name: String,
    category: String,
    ty: String,
    default: String,
    label: String,
    description: String,
}

/// Catalog load outcome surfaced in the content area (egui center messages).
#[derive(Clone, Debug)]
enum CatalogError {
    /// File absent — egui `no cvars_catalog.json at {path}`.
    Missing(PathBuf),
    /// File present but not valid JSON — egui `cvars_catalog.json: parse
    /// error` (valid JSON without an `entries` array is an empty catalog,
    /// not an error, exactly like the egui `unwrap_or_default`).
    Parse,
}

pub struct CvarsPanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    /// Cached catalog: entries parsed once per gamedir (the egui port
    /// re-read the file every frame), re-read when the project changes.
    entries: Arc<Vec<CvarEntry>>,
    /// Category → count, insertion-ordered then sorted (egui
    /// `cvars_sidebar` aggregation). `All` is implicit.
    categories: Vec<(String, usize)>,
    error: Option<CatalogError>,
    /// Gamedir the cache was loaded for; a change reloads the catalog.
    loaded_gamedir: Option<PathBuf>,
    /// Category filter, `None` = All (egui `app.cvars_category`).
    category: Option<String>,
    /// Name substring filter, case-insensitive (egui `app.cvars_filter`).
    filter: String,
    /// Search input; created lazily in `render` (needs `Window`).
    input: Option<Entity<InputState>>,
    /// Virtualized table; created lazily in `render` (needs `Window`).
    table: Option<Entity<TableState<CvarsDelegate>>>,
}

impl CvarsPanel {
    pub fn new(state: Entity<StudioState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |this, _, cx| this.sync_project(cx))
            .detach();
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            state,
            entries: Arc::default(),
            categories: Vec::new(),
            error: None,
            loaded_gamedir: None,
            category: None,
            filter: String::new(),
            input: None,
            table: None,
        };
        this.reload(cx);
        this
    }

    /// Reload the catalog when the observed project changed (the pump
    /// notifies every 50 ms; the gamedir comparison makes steady-state
    /// notifications free).
    fn sync_project(&mut self, cx: &mut Context<Self>) {
        let gamedir = self
            .state
            .read(cx)
            .project
            .as_ref()
            .map(|p| p.gamedir.clone());
        if gamedir != self.loaded_gamedir {
            self.reload(cx);
            cx.notify();
        }
    }

    /// Read + parse `config/cvars_catalog.json` from the project gamedir and
    /// cache the result. Mirrors the egui outcomes: no project → empty;
    /// missing file / broken JSON → the egui error messages.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let project = self.state.read(cx).project.clone();
        let gamedir = project.as_ref().map(|p| p.gamedir.clone());
        self.loaded_gamedir = gamedir;

        let (entries, error) = match project.as_ref() {
            None => (Vec::new(), None),
            Some(project) => {
                let path = project.gamedir.join(CATALOG_REL);
                match std::fs::read_to_string(&path) {
                    Err(_) => (Vec::new(), Some(CatalogError::Missing(path))),
                    Ok(text) => match parse_catalog(&text) {
                        Err(()) => (Vec::new(), Some(CatalogError::Parse)),
                        Ok(entries) => (entries, None),
                    },
                }
            }
        };

        self.entries = Arc::new(entries);
        self.categories = collect_categories(&self.entries);
        self.error = error;
        self.sync_table(cx);
    }

    /// Push the cached catalog + current filters into the table delegate.
    fn sync_table(&mut self, cx: &mut Context<Self>) {
        let Some(table) = &self.table else {
            return;
        };
        let entries = self.entries.clone();
        let filter = self.filter.clone();
        let category = self.category.clone();
        table.update(cx, |table, cx| {
            table
                .delegate_mut()
                .set_data(entries, &filter, category.as_deref());
            table.refresh(cx);
            cx.notify();
        });
    }

    fn set_filter(&mut self, filter: String, cx: &mut Context<Self>) {
        if filter == self.filter {
            return;
        }
        self.filter = filter;
        self.sync_table(cx);
    }

    fn set_category(&mut self, category: Option<String>, cx: &mut Context<Self>) {
        if self.category == category {
            return;
        }
        self.category = category;
        self.sync_table(cx);
        cx.notify();
    }

    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("filter…"));
        cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                let value = input.read(cx).value().to_string();
                this.set_filter(value, cx);
            }
        })
        .detach();
        self.input = Some(input.clone());
        input
    }

    fn ensure_table(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TableState<CvarsDelegate>> {
        if let Some(table) = &self.table {
            return table.clone();
        }
        let delegate =
            CvarsDelegate::new(self.entries.clone(), &self.filter, self.category.as_deref());
        // egui grid parity: rows are display-only (no selection, no sorting,
        // fixed order); columns stay resizable.
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .row_selectable(false)
                .sortable(false)
                .col_movable(false)
        });
        self.table = Some(table.clone());
        table
    }

    /// The loaded-catalog layout: category list on the left, table on the
    /// right (the egui sidebar + center grid, merged into one panel).
    fn catalog_area(
        &self,
        table: Entity<TableState<CvarsDelegate>>,
        panel: Entity<Self>,
    ) -> AnyElement {
        let categories = self.categories.clone();
        let selected = self.category.clone();
        let total = self.entries.len();

        h_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                v_flex()
                    .w(px(CATEGORY_COL_WIDTH))
                    .h_full()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(theme::BORDER)
                    .child(
                        uniform_list(
                            "cvars-categories",
                            categories.len() + 1,
                            move |range, _, _| {
                                range
                                    .map(|ix| {
                                        category_row(
                                            ix,
                                            &categories,
                                            total,
                                            selected.as_deref(),
                                            &panel,
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            },
                        )
                        .flex_1()
                        .min_h_0(),
                    ),
            )
            .child(
                v_flex().flex_1().min_w_0().overflow_hidden().p_1().child(
                    DataTable::new(&table)
                        .stripe(true)
                        .bordered(false)
                        .with_size(Size::Size(ROW_HEIGHT)),
                ),
            )
            .into_any_element()
    }
}

/// One category sidebar row (egui `sidebar_row` + count suffix). `ix == 0`
/// is the implicit `All` row; clicking toggles the filter.
fn category_row(
    ix: usize,
    categories: &[(String, usize)],
    total: usize,
    selected: Option<&str>,
    panel: &Entity<CvarsPanel>,
) -> ListItem {
    let (label, count, filter) = if ix == 0 {
        ("All".to_string(), total, None)
    } else {
        match categories.get(ix - 1) {
            Some((name, count)) => (name.clone(), *count, Some(name.as_str())),
            None => return ListItem::new(("cvars-category", ix)),
        }
    };
    let is_selected = selected == filter;

    let item = ListItem::new(("cvars-category", ix))
        .selected(is_selected)
        .text_size(px(CELL_FONT_SIZE))
        .py_0p5()
        .px_2()
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(if is_selected {
                            theme::TEXT
                        } else {
                            theme::TEXT_DIM
                        })
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::FAINT)
                        .child(count.to_string()),
                ),
        );
    let panel = panel.clone();
    let filter = filter.map(str::to_string);
    item.on_click(move |_, _, cx| {
        panel.update(cx, |this, cx| this.set_category(filter.clone(), cx))
    })
}

/// Catalog table delegate: cached entries + the filtered row index list.
struct CvarsDelegate {
    entries: Arc<Vec<CvarEntry>>,
    /// Indices into `entries` passing both filters (egui center-grid loop).
    rows: Vec<usize>,
}

impl CvarsDelegate {
    fn new(entries: Arc<Vec<CvarEntry>>, filter: &str, category: Option<&str>) -> Self {
        Self {
            rows: build_rows(&entries, filter, category),
            entries,
        }
    }

    fn set_data(&mut self, entries: Arc<Vec<CvarEntry>>, filter: &str, category: Option<&str>) {
        self.rows = build_rows(&entries, filter, category);
        self.entries = entries;
    }

    fn entry(&self, row_ix: usize) -> Option<&CvarEntry> {
        self.rows.get(row_ix).and_then(|ix| self.entries.get(*ix))
    }
}

impl TableDelegate for CvarsDelegate {
    /// egui grid order: name, category, type, default, label+description.
    fn columns_count(&self, _: &App) -> usize {
        5
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("name", "Name")
                .width(px(220.))
                .min_width(px(90.)),
            1 => Column::new("category", "Category")
                .width(px(120.))
                .min_width(px(60.)),
            2 => Column::new("type", "Type")
                .width(px(90.))
                .min_width(px(48.)),
            3 => Column::new("default", "Default")
                .width(px(140.))
                .min_width(px(60.)),
            _ => Column::new("label", "Label / Description")
                .width(px(560.))
                .min_width(px(160.)),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(entry) = self.entry(row_ix) else {
            return div().into_any_element();
        };
        let mono_font = cx.theme().mono_font_family.clone();
        match col_ix {
            // egui: `ui.monospace(name)` / `ui.monospace(default)`.
            0 | 3 => div()
                .font_family(mono_font)
                .text_size(px(CELL_FONT_SIZE))
                .text_color(theme::TEXT)
                .child(if col_ix == 0 {
                    entry.name.clone()
                } else {
                    entry.default.clone()
                })
                .into_any_element(),
            // egui: `ui.weak(category)` / `ui.weak(ty)`.
            1 | 2 => div()
                .text_size(px(CELL_FONT_SIZE))
                .text_color(theme::TEXT_DIM)
                .child(if col_ix == 1 {
                    entry.category.clone()
                } else {
                    entry.ty.clone()
                })
                .into_any_element(),
            // egui: label + weak description stacked in one cell.
            _ => {
                let mut col = v_flex().min_w_0();
                if !entry.label.is_empty() {
                    col = col.child(
                        div()
                            .text_size(px(CELL_FONT_SIZE))
                            .line_height(relative(1.2))
                            .text_color(theme::TEXT)
                            .child(entry.label.clone()),
                    );
                }
                if !entry.description.is_empty() {
                    col = col.child(
                        div()
                            .text_size(px(DESCRIPTION_FONT_SIZE))
                            .line_height(relative(1.2))
                            .text_color(theme::TEXT_DIM)
                            .child(entry.description.clone()),
                    );
                }
                col.into_any_element()
            }
        }
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let entry = self.entry(row_ix)?;
        let text = match col_ix {
            0 => &entry.name,
            1 => &entry.category,
            2 => &entry.ty,
            3 => &entry.default,
            _ if !entry.label.is_empty() => &entry.label,
            _ => &entry.description,
        };
        (!text.is_empty()).then(|| text.clone().into())
    }
}

impl EventEmitter<PanelEvent> for CvarsPanel {}

impl Focusable for CvarsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for CvarsPanel {
    fn panel_name(&self) -> &'static str {
        "CvarsPanel"
    }
}

impl Panel for CvarsPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Cvars"
    }
}

impl Render for CvarsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_project = self.state.read(cx).project.is_some();

        // Window-dependent children are created lazily (console-panel
        // pattern: `InputState`/`TableState` both need `&mut Window`).
        let input = has_project.then(|| self.ensure_input(window, cx));
        let table = has_project.then(|| self.ensure_table(window, cx));
        let panel = cx.entity();
        let error = self.error.clone();

        // egui `sidebar_header`: title left, entry count right (0 when the
        // catalog is missing), "no project" without one.
        let subtitle: SharedString = if has_project {
            self.entries.len().to_string().into()
        } else {
            "no project".into()
        };

        let content: AnyElement = if !has_project {
            centered_text("no project open")
        } else if let Some(CatalogError::Missing(path)) = &error {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_color(theme::FAINT)
                        .child(fa::sized(fa::DATABASE, px(20.))),
                )
                .text_color(theme::TEXT_DIM)
                .child(format!("no cvars_catalog.json at {}", path.display()))
                .into_any_element()
        } else if let Some(CatalogError::Parse) = &error {
            div()
                .flex_1()
                .min_h_0()
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .text_color(theme::YELLOW)
                        .child(fa::sized(fa::CIRCLE_EXCLAMATION, px(12.)))
                        .child("cvars_catalog.json: parse error"),
                )
                .into_any_element()
        } else {
            self.catalog_area(table.expect("table created above"), panel)
        };

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .border_b_1()
                    .border_color(theme::BORDER)
                    .child(
                        div()
                            .text_color(theme::ACCENT)
                            .child(fa::sized(fa::SLIDERS, px(13.))),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::TEXT)
                            .child("Cvars"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::TEXT_DIM)
                            .child(subtitle),
                    ),
            )
            .children(input.map(|input| {
                h_flex().px_2().py_1().child(
                    Input::new(&input)
                        .with_size(Size::Small)
                        .cleanable(true)
                        .w_full(),
                )
            }))
            .child(content)
    }
}

/// egui `ui.centered_and_justified(|ui| ui.weak(..))`.
fn centered_text(text: impl Into<SharedString>) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme::TEXT_DIM)
        .child(text.into())
        .into_any_element()
}

/// Parse the catalog exactly like the egui center view: broken JSON is an
/// error; valid JSON without an `entries` array is an empty catalog.
fn parse_catalog(text: &str) -> Result<Vec<CvarEntry>, ()> {
    let value: Value = serde_json::from_str(text).map_err(|_| ())?;
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(entries.iter().map(entry_from).collect())
}

/// `entry.get(..).and_then(Value::as_str).unwrap_or("")` per field.
fn entry_from(value: &Value) -> CvarEntry {
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    CvarEntry {
        name: field("name"),
        category: field("category"),
        ty: field("type"),
        default: field("default"),
        label: field("label"),
        description: field("description"),
    }
}

/// egui `cvars_sidebar` aggregation: per-category counts in insertion order,
/// then sorted by name. The `All` row (total count) is implicit.
fn collect_categories(entries: &[CvarEntry]) -> Vec<(String, usize)> {
    let mut categories: Vec<(String, usize)> = Vec::new();
    for entry in entries {
        if let Some(slot) = categories.iter_mut().find(|(c, _)| c == &entry.category) {
            slot.1 += 1;
        } else {
            categories.push((entry.category.clone(), 1));
        }
    }
    categories.sort();
    categories
}

/// The egui center-grid filter loop as an index list: category exact match +
/// case-insensitive substring on the name (empty filter matches all).
fn build_rows(entries: &[CvarEntry], filter: &str, category: Option<&str>) -> Vec<usize> {
    let filter = filter.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            category.is_none_or(|want| entry.category == want)
                && (filter.is_empty() || entry.name.to_lowercase().contains(&filter))
        })
        .map(|(ix, _)| ix)
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::cvars::{CvarEntry, build_rows, collect_categories, parse_catalog};

    fn e(name: &str, category: &str) -> CvarEntry {
        CvarEntry {
            name: name.into(),
            category: category.into(),
            ..Default::default()
        }
    }

    /// Fields fall back to "" like the egui `unwrap_or("")` reads.
    #[test]
    fn catalog_parses_entries_with_defaulted_fields() {
        let text = r#"{"entries":[
            {"name":"sv_gravity","category":"physics","type":"float",
             "default":"800.0","label":"Gravity","description":"World gravity"},
            {"name":"ui_scale"}
        ]}"#;
        let entries = parse_catalog(text).expect("valid catalog");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            CvarEntry {
                name: "sv_gravity".into(),
                category: "physics".into(),
                ty: "float".into(),
                default: "800.0".into(),
                label: "Gravity".into(),
                description: "World gravity".into(),
            }
        );
        assert_eq!(entries[1].name, "ui_scale");
        assert_eq!(entries[1].category, "");
        assert_eq!(entries[1].default, "");
    }

    /// egui parity: valid JSON without `entries` is an empty catalog, not an
    /// error; broken JSON is.
    #[test]
    fn catalog_without_entries_is_empty_not_error() {
        assert!(parse_catalog(r#"{"other":1}"#).unwrap().is_empty());
        assert!(parse_catalog("not json").is_err());
    }

    /// egui `cvars_sidebar`: insertion-order aggregation, then sorted by
    /// category name.
    #[test]
    fn categories_aggregate_and_sort() {
        let entries = [e("b", "game"), e("a", ""), e("c", "game"), e("d", "render")];
        assert_eq!(
            collect_categories(&entries),
            vec![
                (String::new(), 1),
                ("game".to_string(), 2),
                ("render".to_string(), 1),
            ]
        );
    }

    /// egui `cvars_center` filtering: category exact match + case-insensitive
    /// substring on the name.
    #[test]
    fn rows_filter_by_category_and_name_substring() {
        let entries = [
            e("sv_gravity", "physics"),
            e("sv_airaccel", "physics"),
            e("ui_scale", "ui"),
        ];
        assert_eq!(build_rows(&entries, "", None), vec![0, 1, 2]);
        assert_eq!(build_rows(&entries, "", Some("ui")), vec![2]);
        assert_eq!(build_rows(&entries, "SV_AIR", Some("physics")), vec![1]);
        assert!(build_rows(&entries, "missing", None).is_empty());
    }
}
