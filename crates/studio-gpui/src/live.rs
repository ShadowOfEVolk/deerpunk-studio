//! Live inspector panel (left sidebar + detail host): collection picker,
//! the active collection's entity list, and the selected entity's detail
//! view. Ported from the egui Live tab — src/panels.rs
//! `collection_sidebar`/`entity_row`/`prefab_glyph`/`name_or_first` and the
//! `detail_panel` scaffolding; src/app.rs `select` + the
//! `ensure_active_list` lazy fetch (activating a collection fetches
//! immediately when the cached list is stale instead of waiting for the
//! refresh timer). The detail view itself is `crate::live_detail::detail`;
//! this panel only hosts it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use serde_json::Value;

use deerpunk_studio::net::ConnState;
use deerpunk_studio::protocol::{self, CollectionDescriptor, CollectionMode};

use crate::fa;
use crate::state::StudioState;
use crate::theme;

/// Sidebar column width (egui `Panel::left("sidebar").default_size(250.0)`).
const SIDEBAR_W: f32 = 250.;
/// Entity row heights: two lines (name + position) vs a single name line
/// (egui entity_row: 30 / 20). `uniform_list` lays every row out with one
/// measured height, so the tall variant applies to the whole list.
const ROW_TALL: f32 = 30.;
const ROW_PLAIN: f32 = 20.;
/// Row text density, matching the files panel rows (egui used 13 px names).
const NAME_SIZE: f32 = 12.5;
/// Monospace "x y z" line under the entity name (egui: monospace 10).
const POS_SIZE: f32 = 10.;
/// Small header/pill text (egui `theme::small`).
const LABEL_SIZE: f32 = 11.;

// FA6 solid glyphs not yet exported by `crate::fa`, verified against the
// embedded font's cmap (candidates to migrate into fa.rs).
const SEARCH: char = '\u{f002}';
const USER: char = '\u{f007}';
const PACKAGE: char = '\u{f487}';
const IMAGE: char = '\u{f03e}';
const CUBE: char = '\u{f1b2}';
const STACK: char = '\u{e4e3}';
const USERS: char = '\u{f0c0}';
const GAUGE: char = '\u{f625}';
const MUSIC: char = '\u{f001}';
const LIST: char = '\u{f0ca}';

/// Fallback glyph for entities whose prefab has no texture icon: a couple of
/// well-known prefabs get a fitting glyph, everything else a cube (egui
/// `prefab_glyph`, Phosphor → FA6 solid).
fn prefab_glyph(prefab: &str) -> char {
    match prefab {
        "player" => USER,
        "item_drop" => PACKAGE,
        "main_menu" | "loading_screen" => IMAGE,
        _ => CUBE,
    }
}

/// Collection glyph — FA analogs of the egui rail's Phosphor `live_glyph`.
fn collection_glyph(id: &str) -> char {
    match id {
        "entities" => CUBE,
        "assets" => STACK,
        "players" => USERS,
        "systems" => GAUGE,
        "fmod_events" => MUSIC,
        "methoddb" => LIST,
        _ => fa::DATABASE,
    }
}

