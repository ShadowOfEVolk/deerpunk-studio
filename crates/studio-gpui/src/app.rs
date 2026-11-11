//! Workspace: root Render view hosting the main DockArea with the studio
//! layout — left dock holding the single active view (Files / Live / Cvars /
//! Launch settings, switched by the rail), bottom dock
//! (Console / Diagnostics), center tabs (Rmlui / Viewer, plus the Puffin
//! profiler tab the rail summons on demand and the entity Inspector tab a
//! Live-sidebar click summons) — mirroring the egui layout in
//! src/panels.rs.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::dock::{
    BasePanelView, DockArea, DockLayout, DockPlacement, DockSkin, panel_handle,
};
#[cfg(test)]
use gpui_kit::component::dock::{PanelId, PaneRef};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;

use crate::console::ConsolePanel;
use crate::cvars::CvarsPanel;
use crate::diagnostics::DiagnosticsPanel;
use crate::files::FilesPanel;
use crate::inspector::InspectorPanel;
use crate::launch_settings::LaunchSettingsPanel;
use crate::live::{InspectEntity, LiveBus, LivePanel};
use crate::puffin::PuffinPanel;
use crate::rail::{Rail, RailEvent, RailView};
use crate::rmlui::RmluiPanel;
use crate::state::{StudioState, spawn_pump};
use crate::statusbar::StatusBar;
use crate::topbar::TopBar;
use crate::viewers::{OpenFile, ViewerBus, ViewersPanel};

const LEFT_DOCK_SIZE: f32 = 250.;
const BOTTOM_DOCK_SIZE: f32 = 220.;

pub struct Workspace {
    dock_area: Entity<DockArea>,
    topbar: Entity<TopBar>,
    rail: Entity<Rail>,
    statusbar: Entity<StatusBar>,
    /// Headless-test hooks (tests/ui.rs drives entity selection without
    /// mouse synthesis — gpui-kit's test-support feature is off): the
    /// shared studio state, the Live inspect bus, and the Inspector tab's
    /// panel id.
    #[cfg(test)]
    test_state: Entity<StudioState>,
    #[cfg(test)]
    test_live_bus: Entity<LiveBus>,
    #[cfg(test)]
    test_inspector_id: PanelId,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (dock_area, skin) = DockSkin::dock_area("main-dock", Some(1), window, cx);
        // The skin renders its own dock-collapse buttons in the tab bars
        // (the center tab bar grows a ~64px prefix block for them), which
        // duplicate the far-left icon rail that already toggles docks — and
        // with an invisible icon they read as a huge dead indent where the
        // content tabs should start. The rail is the affordance; drop the
        // skin's.
        skin.set_toggle_button_visible(false, cx);

        let state = cx.new(|_| StudioState::new());
        spawn_pump(state.clone(), cx);

        let bus = cx.new(|_| ViewerBus::new());
        let live_bus = cx.new(|_| LiveBus::new());
        let files = cx.new(|cx| FilesPanel::new(state.clone(), bus.clone(), cx));
        let live = cx.new(|cx| LivePanel::new(state.clone(), live_bus.clone(), cx));
        let cvars = cx.new(|cx| CvarsPanel::new(state.clone(), cx));
        let launch = cx.new(|cx| LaunchSettingsPanel::new(state.clone(), cx));
        let console = cx.new(|cx| ConsolePanel::new(state.clone(), cx));
        let diagnostics =
            cx.new(|cx| DiagnosticsPanel::with_bus(state.clone(), Some(bus.clone()), cx));
        let rmlui = cx.new(|cx| RmluiPanel::new(state.clone(), cx));
        let viewers = cx.new(|cx| ViewersPanel::new(state.clone(), bus.clone(), cx));
        // The profiler panel has no initial dock home (the rail slot summons
        // it as a center tab on demand); the workspace keeps the handle.
        let puffin = cx.new(|cx| PuffinPanel::new(state.clone(), cx));
        // Same for the Inspector: no dock home — an entity click in the
        // Live sidebar summons it as the (single) center tab.
        let inspector = cx.new(|cx| InspectorPanel::new(state.clone(), cx));

