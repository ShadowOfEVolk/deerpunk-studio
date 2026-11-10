//! Top bar (gpui analog of the egui `top_bar` in src/app.rs), two strips:
//! a thin [`TitleBar`] strip on top owning the macOS traffic lights, window
//! dragging and double-click zoom (nothing else lives there), and below it
//! the studio toolbar — identity block on the left (game icon + `deerpunk >
//! game` logo; clicking it opens the gameinfo modal), the launch cluster
//! centered (play/stop button + active profile/scenario label + a launch
//! dropdown with PROFILE and SCENARIOS sections) and an empty spacer on the
//! right — the inspector status lives in the statusbar chip now.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::popover::{Popover, PopoverState};
use gpui_kit::component::{Disableable, Sizable, Size, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use deerpunk_studio::runner::RunState;

use crate::fa;
use crate::state::StudioState;
use crate::statusbar::tinted;
use crate::theme;

/// Height of the thin strip above the toolbar: a bare [`TitleBar`] owning
/// the native traffic lights (positioned at (9,9)) and the window drag —
/// on macOS `WindowControls` renders an empty div, so nothing else can end
/// up on this level.
const STRIP_HEIGHT: f32 = 28.;
/// Height of the studio toolbar strip below the traffic-light strip (egui:
/// `Panel::top("topbar").exact_size(44.0)`).
const BAR_HEIGHT: f32 = 44.;
/// Height of the mega launch button (egui `BTN_H`).
const BTN_H: f32 = 32.;

pub struct TopBar {
    state: Entity<StudioState>,
    /// Game icon cache: the icon source path it was decoded from plus the
    /// decode result (`None` when the file is missing or its bytes are
    /// broken — the fa::GAMEPAD glyph stands in). Keyed by path, so a
    /// project/gameinfo change (the observed [`StudioState`] re-renders us)
    /// reloads exactly once, and a broken icon doesn't re-decode per frame.
    icon: Option<(PathBuf, Option<Arc<RenderImage>>)>,
}

impl TopBar {
    pub fn new(state: Entity<StudioState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self { state, icon: None }
    }
}

/// Edge of the identity icon slot. The decoded frame is scaled to fit
/// exactly this box.
const ICON_SIZE: f32 = 36.;

/// Size the icon frame is rendered at inside the slot, preserving the
/// aspect ratio. `game.ico` frames are commonly smaller than the slot
/// (16/32px) — [`ObjectFit::ScaleDown`] never enlarges, so the decoder
/// upscales the buffer itself (see [`load_game_icon`]); anything larger
/// than the slot keeps drawing scaled down to it.
fn icon_fit_size(w: u32, h: u32) -> (u32, u32) {
    let scale = ICON_SIZE / w.max(h) as f32;
    if scale > 1. {
        (
            (w as f32 * scale).round().max(1.) as u32,
            (h as f32 * scale).round().max(1.) as u32,
        )
    } else {
        (w, h)
    }
}

/// Decode the project icon for the identity block — the egui
/// `load_game_icon` path (fs read → `image::load_from_memory`, which
/// handles the `game.ico` container) ending in a [`RenderImage`] instead of
/// a bytes handle. gpui's `RenderImage` carries BGRA bytes, so the decoded
/// RGBA channels are swapped first — the same byte order the RmlUi
/// rasterizer feeds it in `raster.rs`.
fn load_game_icon(path: &Path) -> Option<Arc<RenderImage>> {
    let bytes = std::fs::read(path).ok()?;
    let decoded = image::load_from_memory(&bytes).ok()?;
    // Upscale a frame smaller than the slot to exactly fit it: gpui paints
    // `ScaleDown` images at their natural size, so a 16px ICO frame would
    // otherwise render as a 16px icon inside the slot box. Nearest
    // neighbor: game.ico frames are pixel art (ohmyblock's is 16x16), and
    // an interpolating filter smears them into mush at 2.25x.
    let (fit_w, fit_h) = icon_fit_size(decoded.width(), decoded.height());
    let decoded = if (fit_w, fit_h) != (decoded.width(), decoded.height()) {
        image::DynamicImage::ImageRgba8(image::imageops::resize(
            &decoded.to_rgba8(),
            fit_w,
            fit_h,
            image::imageops::FilterType::Nearest,
        ))
    } else {
        decoded
    };
    let mut bgra = decoded.to_rgba8().into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let frame = image::Frame::new(image::RgbaImage::from_raw(
        decoded.width(),
        decoded.height(),
        bgra,
    )?);
    Some(Arc::new(RenderImage::new([frame])))
}

impl Render for TopBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let running = state.run_state != RunState::Idle;
        let has_project = state.project.is_some();

        // Game icon (egui identity block): resolve the source path from the
        // open project's gameinfo and reload only when it moved — the cache
        // doubles as change detection on top of the `cx.observe(state)`
        // notify, since project/gameinfo swaps change the resolved path.
        let icon_path = state
            .project
            .as_ref()
            .zip(state.gameinfo.as_ref())
            .and_then(|(project, info)| info.icon_path(&project.gamedir));
        if self.icon.as_ref().map(|(path, _)| path) != icon_path.as_ref() {
            self.icon = icon_path.map(|path| {
                let icon = load_game_icon(&path);
                (path, icon)
            });
        }
        let game_icon = self.icon.as_ref().and_then(|(_, icon)| icon.clone());

        // The play slot doubles as the run-state indicator: spinner while
        // building/stopping, pulse while running, warning on failure
        // (same glyph/tint mapping as the egui top_bar_center).
        let (glyph, tint) = match &state.run_state {
            RunState::Idle => (fa::PLAY, theme::ACCENT),
            RunState::Building | RunState::Stopping => (fa::SPINNER, theme::YELLOW),
            RunState::Running => (fa::ACTIVITY, theme::GREEN),
            RunState::Failed(_) => (fa::CIRCLE_EXCLAMATION, theme::RED),
        };

        let profile_label = state
            .profiles
            .get(&state.active_profile)
            .map(|p| p.label.clone())
            .unwrap_or_else(|| state.active_profile.clone());
        // The active scenario after the profile, egui top-bar parity
        // (`{profile} / {scenario}`); an unknown id falls back to itself.
        let scenario_label = state
            .scenarios
            .iter()
            .find(|s| s.id == state.active_scenario)
            .map(|s| s.label.clone())
            .unwrap_or_else(|| state.active_scenario.clone());
        let launch_label = format!("{profile_label} / {scenario_label}");

        let project_name = state
            .project
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "no project".into());

        // ── left zone: game identity (icon + project name) ──────────────
        // The zone keeps `flex_1` (twin of the right spacer, so the launch
        // cluster stays centered) and centers the identity block; the block
        // itself hugs its content — icon and wordmark in a row with an 8px
        // gap, a small 8px indent from the toolbar edge (the traffic lights
        // live on their own strip above) and vertically centered in the 44px
        // toolbar. Clickable like the egui `identity_block`: opens the
        // gameinfo modal (the egui "Game info" hover hint has no public
        // tooltip-on-div API in gpui-component; the pointer + hover fill
        // carry the affordance).
        let logo = h_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .overflow_hidden()
            .items_center()
            .pl(px(8.))
            .child(
                h_flex()
                    .id("game-identity")
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|this| this.bg(tinted(theme::TEXT, 0.06)))
                    .on_click({
                        let state = self.state.clone();
                        move |_, window, cx| crate::modals::open_gameinfo(&state, window, cx)
                    })
                    .child(match game_icon {
                        // The game's own icon: the full slot, ~6px rounded,
                        // no box or border. The decoder upscales small ICO
                        // frames to the slot, so `ScaleDown` paints it at
                        // full size.
                        Some(icon) => div().flex_shrink_0().child(
                            img(icon)
                                .size(px(ICON_SIZE))
                                .rounded(px(6.))
                                .object_fit(ObjectFit::ScaleDown),
                        ),
                        // No project / no icon file / broken bytes: controller
                        // glyph, exactly 30px.
                        None => div()
                            .flex_shrink_0()
                            .text_color(if has_project {
                                theme::TEXT
                            } else {
                                theme::TEXT_DIM
                            })
                            .child(fa::sized(fa::GAMEPAD, px(30.))),
                    })
                    // The logo line: muted "deerpunk >" prefix before the game
                    // name.
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .items_baseline()
                            .text_size(px(21.))
                            .font_weight(FontWeight::BOLD)
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .whitespace_nowrap()
                                    .text_color(theme::MUTED)
                                    .child("deerpunk"),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .whitespace_nowrap()
                                    .text_color(theme::FAINT)
                                    .child(">"),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .overflow_hidden()
                                    .text_color(if has_project {
                                        theme::TEXT
                                    } else {
                                        theme::MUTED
                                    })
                                    .child(project_name),
                            ),
                    ),
            );

        // ── center zone: ONE mega launch button ─────────────────────────
        let play = Button::new("mega-play")
            .ghost()
            .custom(ButtonCustomVariant::new(cx).foreground(if has_project {
                tint
            } else {
                theme::TEXT_DIM
            }))
            .with_size(Size::Size(px(BTN_H)))
            .child(
                div()
                    .text_color(if has_project { tint } else { theme::TEXT_DIM })
                    .child(fa::sized(glyph, px(16.))),
            )
            .disabled(!has_project)
            .tooltip(if running {
                "Stop instance"
            } else {
                "Run instance"
            })
            .on_click({
                let state = self.state.clone();
                move |_, _, cx| {
                    state.update(cx, |state, cx| {
                        if state.run_state == RunState::Idle {
                            state.run_instance();
                        } else {
                            state.runner.stop();
                        }
                        cx.notify();
                    });
                }
            });

        let launch_menu_popover = {
            let state = self.state.clone();
            Popover::new("launch-menu")
                .anchor(Anchor::TopLeft)
                .trigger(
                    Button::new("profile-caret")
                        .ghost()
                        .with_size(Size::Size(px(BTN_H)))
                        .custom(ButtonCustomVariant::new(cx).foreground(theme::TEXT))
                        .child(fa::sized(fa::CHEVRON_DOWN, px(13.))),
                )
                .content(move |popover, _, cx| launch_menu(&state, popover, cx))
        };

        let cluster = h_flex()
            .h(px(BTN_H))
            .flex_shrink_0()
            .overflow_hidden()
            .rounded(px(2.))
            .map(|this| {
                if has_project {
                    this.bg(tinted(tint, 0.18))
                } else {
                    this.bg(theme::FIELD)
                }
            })
            .when(running, |this| {
                this.border_1().border_color(tinted(tint, 0.31))
            })
            .child(play)
            .child(
                div()
                    .px_2()
                    .h_full()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .text_size(px(15.))
                    .text_color(if has_project {
                        theme::TEXT
                    } else {
                        theme::MUTED
                    })
                    .whitespace_nowrap()
                    .child(launch_label),
            )
            // Subtle vertical divider between the label and the chevron
            // (the egui bar draws the same line).
            .child(
                div()
                    .h(px(BTN_H - 12.))
                    .w(px(1.))
                    .flex_shrink_0()
                    .bg(tinted(theme::TEXT, 0.15)),
            )
            .child(launch_menu_popover);

        // ── right zone: empty spacer ────────────────────────────────────
        // The inspector badge + host:port used to live here; they moved to
        // the statusbar inspector chip (hover popover). This flex-1 twin of
        // the logo zone is what keeps the launch cluster centered.
        let right = h_flex().flex_1();

        // Solid fill for both strips: header gray at rest, softly tinted
        // toward the run state color while a run is active (egui HEADER /
        // BAR_ACTIVE).
        let bar_bg = if running {
            mix(theme::HEADER, tint, 0.12)
        } else {
            theme::HEADER
        };

        // ── two strips ─────────────────────────────────────────────────
        // The bare TitleBar strip owns the traffic lights + window drag and
        // stays free of studio controls (on macOS WindowControls renders an
        // empty div, so nothing native lands anywhere else); the toolbar
        // below is a plain row — no 80px traffic-light reservation needed.
        let toolbar = h_flex()
            .w_full()
            .h(px(BAR_HEIGHT))
            .flex_shrink_0()
            .bg(bar_bg)
            .child(logo)
            .child(cluster)
            .child(right);

        v_flex()
            .child(TitleBar::new().h(px(STRIP_HEIGHT)).bg(bar_bg))
            .child(toolbar)
    }
}