/// Space-joined mode names of a collection ("list get …").
fn mode_names(c: &CollectionDescriptor) -> String {
    c.modes
        .iter()
        .map(|m| match m {
            CollectionMode::List => "list",
            CollectionMode::Get => "get",
            CollectionMode::Stream => "stream",
            CollectionMode::Catalog => "catalog",
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn name_or_first(name: &str, first: &str) -> String {
    if name.is_empty() {
        first.to_string()
    } else {
        name.to_string()
    }
}

/// One visible row's precomputed render data (the egui sidebar recomputed
/// these per frame too).
struct RowModel {
    /// Click index, stringified cell of the collection's `id_field`.
    index_str: String,
    display_name: String,
    /// Classname: prefabId through `prefab_map`, legacy `prefab` string.
    classname: Option<String>,
    position: Option<[f32; 3]>,
    /// Index into [`SidebarData::raw`]; non-get collections stash the whole
    /// row as the detail on click (egui `app.detail = Some(row.clone())`).
    raw_ix: usize,
}

/// Everything the sidebar renders for the active collection's list.
struct SidebarData {
    label: String,
    get_mode: bool,
    /// Unfiltered row count — the header badge (egui showed `rows.len()`).
    total: usize,
    models: Vec<RowModel>,
    raw: Vec<Value>,
}

/// Visible rows for a collection: display data resolved like the egui
/// sidebar (first cell, display name through the prefab map, `id_field`
/// click index, position) with the filter applied over
/// `{first} {name} {display_name}`. `None` when the collection has no
/// cached list yet (egui's early-return header branch).
fn snapshot(state: &StudioState, id: &str, filter: &str) -> Option<SidebarData> {
    let result = state.lists.get(id)?;
    let desc = state.collections.iter().find(|c| c.id == id);
    let rows = protocol::list_rows(result);
    let keys = protocol::row_keys(&rows);
    let id_field = desc.map(|c| c.id_field.as_str()).unwrap_or("index");
    let lowered = filter.to_lowercase();

    let mut models = Vec::new();
    let mut raw = Vec::new();
    for row in rows.iter() {
        let first = keys
            .first()
            .and_then(|k| row.get(k.as_str()))
            .map(protocol::cell)
            .unwrap_or_default();
        let name = row.get("name").and_then(Value::as_str).unwrap_or("");
        let index_str = row.get(id_field).map(protocol::cell).unwrap_or_default();
        let classname = protocol::row_prefab_id(row)
            .and_then(|pid| state.prefab_map.get(&pid).cloned())
            .or_else(|| protocol::row_prefab(row));
        let display_name = classname
            .clone()
            .unwrap_or_else(|| name_or_first(name, &first));
        if !lowered.is_empty()
            && !format!("{first} {name} {display_name}")
                .to_lowercase()
                .contains(&lowered)
        {
            continue;
        }
        models.push(RowModel {
            index_str,
            display_name,
            classname,
            position: protocol::row_position(row),
            raw_ix: raw.len(),
        });
        raw.push(row.clone());
    }
    Some(SidebarData {
        label: desc
            .map(|c| c.label.clone())
            .unwrap_or_else(|| id.to_string()),
        get_mode: desc
            .map(|c| c.modes.contains(&CollectionMode::Get))
            .unwrap_or(false),
        total: rows.len(),
        models,
        raw,
    })
}

/// Activate a collection — the egui rail-switch path: set the active id and
/// fetch immediately when the cached list is stale (`ensure_active_list`).
/// `state.ensure_active_list` is private to state.rs, so the stale check is
/// inlined here against the same fields.
fn activate(state: &Entity<StudioState>, id: &str, cx: &mut App) {
    state.update(cx, |s, _| {
        s.active_collection = Some(id.to_string());
        let stale = match s.last_fetch.get(id) {
            None => true,
            Some(t) => s.refresh_ms > 0 && t.elapsed() >= Duration::from_millis(s.refresh_ms),
        };
        if stale {
            s.request_refresh();
        }
    });
}

pub struct LivePanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    /// Single-line filter input; created lazily in `render` (needs `Window`).
    filter_input: Option<Entity<InputState>>,
    /// Per-collection filter text (egui `app.filters[id]`; the gpui
    /// `StudioState` has no filters field, so it lives on the panel).
    filters: HashMap<String, String>,
    /// Collection the filter input currently mirrors, so `set_value` fires
    /// only on a collection switch (a repeated set would notify forever).
    filter_col: Option<String>,
}

impl LivePanel {
    pub fn new(state: Entity<StudioState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self {
            focus_handle: cx.focus_handle(),
            state,
            filter_input: None,
            filters: HashMap::new(),
            filter_col: None,
        }
    }

    fn ensure_filter_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = &self.filter_input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("filter…"));
        cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    let value = input.read(cx).value().to_string();
                    let col = this.state.read(cx).active_collection.clone();
                    if let Some(col) = col {
                        this.filters.insert(col, value);
                    }
                    cx.notify();
                }
            },
        )
        .detach();
        self.filter_input = Some(input.clone());
        input
    }

    /// Filter box (egui `filter_box`): magnifier glyph + single-line input
    /// whose value mirrors the active collection's filter.
    fn filter_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let input = self.ensure_filter_input(window, cx);
        let col = self.state.read(cx).active_collection.clone();
        if self.filter_col != col {
            let value = col
                .as_ref()
                .and_then(|id| self.filters.get(id).cloned())
                .unwrap_or_default();
            input.update(cx, |st, cx| st.set_value(value, window, cx));
            self.filter_col = col;
        }
        h_flex()
            .items_center()
            .gap_1p5()
            .child(
                div()
                    .text_color(theme::MUTED)
                    .child(fa::sized(SEARCH, px(11.))),
            )
            .child(Input::new(&input).flex_1().min_w_0())
            .into_any_element()
    }

    /// The whole sidebar: header + pills + filter + rows for the active
    /// collection, the collection picker when none is active (egui's rail
    /// buttons owned collection switching; the gpui panel owns it).
    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        let (collections, active, connected, data) = {
            let st = self.state.read(cx);
            let data = st.active_collection.as_deref().and_then(|id| {
                let filter = self.filters.get(id).cloned().unwrap_or_default();
                snapshot(st, id, &filter)
            });
            (
                st.collections.clone(),
                st.active_collection.clone(),
                st.conn_state == ConnState::Connected,
                data,
            )
        };

        let mut sidebar = v_flex()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex_shrink_0()
            .bg(theme::PANEL)
            .px_2()
            .py_1p5()
            .gap_1p5()
            .overflow_hidden();

        match (active.as_deref(), data) {
            // Cached list: header(label, count) + pills + filter + rows.
            (Some(id), Some(d)) => {
                sidebar = sidebar.child(header(&d.label, &d.total.to_string()));
                if collections.len() > 1 {
                    sidebar = sidebar.child(pills(&collections, active.as_deref(), &self.state));
                }
                sidebar = sidebar.child(self.filter_row(window, cx));
                sidebar = sidebar.child(rows(id, d, mono, self.state.clone()));
            }
            // Active but not fetched yet: egui's early-return header branch
            // (id as the title, connection status as the badge).
            (Some(id), None) => {
                sidebar = sidebar.child(header(
                    id,
                    if connected {
                        "no data"
                    } else {
                        "not connected"
                    },
                ));
                if collections.len() > 1 {
                    sidebar = sidebar.child(pills(&collections, active.as_deref(), &self.state));
                }
            }
            // No active collection: the picker replaces the egui rail.
            (None, _) => {
                if collections.is_empty() {
                    sidebar = sidebar
                        .child(header(
                            "Live",
                            if connected {
                                "no collections"
                            } else {
                                "not connected"
                            },
                        ))
                        .child(hint(if connected {
                            "the inspector reports no collections"
                        } else {
                            "connect to the inspector to browse collections"
                        }));
                } else {
                    sidebar = sidebar
                        .child(header("Live", "select a collection"))
                        .child(picker(&collections, &self.state, mono));
                }
            }
        }
        sidebar.into_any_element()
    }
}

