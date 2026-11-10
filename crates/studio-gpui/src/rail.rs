//! Far-left view rail (gpui analog of the egui `rail` — the VS Code style
//! activity bar): a ~44 px vertical icon strip that switches the active
//! view. Every slot is wired: a click marks the item active and emits a
//! [`RailEvent`]; the workspace maps it to the panel activation in the
//! dock area. Bottom: the console dock toggle and the MCP server
//! indicator.
//!
//! Icons are Font Awesome solid glyphs (`crate::fa`), rendered as text so
//! they never depend on bundled SVG assets.

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::dock::{DockArea, DockPlacement};
use gpui_kit::component::v_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::fa;
use crate::state::StudioState;
use crate::theme;

/// Rail width (egui: `Panel::left("rail").exact_size(48.0)`; the gpui
/// frontend uses a slightly tighter 44 px).
const RAIL_WIDTH: f32 = 44.;
/// Square icon-button size (egui buttons were 38×30 with a 17 px glyph).
const BTN_SIZE: f32 = 30.;
/// Glyph size inside a rail button.
const ICON_SIZE: f32 = 16.;
/// Accent stripe width at the left edge of the active item.
const STRIPE_W: f32 = 3.;

/// The views the rail can select — the workspace re-fills the left dock
/// with the matching panel (Puffin goes to the center as a tab), so the
/// rail is the only switcher and the dock draws no tab bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RailView {
    Files,
    Live,
    Cvars,
    LaunchSettings,
    Puffin,
}

/// A rail click: the view the user wants to see. The workspace subscribes
/// and activates the matching panel (the rail itself owns no panels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RailEvent(pub RailView);

struct RailItem {
    view: RailView,
    icon: char,
    tooltip: &'static str,
}

const RAIL_ITEMS: [RailItem; 5] = [
    RailItem {
        view: RailView::Files,
        icon: fa::FOLDER,
        tooltip: "File browser",
    },
    RailItem {
        view: RailView::Live,
        icon: fa::DATABASE,
        tooltip: "Live inspector",
    },
    RailItem {
        view: RailView::Cvars,
        icon: fa::SLIDERS,
        tooltip: "Cvars",
    },
    RailItem {
        view: RailView::LaunchSettings,
        icon: fa::GEAR,
        tooltip: "Launch settings",
    },
    RailItem {
        view: RailView::Puffin,
        icon: fa::FLAME,
        tooltip: "Puffin profiler",
    },
];

impl RailItem {
    fn id(&self) -> &'static str {
        match self.view {
            RailView::Files => "rail-files",
            RailView::Live => "rail-live",
            RailView::Cvars => "rail-cvars",
            RailView::LaunchSettings => "rail-launch",
            RailView::Puffin => "rail-puffin",
        }
    }
}

pub struct Rail {
    /// Selected view; the workspace mirrors it onto the dock tabs.
    selected: RailView,
    state: Entity<StudioState>,
    dock_area: Entity<DockArea>,
}

impl EventEmitter<RailEvent> for Rail {}

impl Rail {
    pub fn new(state: Entity<StudioState>, dock_area: Entity<DockArea>) -> Self {
        Self {
            selected: RailView::Files,
            state,
            dock_area,
        }
    }

    fn item_button(&self, item: &RailItem, rail: &WeakEntity<Rail>, cx: &App) -> impl IntoElement {
        let active = self.selected == item.view;

        let mut variant = ButtonCustomVariant::new(cx).foreground(if active {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        });
        if active {
            variant = variant.color(theme::FIELD);
        }

        let view = item.view;
        let rail = rail.clone();
        let button = Button::new(item.id())
            .custom(variant)
            // FA glyph as a plain child: the glyph inherits the button's
            // foreground (normal/hover/selected/disabled), so no extra
            // styling is needed here.
            .child(fa::sized(item.icon, px(ICON_SIZE)))
            // Force the icon-button geometry: with a child element the
            // button switches to the padded "normal" layout, so the exact
            // square size and zero padding are replayed via the base style.
            .p_0()
            .size(px(BTN_SIZE))
            .rounded(px(4.))
            .tooltip(item.tooltip)
            .on_click(move |_, _, cx| {
                if let Some(rail) = rail.upgrade() {
                    rail.update(cx, |rail, cx| {
                        rail.selected = view;
                        cx.emit(RailEvent(view));
                        cx.notify();
                    });
                }
            });

        // VS Code-style active marker: accent stripe at the rail's left
        // edge (the egui rail painted the same 3 px stripe).
        div()
            .relative()
            .w_full()
            .flex()
            .justify_center()
            .when(active, |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(STRIPE_W))
                        .rounded_r(px(1.))
                        .bg(theme::ACCENT),
                )
            })
            .child(button)
    }

    fn console_toggle(&self, cx: &App) -> impl IntoElement {
        let open = self.dock_area.read(cx).is_dock_open(DockPlacement::Bottom);
        let dock_area = self.dock_area.clone();

        let mut variant = ButtonCustomVariant::new(cx).foreground(if open {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        });
        if open {
            variant = variant.color(theme::FIELD);
        }

        Button::new("rail-console")
            .custom(variant)
            .child(fa::sized(fa::SQUARE_TERMINAL, px(ICON_SIZE)))
            .p_0()
            .size(px(BTN_SIZE))
            .rounded(px(4.))
            .tooltip("Toggle console")
            .on_click(move |_, window, cx| {
                dock_area.update(cx, |area, cx| {
                    area.toggle_dock(DockPlacement::Bottom, window, cx)
                });
            })
    }

    /// MCP server plug, green while the child process is alive (the egui
    /// rail opened the MCP modal from here; the gpui frontend has no modal
    /// yet, so it stays a status indicator).
    fn mcp_indicator(&self, cx: &mut App) -> impl IntoElement {
        let up = self.state.update(cx, |state, _| state.mcp.is_running());
        div()
            .flex()
            .items_center()
            .justify_center()
            .size(px(BTN_SIZE))
            .rounded(px(4.))
            .text_color(if up { theme::GREEN } else { theme::MUTED })
            .child(fa::sized(fa::PLUG, px(ICON_SIZE)))
    }
}

impl Render for Rail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rail = cx.entity().downgrade();
        let items: Vec<_> = RAIL_ITEMS
            .iter()
            .map(|item| self.item_button(item, &rail, cx))
            .collect();

        v_flex()
            .w(px(RAIL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .items_center()
            .pt_2()
            .pb_2()
            .gap_1()
            .bg(theme::BG)
            .border_r_1()
            .border_color(theme::BORDER)
            .children(items)
            .child(div().flex_1())
            .child(self.console_toggle(cx))
            .child(self.mcp_indicator(cx))
    }
}