/// Dropdown body for the launch chevron, two sections: the run profiles and
/// the launch scenarios, each row checked when active. Selecting a row
/// stores the id in `StudioState::active_profile` / `active_scenario` (the
/// mega-button label reads both) and dismisses the popover.
fn launch_menu(
    state: &Entity<StudioState>,
    _popover: &mut PopoverState,
    cx: &mut Context<PopoverState>,
) -> Div {
    let popover_state = cx.entity().downgrade();
    let state_ref = state.read(cx);
    let active_profile = state_ref.active_profile.clone();
    let active_scenario = state_ref.active_scenario.clone();

    let profile_rows: Vec<_> = state_ref
        .profiles
        .order
        .iter()
        .filter_map(|id| {
            let label = state_ref.profiles.get(id)?.label.clone();
            let checked = *id == active_profile;
            let id = id.clone();
            Some(menu_row(
                state,
                &popover_state,
                format!("profile-{id}"),
                label,
                checked,
                move |state, _| state.active_profile = id.clone(),
            ))
        })
        .collect();

    let scenario_rows: Vec<_> = state_ref
        .scenarios
        .iter()
        .map(|scenario| {
            let checked = scenario.id == active_scenario;
            let id = scenario.id.clone();
            menu_row(
                state,
                &popover_state,
                format!("scenario-{}", scenario.id),
                scenario.label.clone(),
                checked,
                move |state, _| state.active_scenario = id.clone(),
            )
        })
        .collect();

    v_flex()
        .py_1()
        .min_w(px(180.))
        .child(section_header("PROFILE"))
        .children(profile_rows)
        // Divider between the two sections.
        .child(div().mx_2().my_1().h(px(1.)).bg(tinted(theme::TEXT, 0.12)))
        .child(section_header("SCENARIOS"))
        .children(scenario_rows)
}