impl EventEmitter<PanelEvent> for LivePanel {}

impl Focusable for LivePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for LivePanel {
    fn panel_name(&self) -> &'static str {
        "LivePanel"
    }
}

impl Panel for LivePanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Live"
    }
}

impl Render for LivePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = self.render_sidebar(window, cx);
        // Detail host: brother-agent's `live_detail::detail` view, or the
        // egui placeholder while nothing is selected.
        let detail = {
            let st = self.state.read(cx);
            st.detail.clone().map(|d| (d, st.prefab_map.clone()))
        };
        let detail_area: AnyElement = match detail {
            Some((d, prefab_map)) => div()
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_hidden()
                .child(crate::live_detail::detail(&d, &prefab_map, window, cx))
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::FAINT)
                .text_size(px(NAME_SIZE))
                .child("Select an item")
                .into_any_element(),
        };

        div()
            .flex()
            .size_full()
            .bg(theme::BG)
            .child(sidebar)
            .child(Separator::vertical())
            .child(detail_area)
    }
}

/// Section header (egui `sidebar_header`): small title, muted badge.
fn header(title: &str, right: &str) -> Div {
    h_flex()
        .justify_between()
        .items_center()
        .child(
            div()
                .text_size(px(LABEL_SIZE))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::TEXT)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(LABEL_SIZE))
                .text_color(theme::MUTED)
                .child(right.to_string()),
        )
}

fn hint(text: &str) -> Div {
    div()
        .text_size(px(LABEL_SIZE))
        .text_color(theme::FAINT)
        .child(text.to_string())
}