        let files_view = panel_handle(files);
        let live_view = panel_handle(live);
        let cvars_view = panel_handle(cvars);
        let launch_view = panel_handle(launch);
        let console_view = panel_handle(console);
        let diagnostics_view = panel_handle(diagnostics);
        let rmlui_view = panel_handle(rmlui.clone());
        let viewers_view = panel_handle(viewers.clone());
        let puffin_view = panel_handle(puffin);
        let inspector_view = panel_handle(inspector.clone());
        #[cfg(test)]
        let test_inspector_id = inspector_view.panel_id(cx);

        let center = DockLayout::tabs()
            .panel_view(rmlui_view.clone(), cx)
            .panel_view(viewers_view.clone(), cx);
        // VS Code model: the rail is the only view switcher, so the left
        // dock holds just the active view and draws no tab bar (a
        // single-panel group under the default PanelStyle::Auto renders a
        // bare title, see the rail routing below).
        let left = DockLayout::tabs().panel_view(files_view.clone(), cx);
        let bottom = DockLayout::tabs()
            .panel_view(console_view, cx)
            .panel_view(diagnostics_view, cx);

        dock_area.update(cx, |area, cx| {
            area.set_center(center, window, cx);
            area.set_dock(DockPlacement::Left, left, window, cx);
            area.set_dock_size(DockPlacement::Left, px(LEFT_DOCK_SIZE), window, cx);
            area.set_dock(DockPlacement::Bottom, bottom, window, cx);
            area.set_dock_size(DockPlacement::Bottom, px(BOTTOM_DOCK_SIZE), window, cx);
        });

        // Chrome around the dock area: top bar (launch + connection), the
        // far-left view rail and the bottom status strip — the gpui analog
        // of the egui topbar / rail / statusbar panels.
        let topbar = cx.new(|cx| TopBar::new(state.clone(), cx));
        let rail = cx.new(|_| Rail::new(state.clone(), dock_area.clone()));
        let statusbar = cx.new(|cx| StatusBar::new(state.clone(), cx));

        let window_handle = window.window_handle();

        // Rail routing: the rail is the only switcher, so a click re-fills
        // the left dock with the clicked view's panel alone — no tab bar is
        // drawn for a single-panel group (`PanelStyle::Auto` falls back to a
        // plain title). `set_dock` keeps the replaced dock's size and open
        // state, so the width the user dragged survives the swap. Puffin is
        // the exception — it has no left-dock home: its slot adds the panel
        // as a center tab the first time (an add activates the tab);
        // afterwards it just selects.
        cx.subscribe(&rail, {
            let dock_area = dock_area.clone();
            let files_view = files_view;
            let live_view = live_view;
            let cvars_view = cvars_view;
            let launch_view = launch_view;
            let puffin_view = puffin_view;
            move |_, _, event: &RailEvent, cx| {
                let view = event.0;
                let target = match view {
                    RailView::Files => files_view.clone(),
                    RailView::Live => live_view.clone(),
                    RailView::Cvars => cvars_view.clone(),
                    RailView::LaunchSettings => launch_view.clone(),
                    RailView::Puffin => puffin_view.clone(),
                };
                let _ = window_handle.update(cx, |_root, window, cx| {
                    dock_area.update(cx, |area, cx| {
                        if view == RailView::Puffin {
                            summon_center_tab(area, target, window, cx);
                        } else {
                            area.set_dock(
                                DockPlacement::Left,
                                DockLayout::tabs().panel_view(target, cx),
                                window,
                                cx,
                            );
                        }
                    });
                });
            }
        })
        .detach();