/// Muted 10px caps header of a launch-menu section.
fn section_header(label: &'static str) -> Div {
    div()
        .px_2()
        .py_1()
        .text_size(px(10.))
        .text_color(theme::MUTED)
        .child(label)
}

/// One selectable launch-menu row: a check glyph slot first (always
/// reserved, so labels align across rows), then the label. Clicking applies
/// `pick` to [`StudioState`] and dismisses the popover.
fn menu_row(
    state: &Entity<StudioState>,
    popover: &WeakEntity<PopoverState>,
    el_id: String,
    label: String,
    checked: bool,
    pick: impl Fn(&mut StudioState, &mut Context<StudioState>) + Clone + 'static,
) -> Stateful<Div> {
    // Owned handles: the click closure must be 'static.
    let state = state.clone();
    let popover = popover.clone();
    h_flex()
        .id(ElementId::Name(el_id.into()))
        .h(px(26.))
        .px_2()
        .gap_2()
        .items_center()
        .rounded(px(4.))
        .text_size(px(13.))
        .text_color(if checked {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        })
        .hover(|this| this.bg(theme::HOVER))
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            let pick = pick.clone();
            state.update(cx, |state, cx| pick(state, cx));
            if let Some(popover) = popover.upgrade() {
                popover.update(cx, |state, cx| state.dismiss(window, cx));
            }
        })
        .child(
            div()
                .w(px(14.))
                .flex_shrink_0()
                .text_color(theme::ACCENT)
                .when(checked, |this| {
                    this.child(fa::sized(fa::CHECK, px(12.)))
                }),
        )
        .child(label)
}