/// Compact collection switcher, shown when more than one collection exists
/// (the egui rail had one button per collection; here they are pills).
fn pills(
    collections: &[CollectionDescriptor],
    active: Option<&str>,
    state: &Entity<StudioState>,
) -> AnyElement {
    let mut bar = h_flex().flex_wrap().gap_1();
    for (ix, c) in collections.iter().enumerate() {
        let on = active == Some(c.id.as_str());
        let (bg, fg) = if on {
            (theme::ACCENT, theme::ACCENT_TEXT)
        } else {
            (theme::PANEL_ALT, theme::TEXT_DIM)
        };
        let mut pill = div()
            .id(("live-col", ix))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap_1()
            .px_1p5()
            .py_0p5()
            .rounded(px(3.))
            .bg(bg)
            .text_color(fg)
            .text_size(px(LABEL_SIZE))
            .child(fa::sized(collection_glyph(&c.id), px(10.)))
            .child(c.label.clone());
        if !on {
            pill = pill.hover(|s| s.bg(theme::HOVER));
        }
        pill = pill.on_click({
            let state = state.clone();
            let id = c.id.clone();
            move |_, _, cx| activate(&state, &id, cx)
        });
        bar = bar.child(pill);
    }
    bar.into_any_element()
}

/// The no-active-collection picker (replaces the egui rail buttons).
fn picker(
    collections: &[CollectionDescriptor],
    state: &Entity<StudioState>,
    mono: SharedString,
) -> AnyElement {
    let mut list = v_flex()
        .id("live-picker")
        .gap_0p5()
        .overflow_y_scroll()
        .flex_1()
        .min_h_0();
    for (ix, c) in collections.iter().enumerate() {
        let mut row = div()
            .id(("live-pick", ix))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap_1p5()
            .px_2()
            .py_1()
            .rounded(px(2.))
            .hover(|s| s.bg(theme::HOVER))
            .child(
                div()
                    .flex()
                    .w(px(16.))
                    .justify_center()
                    .text_color(theme::ACCENT)
                    .child(fa::sized(collection_glyph(&c.id), px(12.))),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(NAME_SIZE))
                            .text_color(theme::TEXT)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(c.label.clone()),
                    )
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(POS_SIZE))
                            .text_color(theme::MUTED)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(format!("{} · {}", c.id, mode_names(c))),
                    ),
            );
        row = row.on_click({
            let state = state.clone();
            let id = c.id.clone();
            move |_, _, cx| activate(&state, &id, cx)
        });
        list = list.child(row);
    }
    list.into_any_element()
}

/// The active collection's visible rows as a virtual list (egui
/// ScrollArea over entity_row/sidebar_row rows). Row height is uniform per
/// list: tall when any row carries a position, plain otherwise.
fn rows(id: &str, d: SidebarData, mono: SharedString, state: Entity<StudioState>) -> AnyElement {
    let row_h = if d.models.iter().any(|m| m.position.is_some()) {
        ROW_TALL
    } else {
        ROW_PLAIN
    };
    let get_mode = d.get_mode;
    let models = Arc::new(d.models);
    let raw = Arc::new(d.raw);
    let col = id.to_string();

    uniform_list("live-rows", models.len().max(1), move |range, _, cx| {
        let selected = state
            .read(cx)
            .selection
            .as_ref()
            .filter(|s| s.col == col)
            .map(|s| s.index_str.clone());
        range
            .map(|ix| match models.get(ix) {
                Some(m) => {
                    let selected = selected.as_deref() == Some(m.index_str.as_str());
                    entity_row(
                        ix,
                        m,
                        selected,
                        row_h,
                        get_mode,
                        &col,
                        &raw,
                        &state,
                        mono.clone(),
                    )
                    .into_any_element()
                }
                // Keeps the measured slot when the list is empty.
                None => div().h(px(row_h)).into_any_element(),
            })
            .collect::<Vec<_>>()
    })
    .flex_1()
    .min_h_0()
    .w_full()
    .into_any_element()
}