        // File-open routing: `.rml` goes to the live RmlUi preview tab,
        // everything else lands in the generic viewer tab. Both handlers
        // also activate their tab so the click visibly switches content.
        cx.subscribe(&bus, {
            let rmlui = rmlui.clone();
            let rmlui_view = rmlui_view.clone();
            let viewers_view = viewers_view.clone();
            let dock_area = dock_area.clone();
            move |_, _, event: &OpenFile, cx| {
                let path = event.0.clone();
                let is_rml = path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("rml"));
                let target = if is_rml {
                    let rel = ui_rel_of(&path);
                    rmlui.update(cx, |panel, cx| panel.open(path, rel, cx));
                    rmlui_view.clone()
                } else {
                    viewers_view.clone()
                };
                let id = target.panel_id(cx);
                let _ = window_handle.update(cx, |_root, window, cx| {
                    dock_area.update(cx, |area, cx| area.select_panel(id, window, cx));
                });
            }
        })
        .detach();

        // Entity-inspect routing: a click in the Live sidebar applies its
        // selection to the shared StudioState and pings LiveBus; the
        // workspace answers by summoning the Inspector center tab — added
        // the first time (an add activates the tab), re-selected after, so
        // one tab serves every entity (the rail's Puffin pattern).
        cx.subscribe(&live_bus, {
            let inspector_view = inspector_view;
            let dock_area = dock_area.clone();
            move |_, _, _: &InspectEntity, cx| {
                let _ = window_handle.update(cx, |_root, window, cx| {
                    dock_area.update(cx, |area, cx| {
                        summon_center_tab(area, inspector_view.clone(), window, cx);
                    });
                });
            }
        })
        .detach();

        Self {
            dock_area,
            topbar,
            rail,
            statusbar,
            #[cfg(test)]
            test_state: state,
            #[cfg(test)]
            test_live_bus: live_bus,
            #[cfg(test)]
            test_inspector_id,
        }
    }
}

/// Summon `view` as a center tab: add it the first time (an add activates
/// the tab), select the existing tab afterwards. Shared by the rail's
/// Puffin slot and the entity-inspect routing — one panel, one tab, no
/// per-target duplicates.
fn summon_center_tab(
    area: &mut DockArea,
    view: Arc<dyn BasePanelView>,
    window: &mut Window,
    cx: &mut Context<DockArea>,
) {
    let id = view.panel_id(cx);
    if area.panel(id).is_none() {
        area.add_panel_view(view, DockPlacement::Center, None, window, cx);
    } else {
        area.select_panel(id, window, cx);
    }
}

#[cfg(test)]
impl Workspace {
    /// The shared studio state (headless tests drive entity selection
    /// through `live::select_row`; mouse synthesis is unavailable without
    /// gpui-kit's test-support feature).
    pub fn studio_state_for_test(&self) -> &Entity<StudioState> {
        &self.test_state
    }

    /// The Live inspect bus: emitting [`InspectEntity`] on it exercises the
    /// workspace's Inspector summon routing end-to-end.
    pub fn live_bus_for_test(&self) -> &Entity<LiveBus> {
        &self.test_live_bus
    }

    /// Inspector tab state: present in the dock area, and selected as the
    /// active tab of its center tab group.
    pub fn inspector_tab_for_test(&self, cx: &App) -> (bool, bool) {
        let area = self.dock_area.read(cx);
        let present = area.panel(self.test_inspector_id).is_some();
        let selected = area
            .layout(DockPlacement::Center)
            .and_then(|tree| {
                let node = tree.find_panel_node(self.test_inspector_id)?;
                match tree.find_node(node)?.kind() {
                    PaneRef::Tabs { panels, active_ix } => {
                        Some(panels.get(active_ix) == Some(&self.test_inspector_id))
                    }
                    _ => None,
                }
            })
            .unwrap_or(false);
        (present, selected)
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // egui layout parity: top bar / left rail + dock area / status bar.
        v_flex()
            .relative()
            .size_full()
            .bg(crate::theme::BG)
            .text_color(crate::theme::TEXT)
            .child(self.topbar.clone())
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.rail.clone())
                    // h_flex centers children on the cross axis; without an
                    // explicit height this wrapper collapses to the DockArea's
                    // content height (a tab bar plus the bottom dock — the
                    // panels hang absolutely and contribute none) and gets
                    // vertically centered, leaving equal gaps against the
                    // top bar and the status bar.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(self.dock_area.clone()),
                    ),
            )
            .child(self.statusbar.clone())
    }
}

/// Path of `file` relative to its `ui` root: the components after the last
/// `ui` directory component (`<gamedir>/ui/widgets/main_menu.rml` →
/// `widgets/main_menu.rml`); falls back to the file name.
fn ui_rel_of(file: &Path) -> String {
    let comps: Vec<_> = file.components().collect();
    if let Some(ix) = comps.iter().rposition(|c| c.as_os_str() == "ui") {
        let rel: PathBuf = comps[ix + 1..].iter().collect();
        if let Some(s) = rel.to_str() {
            return s.replace('\\', "/");
        }
    }
    file.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