/// Linear mix of two colors, `b` weighted by `t` (egui `tinted` layers).
fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    hsla(
        a.h + (b.h - a.h) * t,
        a.s + (b.s - a.s) * t,
        a.l + (b.l - a.l) * t,
        a.a + (b.a - a.a) * t,
    )
}

#[cfg(test)]
mod tests {
    use crate::topbar::icon_fit_size;

    #[test]
    fn small_icon_frames_upscale_to_the_slot() {
        // The ohmyblock `game.ico`: a single 16x16 frame → exactly 36x36,
        // so `ObjectFit::ScaleDown` paints it at full slot size.
        assert_eq!(icon_fit_size(16, 16), (36, 36));
        assert_eq!(icon_fit_size(32, 32), (36, 36));
    }

    #[test]
    fn non_square_frames_upscale_preserving_the_ratio() {
        // Aspect ratio preserved, longest edge scaled to the slot edge.
        assert_eq!(icon_fit_size(16, 8), (36, 18));
    }

    #[test]
    fn frames_at_or_above_the_slot_stay_put() {
        // Down-scaling is `ObjectFit::ScaleDown`'s job; the decoder only
        // upscales.
        assert_eq!(icon_fit_size(40, 40), (40, 40));
        assert_eq!(icon_fit_size(64, 64), (64, 64));
        assert_eq!(icon_fit_size(32, 48), (32, 48));
        assert_eq!(icon_fit_size(256, 128), (256, 128));
    }
}
