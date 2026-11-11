//! Entity Inspector (center tab): the working-zone host for the selected
//! entity's detail view (`crate::live_detail::detail`). The panel has no
//! dock home — the workspace summons it as a center tab the first time an
//! entity row is clicked in the Live sidebar (the rail's Puffin pattern)
//! and re-selects that one tab on every later click, so a browsing session
//! accumulates one Inspector tab, not one per entity. The selection
//! snapshot itself lives in `StudioState` (`detail` + `prefab_map`);
//! this panel only mirrors it.

use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::*;

use serde_json::Value;

use crate::live_detail;
use crate::state::StudioState;
use crate::theme;

/// Placeholder text density (the live sidebar's old placeholder size).
const NAME_SIZE: f32 = 12.5;

pub struct InspectorPanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    /// The `state.detail` snapshot the memoized view was built from.
    /// [`live_detail::detail`] builds a fresh default-open view per call,
    /// so it must be rebuilt only when the selection actually changes —
    /// not on every frame (the 50 ms pump re-renders the panel often).
    shown: Option<Value>,
    /// Memoized detail view (`None` while no entity is selected).
    view: Option<AnyView>,
}

impl InspectorPanel {
    pub fn new(state: Entity<StudioState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |this, state, cx| {
            // The pump notifies on every tick; re-render only when the
            // selection snapshot actually changed.
            let changed = this.shown.as_ref() != state.read(cx).detail.as_ref();
            if changed {
                cx.notify();
            }
        })
        .detach();
        Self {
            focus_handle: cx.focus_handle(),
            state,
            shown: None,
            view: None,
        }
    }

    /// The memoized detail view for the current selection, rebuilt whenever
    /// `state.detail` differs from the one it was built from. `None` while
    /// nothing is selected (the caller renders the placeholder).
    fn ensure_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyView> {
        if self.shown.as_ref() != self.state.read(cx).detail.as_ref() {
            self.shown = self.state.read(cx).detail.clone();
            self.view = None;
        }
        if self.view.is_none() {
            let detail = self.shown.as_ref()?;
            let map = self.state.read(cx).prefab_map.clone();
            self.view = Some(live_detail::detail(detail, &map, window, cx));
        }
        self.view.clone()
    }
}

impl EventEmitter<PanelEvent> for InspectorPanel {}

impl Focusable for InspectorPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for InspectorPanel {
    fn panel_name(&self) -> &'static str {
        "InspectorPanel"
    }
}

impl Panel for InspectorPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Inspector"
    }
}

impl Render for InspectorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.ensure_view(window, cx) {
            Some(view) => div()
                .size_full()
                .overflow_hidden()
                .bg(theme::BG)
                .child(view)
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::BG)
                .text_color(theme::FAINT)
                .text_size(px(NAME_SIZE))
                .child("Select an item")
                .into_any_element(),
        }
    }
}