/// Entity list row: 16px prefab icon slot, the display name, and for
/// positioned entities a muted "x y z" line under it (egui `entity_row`).
/// Selected rows invert to the accent background; missing icon/prefab
/// degrades to a plain name line (egui `sidebar_row`).
#[allow(clippy::too_many_arguments)]
fn entity_row(
    ix: usize,
    m: &RowModel,
    selected: bool,
    row_h: f32,
    get_mode: bool,
    col: &str,
    raw: &Arc<Vec<Value>>,
    state: &Entity<StudioState>,
    mono: SharedString,
) -> AnyElement {
    let (name_color, dim_color, icon_color) = if selected {
        (theme::ACCENT_TEXT, theme::ACCENT_TEXT, theme::ACCENT_TEXT)
    } else {
        (theme::TEXT, theme::FAINT, theme::MUTED)
    };
    let name = |text: String| {
        div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .text_size(px(NAME_SIZE))
            .text_color(name_color)
            .child(text)
    };

    let mut row = div()
        .id(("live-row", ix))
        .cursor_pointer()
        .flex()
        .items_center()
        .gap_1p5()
        .px_2()
        .h(px(row_h))
        .rounded(px(2.))
        .when(selected, |d| d.bg(theme::ACCENT))
        .when(!selected, |d| d.hover(|s| s.bg(theme::HOVER)));

    if let Some(classname) = &m.classname {
        row = row.child(
            div()
                .flex()
                .w(px(16.))
                .justify_center()
                .text_color(icon_color)
                .child(fa::sized(prefab_glyph(classname), px(12.))),
        );
    }

    match m.position {
        Some(p) => {
            row = row.child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .justify_center()
                    .child(name(m.display_name.clone()))
                    .child(
                        div()
                            .font_family(mono)
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(POS_SIZE))
                            .text_color(dim_color)
                            .child(protocol::format_position(&p)),
                    ),
            );
        }
        None => {
            row = row.child(name(m.display_name.clone()).flex_1());
        }
    }

    row = row.on_click({
        let state = state.clone();
        let col = col.to_string();
        let index_str = m.index_str.clone();
        let raw = raw.clone();
        let raw_ix = m.raw_ix;
        move |_, _, cx| {
            state.update(cx, |s, _| {
                // List+get collections fetch the detail snapshot; list-only
                // collections show the raw row (egui's click branch).
                if get_mode {
                    s.select(col.clone(), index_str.clone());
                } else if let Some(row) = raw.get(raw_ix) {
                    s.detail = Some(row.clone());
                }
            });
        }
    });
    row.into_any_element()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use serde_json::json;

    use deerpunk_studio::ansi::Stream;
    use deerpunk_studio::config::StudioConfig;
    use deerpunk_studio::mcp::McpManager;
    use deerpunk_studio::net::{ConnState, NetClient};
    use deerpunk_studio::profiles::Profiles;
    use deerpunk_studio::protocol::{CollectionDescriptor, CollectionGroup, CollectionMode};
    use deerpunk_studio::runner::{RunState, Runner};

    use super::{collection_glyph, name_or_first, prefab_glyph, snapshot};
    use crate::state::StudioState;

    /// A bare `StudioState` without the `new()` side effects (config load,
    /// project open, inspector auto-connect).
    fn test_state() -> StudioState {
        StudioState {
            cfg: StudioConfig::default(),
            project: None,
            gameinfo: None,
            scan: None,
            scan_rx: None,
            error: None,
            net: NetClient::new(),
            conn_state: ConnState::Disconnected,
            want_connected: false,
            reconnect_at: None,
            host: "127.0.0.1".into(),
            port: "9000".into(),
            refresh_ms: 250,
            last_refresh: Instant::now(),
            next_rpc_id: 1,
            pending: HashMap::new(),
            collections: Vec::new(),
            lists: HashMap::new(),
            last_fetch: HashMap::new(),
            prefab_map: HashMap::new(),
            selection: None,
            detail: None,
            inspector_built: false,
            profiles: Profiles::default(),
            active_profile: "dev".into(),
            scenarios: Vec::new(),
            active_scenario: "default".into(),
            runner: Runner::new(),
            run_state: RunState::Idle,
            steps: Vec::new(),
            console: Stream::new(5000),
            input: String::new(),
            mcp: McpManager::new(9100),
            active_collection: None,
        }
    }

    fn entities_descriptor() -> CollectionDescriptor {
        CollectionDescriptor {
            id: "serverentities".into(),
            label: "Entities".into(),
            group: CollectionGroup::Runtime,
            modes: vec![CollectionMode::List, CollectionMode::Get],
            id_field: "index".into(),
            nested_in: None,
        }
    }

    /// Rows resolve the display name through the prefab map (prefabId →
    /// classname), keep the position and the `index` click index, and the
    /// filter matches over `{first} {name} {display}` — egui parity.
    #[test]
    fn snapshot_resolves_classname_filter_and_index() {
        let mut state = test_state();
        state.collections = vec![entities_descriptor()];
        state.prefab_map = HashMap::from([(1u32, "player".to_string())]);
        state.lists.insert(
            "serverentities".into(),
            json!({
                "time": 0,
                "count": 2,
                "rows": [
                    {"index": 1, "name": "", "prefabId": 1, "position": [1.5, 0.0, -3.25]},
                    {"index": 2, "name": "misc ent"}
                ]
            }),
        );

        let all = snapshot(&state, "serverentities", "").expect("cached list");
        assert_eq!(all.total, 2);
        assert_eq!(all.models.len(), 2);
        assert_eq!(all.models[0].display_name, "player");
        assert_eq!(all.models[0].classname.as_deref(), Some("player"));
        assert_eq!(all.models[0].position, Some([1.5, 0.0, -3.25]));
        assert_eq!(all.models[0].index_str, "1");
        assert_eq!(all.models[1].display_name, "misc ent");
        assert_eq!(all.models[1].classname, None);
        assert_eq!(all.models[1].position, None);

        // Position rows put the list in the tall uniform slot.
        let tall = &all.models;
        assert!(tall.iter().any(|m| m.position.is_some()));

        // Filter hits the classname display name but not the bare name.
        let by_class = snapshot(&state, "serverentities", "PLAY").expect("filter result");
        assert_eq!(by_class.models.len(), 1);
        assert_eq!(by_class.models[0].display_name, "player");
        assert!(
            snapshot(&state, "serverentities", "zzz")
                .expect("filter result")
                .models
                .is_empty()
        );
    }

    /// Legacy `prefab` strings and custom `id_field`s resolve like egui:
    /// classname falls back to the row's `prefab`, the click index comes
    /// from the descriptor's `id_field`.
    #[test]
    fn snapshot_falls_back_to_legacy_prefab_and_id_field() {
        let mut state = test_state();
        state.collections = vec![CollectionDescriptor {
            id: "methoddb".into(),
            label: "Methods".into(),
            group: CollectionGroup::Definitions,
            modes: vec![CollectionMode::List],
            id_field: "name".into(),
            nested_in: None,
        }];
        state.lists.insert(
            "methoddb".into(),
            json!({"count": 1, "entries": [
                {"name": "cam_shake", "prefab": "item_drop"}
            ]}),
        );

        let snap = snapshot(&state, "methoddb", "").expect("cached list");
        assert!(!snap.get_mode, "list-only collection: no .get fetch");
        assert_eq!(snap.models.len(), 1);
        let m = &snap.models[0];
        assert_eq!(m.classname.as_deref(), Some("item_drop"));
        assert_eq!(m.display_name, "item_drop");
        assert_eq!(m.index_str, "cam_shake");
    }

    /// No cached list → `None` (the header-only branch).
    #[test]
    fn snapshot_without_cached_list_is_none() {
        let mut state = test_state();
        state.collections = vec![entities_descriptor()];
        assert!(snapshot(&state, "serverentities", "").is_none());
    }

    /// Glyph tables keep the egui mapping (Phosphor → FA6 solid).
    #[test]
    fn glyph_tables_match_egui_mapping() {
        assert_eq!(prefab_glyph("player"), '\u{f007}');
        assert_eq!(prefab_glyph("item_drop"), '\u{f487}');
        assert_eq!(prefab_glyph("main_menu"), '\u{f03e}');
        assert_eq!(prefab_glyph("loading_screen"), '\u{f03e}');
        assert_eq!(prefab_glyph("whatever"), '\u{f1b2}');

        assert_eq!(collection_glyph("entities"), '\u{f1b2}');
        assert_eq!(collection_glyph("assets"), '\u{e4e3}');
        assert_eq!(collection_glyph("players"), '\u{f0c0}');
        assert_eq!(collection_glyph("systems"), '\u{f625}');
        assert_eq!(collection_glyph("fmod_events"), '\u{f001}');
        assert_eq!(collection_glyph("methoddb"), '\u{f0ca}');
        assert_eq!(collection_glyph("unknown"), '\u{f1c0}');
    }

    /// `name` wins over the first cell only when non-empty (egui
    /// `name_or_first`).
    #[test]
    fn name_or_first_prefers_non_empty_name() {
        assert_eq!(name_or_first("named", "first"), "named");
        assert_eq!(name_or_first("", "first"), "first");
    }
}
