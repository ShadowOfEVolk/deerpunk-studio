//! Puffin panel (center tab): the in-studio profiler — a gpui port of the
//! vendored `third_party/puffin_egui` (`ProfilerUi`): connects to the
//! engine's `puffin_http` server, renders the flamegraph canvas
//! (zoom/pan/hover tooltips/merge view) and the scope stats table.
//!
//! The pure aggregation/geometry logic lives in [`crate::puffin_flame`]
//! (ported from the vendor's lib.rs/flamegraph.rs/stats.rs/filter.rs) so it
//! is unit-testable without gpui. Data source: `puffin_http::Client` feeds a
//! `puffin::FrameView` on its own thread; this panel snapshots the latest
//! `max_num_latest` frames whenever the newest frame index changes (the
//! 50 ms `StudioState` pump drives re-renders), so steady-state frames cost
//! nothing. Panning/zooming never re-aggregates: the canvas maps
//! nanosecond ranges to pixels at paint time.

use std::sync::Arc;
use std::time::Instant;

use deerpunk_studio::runner::RunState;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Sizable, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use puffin::{NanoSecond, ScopeCollection, ScopeDetails};

use crate::fa;
use crate::puffin_flame;
use crate::state::StudioState;
use crate::theme;

/// Default profiler socket, same as the engine's `puffin_http::Server`.
const DEFAULT_ADDR: &str = "127.0.0.1:8585";

/// Flamegraph canvas font (vendor Body ~15px → we use the studio mono 11px).
const FLAME_FONT_SIZE: Pixels = px(11.);
/// Line box for shaped flame text.
const FLAME_LINE_HEIGHT: Pixels = px(14.);
/// Vendor `dark_canvas` background.
const FLAME_BG: u32 = 0x15121b;
/// Vendor `HOVER_COLOR`.
const HOVER_RGBA: Rgba = Rgba {
    r: 0.8,
    g: 0.8,
    b: 0.8,
    a: 1.0,
};

#[derive(Copy, Clone, PartialEq, Eq)]
enum View {
    Flamegraph,
    Stats,
}

/// Everything the flame/stats views need for one selection: the frames and
/// the scope-name collection (owned clone so paint never locks the client).
struct Snapshot {
    selected: Arc<puffin_flame::SelectedFrames>,
    collection: ScopeCollection,
}

/// First soft-retry delay after a failed attempt while a project is open.
const RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(1);
/// Cap for the exponential soft-retry backoff (attempts double: 1s, 2s, …).
const RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// Pure trigger policy for puffin auto-connect, extracted so the logic is
/// unit-testable without gpui. Decides when the panel should create a
/// `puffin_http` client towards [`DEFAULT_ADDR`] (or the user-typed address):
///
/// - entering [`RunState::Running`] attempts immediately (once per run; the
///   client itself retries on its thread until the engine's server accepts);
/// - a project being open also triggers an attempt (engine started outside
///   the studio), then soft-retries while the project stays open: exponentially
///   backing off from [`RETRY_BASE`] to [`RETRY_MAX`] so we never spam;
/// - a manual Disconnect suppresses all auto attempts until the next
///   Running entry or a project (re)open re-arms them.
struct AutoConnectPolicy {
    last_run_state: Option<RunState>,
    last_project_open: bool,
    manual_disconnect: bool,
    last_attempt: Option<std::time::Instant>,
    attempts: u32,
}

impl AutoConnectPolicy {
    fn new() -> Self {
        Self {
            last_run_state: None,
            last_project_open: false,
            manual_disconnect: false,
            last_attempt: None,
            attempts: 0,
        }
    }

    /// Backoff for the next soft retry (`attempts` consecutive attempts so far).
    fn retry_delay(attempts: u32) -> std::time::Duration {
        RETRY_BASE
            .checked_mul(1u32.checked_shl(attempts.min(5)).unwrap_or(32))
            .unwrap_or(RETRY_MAX)
            .min(RETRY_MAX)
    }

    /// Evaluate one `StudioState` observation. Returns `true` when a connect
    /// attempt should be made now (`has_client` = a client already exists).
    fn evaluate(
        &mut self,
        run_state: &RunState,
        project_open: bool,
        has_client: bool,
        now: std::time::Instant,
    ) -> bool {
        let run_entry = *run_state == RunState::Running
            && self.last_run_state.as_ref() != Some(run_state);
        let project_entry = project_open && !self.last_project_open;

        // Fresh run or freshly opened project re-arms suppressed attempts.
        if run_entry || project_entry {
            self.manual_disconnect = false;
            self.attempts = 0;
        }
        if run_entry {
            self.last_attempt = None;
        }
        self.last_run_state = Some(run_state.clone());
        self.last_project_open = project_open;

        // While Running: one immediate attempt per run (client self-retries).
        // Otherwise: retry while a project is open, rate-limited by backoff.
        let want = if *run_state == RunState::Running {
            self.last_attempt.is_none()
        } else {
            project_open
        };
        let cooldown_ok = match self.last_attempt {
            None => true,
            Some(t) => now.duration_since(t) >= Self::retry_delay(self.attempts),
        };
        if !want || !cooldown_ok || has_client || self.manual_disconnect {
            return false;
        }
        self.last_attempt = Some(now);
        self.attempts = self.attempts.saturating_add(1);
        true
    }
}

pub struct PuffinPanel {
    focus_handle: FocusHandle,
    /// Observed for the 50 ms pump notifications (live frame updates).
    /// Never read directly; the field keeps the subscription meaningful.
    #[allow(dead_code)]
    state: Entity<StudioState>,
    addr_input: Option<Entity<InputState>>,
    filter_input: Option<Entity<InputState>>,
    table: Option<Entity<TableState<StatsDelegate>>>,

    /// `puffin_http` client; dropping it stops its receiver thread.
    client: Option<puffin_http::Client>,
    /// Auto-connect trigger policy (pure, unit-testable).
    policy: AutoConnectPolicy,
    view: View,
    /// Frozen selection (vendor `Paused`); `None` = live latest frames.
    paused: Option<Arc<Snapshot>>,
    /// Latest live snapshot, rebuilt when the newest frame index changes.
    snapshot: Option<Arc<Snapshot>>,
    cached_index: Option<u64>,
    /// Vendor resets the view when the selected frame count changes.
    last_num_frames: Option<usize>,
    /// Vendor "Max latest frames to show".
    max_num_latest: usize,
    flame: puffin_flame::FlameOptions,
    /// Pre-walked canvas geometry for the active options.
    layout: Option<Arc<puffin_flame::FlameLayout>>,

    /// Stats caption ("N unique scopes, … kB …"), rebuilt with the rows.
    stats_caption: Option<String>,
    /// Dedup key of the last `sync_stats` (snapshot identity + filter).
    last_stats_key: Option<(usize, String)>,

    // Interaction state (canvas-space mouse, drag bookkeeping).
    mouse: Option<Point<Pixels>>,
    last_mouse: Option<Point<Pixels>>,
    drag_accum: f32,
    /// Canvas element bounds, stored by the paint pass (no notify).
    flame_bounds: Bounds<Pixels>,
}

impl PuffinPanel {
    pub fn new(state: Entity<StudioState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |this, state, cx| this.observe_state(&state, cx))
            .detach();
        Self {
            focus_handle: cx.focus_handle(),
            state,
            addr_input: None,
            filter_input: None,
            table: None,
            client: None,
            policy: AutoConnectPolicy::new(),
            view: View::Flamegraph,
            paused: None,
            snapshot: None,
            cached_index: None,
            last_num_frames: None,
            max_num_latest: 1,
            flame: puffin_flame::FlameOptions::default(),
            layout: None,
            stats_caption: None,
            last_stats_key: None,
            mouse: None,
            last_mouse: None,
            drag_accum: 0.,
            flame_bounds: Bounds::default(),
        }
    }

    // -- inputs ------------------------------------------------------------

    fn ensure_addr_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = &self.addr_input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(DEFAULT_ADDR));
        cx.subscribe_in(&input, window, |_, _, _: &InputEvent, _, _| {})
            .detach();
        self.addr_input = Some(input.clone());
        input
    }

    fn ensure_filter_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = &self.filter_input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("scope filter"));
        cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                let value = input.read(cx).value().to_string();
                this.set_filter(value, cx);
            }
        })
        .detach();
        self.filter_input = Some(input.clone());
        input
    }

    fn addr(&self, cx: &App) -> String {
        let typed = self
            .addr_input
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if typed.is_empty() {
            DEFAULT_ADDR.to_string()
        } else {
            typed
        }
    }

    fn set_filter(&mut self, filter: String, cx: &mut Context<Self>) {
        let normalized = filter.to_lowercase();
        if self.flame.filter.as_str() == normalized {
            return;
        }
        self.flame.filter.set(filter);
        self.sync_stats(cx);
        cx.notify();
    }

    // -- data --------------------------------------------------------------

    /// Auto-connect hook on `StudioState` changes: attempt a `puffin_http`
    /// connection when the engine enters [`RunState::Running`] (one attempt
    /// per run — the client retries on its own thread) or while a project is
    /// open without a client (engine started outside the studio), with an
    /// exponential soft-retry backoff. A manual Disconnect is respected until
    /// the next run entry or project (re)open; a manual Connect re-enables
    /// automation. See [`AutoConnectPolicy`] for the exact trigger rules.
    fn observe_state(&mut self, state: &Entity<StudioState>, cx: &mut Context<Self>) {
        let s = state.read(cx);
        let run_state = s.run_state.clone();
        let project_open = s.project.is_some();
        if self.policy.evaluate(&run_state, project_open, self.client.is_some(), Instant::now()) {
            let addr = self.addr(cx);
            self.connect(addr, cx);
            return; // connect() already notified
        }
        cx.notify();
    }

    /// Active selection: paused snapshot wins over the live one.
    fn active(&self) -> Option<Arc<Snapshot>> {
        self.paused.clone().or_else(|| self.snapshot.clone())
    }

    /// Snapshot the latest frames when the newest frame index changed
    /// (vendor `select_latest_frames` + `AvailableFrames::latest`).
    fn refresh_snapshot(&mut self, cx: &mut Context<Self>) {
        if self.paused.is_some() {
            return;
        }
        enum Outcome {
            Clear,
            Unchanged,
            Fresh {
                selected: Arc<puffin_flame::SelectedFrames>,
                collection: ScopeCollection,
                latest: u64,
            },
        }

        // Compute under the frame-view lock without mutating `self`.
        let outcome = if let Some(client) = self.client.as_ref() {
            let fv = client.frame_view();
            if fv.is_empty() {
                Outcome::Clear
            } else {
                let latest = fv.latest_frame().expect("non-empty").frame_index();
                if self.cached_index == Some(latest) {
                    Outcome::Unchanged
                } else {
                    let frames: Vec<_> = fv
                        .latest_frames(self.max_num_latest)
                        .filter_map(|frame| frame.unpacked().ok())
                        .collect();
                    match puffin_flame::SelectedFrames::try_from_iter(
                        fv.scope_collection(),
                        frames.into_iter(),
                    ) {
                        None => Outcome::Unchanged,
                        Some(selected) => Outcome::Fresh {
                            selected: Arc::new(selected),
                            collection: fv.scope_collection().clone(),
                            latest,
                        },
                    }
                }
            }
        } else {
            Outcome::Clear
        };

        match outcome {
            Outcome::Unchanged => {}
            Outcome::Clear => {
                if self.snapshot.is_some() {
                    self.snapshot = None;
                    self.layout = None;
                    self.cached_index = None;
                    self.last_num_frames = None;
                    self.sync_stats(cx);
                }
            }
            Outcome::Fresh {
                selected,
                collection,
                latest,
            } => {
                // vendor ui(): reset view if the number of selected frames
                // changed (and we are not in merge view), or on first view.
                let num_frames = selected.frames.len();
                if self.last_num_frames != Some(num_frames) && !self.flame.merge_scopes
                    || self.flame.canvas_width_ns <= 0.0
                {
                    self.flame.reset_view(selected.raw_range_ns);
                }
                self.last_num_frames = Some(num_frames);
                self.cached_index = Some(latest);
                let layout = puffin_flame::FlameLayout::build(&selected, &self.flame);
                self.layout = Some(Arc::new(layout));
                self.snapshot = Some(Arc::new(Snapshot {
                    selected,
                    collection,
                }));
                self.sync_stats(cx);
            }
        }
    }

    /// Re-walk the canvas geometry after an options change (merge toggle,
    /// thread collapse/visibility, thread sorting).
    fn rebuild_layout(&mut self) {
        let Some(snapshot) = self.active() else {
            return;
        };
        if self.flame.canvas_width_ns <= 0.0 {
            let range = if self.flame.merge_scopes {
                snapshot.selected.merged_range_ns
            } else {
                snapshot.selected.raw_range_ns
            };
            self.flame.reset_view(range);
        }
        self.layout = Some(Arc::new(puffin_flame::FlameLayout::build(
            &snapshot.selected,
            &self.flame,
        )));
    }

    // -- toolbar actions -----------------------------------------------------

    /// Copy the flamegraph of all visible threads as indented text.
    fn copy_frame(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.active() else {
            return;
        };
        let opts = self.flame.clone();
        let text = puffin_flame::frame_trace_text(&snapshot.selected, &opts, &snapshot.collection);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn connect(&mut self, addr: String, cx: &mut Context<Self>) {
        self.client = Some(puffin_http::Client::new(addr));
        self.policy.manual_disconnect = false;
        self.paused = None;
        self.snapshot = None;
        self.layout = None;
        self.cached_index = None;
        self.last_num_frames = None;
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.client = None;
        // A manual disconnect wins over auto-connect until the next run or
        // a project (re)open.
        self.policy.manual_disconnect = true;
        self.paused = None;
        self.snapshot = None;
        self.layout = None;
        self.cached_index = None;
        self.last_num_frames = None;
        self.sync_stats(cx);
        cx.notify();
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if self.paused.is_some() {
            self.paused = None;
        } else if let Some(snapshot) = self.snapshot.clone() {
            // vendor: pause on the latest frame(s) currently selected.
            self.paused = Some(snapshot);
        }
        cx.notify();
    }

    fn set_merge_scopes(&mut self, on: bool, cx: &mut Context<Self>) {
        let num_frames = self.active().map(|s| s.selected.frames.len()).unwrap_or(0);
        self.flame.merge_scopes = on;
        self.rebuild_layout();
        // vendor: toggling merge with several frames changes the view
        // drastically — reset the zoom.
        if num_frames > 1 {
            let range = self
                .active()
                .map(|s| {
                    if on {
                        s.selected.merged_range_ns
                    } else {
                        s.selected.raw_range_ns
                    }
                })
                .unwrap_or((0, 1));
            self.flame.reset_view(range);
            self.rebuild_layout();
        }
        cx.notify();
    }

    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        cx.notify();
    }

    fn step_frames(&mut self, delta: isize, cx: &mut Context<Self>) {
        let next = (self.max_num_latest as isize + delta).clamp(1, 100) as usize;
        if next != self.max_num_latest {
            self.max_num_latest = next;
            self.cached_index = None; // force re-snapshot
            cx.notify();
        }
    }

    fn zoom(&mut self, factor: f64, center_px: Option<f32>, cx: &mut Context<Self>) {
        let width_px = self.flame_bounds.size.width.as_f32();
        if self.flame.canvas_width_ns <= 0.0 || width_px <= 0.0 {
            return;
        }
        let center = center_px.unwrap_or(width_px / 2.0);
        let f = factor.clamp(0.05, 20.0);
        // Keep the canvas range sane: 1 ns resolution floor, and zooming out
        // past the session scale is pointless (the timeline grid degenerates).
        self.flame.canvas_width_ns = (self.flame.canvas_width_ns / f).clamp(1.0, 1e12);
        self.flame.sideways_pan_px = (self.flame.sideways_pan_px - center) * f as f32 + center;
        cx.notify();
    }

    fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.active() else {
            return;
        };
        let range = if self.flame.merge_scopes {
            snapshot.selected.merged_range_ns
        } else {
            snapshot.selected.raw_range_ns
        };
        self.flame.reset_view(range);
        self.rebuild_layout();
        cx.notify();
    }

    fn zoom_to_scope(&mut self, entry_ix: usize, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.clone() else {
            return;
        };
        let Some(entry) = layout.scopes.get(entry_ix) else {
            return;
        };
        let width_px = self.flame_bounds.size.width.as_f32();
        if width_px <= 0.0 {
            return;
        }
        let relative = (
            entry.start_ns - layout.start_ns,
            entry.end_ns - layout.start_ns,
        );
        self.flame.zoom_to(relative, width_px, 0);
        cx.notify();
    }

    fn toggle_thread_collapse(&mut self, thread_ix: usize, cx: &mut Context<Self>) {
        let Some(layout) = &self.layout else {
            return;
        };
        let Some(thread) = layout.threads.get(thread_ix) else {
            return;
        };
        let name = thread.info.name.clone();
        if self.flame.collapsed_threads.contains(&name) {
            self.flame.collapsed_threads.remove(&name);
        } else {
            self.flame.collapsed_threads.insert(name);
        }
        self.rebuild_layout();
        cx.notify();
    }

    // -- stats table ---------------------------------------------------------

    fn ensure_table(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TableState<StatsDelegate>> {
        if let Some(table) = &self.table {
            return table.clone();
        }
        // egui grid parity: fixed rows, sortable by header click, resizable
        // columns, no selection.
        let table = cx.new(|cx| {
            TableState::new(StatsDelegate::default(), window, cx)
                .row_selectable(false)
                .sortable(true)
                .col_movable(false)
        });
        self.table = Some(table.clone());
        table
    }

    /// Rebuild the stats rows from the active selection + current filter.
    /// Skipped when neither changed (the 50 ms pump re-renders otherwise).
    fn sync_stats(&mut self, cx: &mut Context<Self>) {
        let Some(table) = &self.table else {
            return;
        };
        let snapshot = self.active();
        let filter = self.flame.filter.clone();
        let key = (
            snapshot
                .as_ref()
                .map(|s| std::ptr::from_ref(s.as_ref()) as usize)
                .unwrap_or(0),
            filter.as_str().to_string(),
        );
        if self.last_stats_key.as_ref() == Some(&key) {
            return;
        }
        self.last_stats_key = Some(key);

        let mut caption = None;
        let rows = snapshot
            .map(|snapshot| {
                let stats = puffin_flame::collect_scope_stats(&snapshot.selected.frames);
                let (bytes, ns) = puffin_flame::stats_totals(&stats);
                caption = Some(format!(
                    "{} unique scopes, {:.1} kB, covering {:.1} ms over {} thread(s)",
                    stats.len(),
                    bytes as f32 * 1e-3,
                    ns as f32 * 1e-6,
                    snapshot.selected.threads.len(),
                ));
                puffin_flame::build_stat_rows(&snapshot.collection, &stats, &filter)
            })
            .unwrap_or_default();

        table.update(cx, |table, cx| {
            table.delegate_mut().set_rows(rows);
            table.refresh(cx);
            cx.notify();
        });
        self.stats_caption = caption;
    }

    // -- flame canvas ----------------------------------------------------------

    /// The scrollable flamegraph: a `canvas` element of the layout's content
    /// height inside a vertical-scroll container. The canvas itself is
    /// wrapped in an inner div that owns wheel/pinch handling: its bubble
    /// listeners run before the scroll container's internal handler, so the
    /// canvas can claim the wheel for zoom/pan (`on_canvas_scroll` stops
    /// propagation). The container keeps the mouse/click listeners and the
    /// right-click copy menu (vendor canvas interactions).
    fn flame_area(
        &self,
        snapshot: Arc<Snapshot>,
        layout: Arc<puffin_flame::FlameLayout>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let opts = self.flame.clone();
        let mouse = self.mouse;
        let hovered = self.hovered();
        let content_height = px(layout.height.max(120.));
        let panel = cx.entity().downgrade();

        div()
            .id("puffin-flame-scroll")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .overflow_x_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag_accum = 0.),
            )
            .on_mouse_move(
                cx.listener(|this, ev: &MouseMoveEvent, _, cx| this.on_canvas_mouse_move(ev, cx)),
            )
            .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| this.on_canvas_click(ev, cx)))
            .on_hover({
                let panel = panel.clone();
                move |hovered: &bool, _, cx| {
                    if !*hovered && let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| {
                            if this.mouse.take().is_some() {
                                this.last_mouse = None;
                                cx.notify();
                            }
                        });
                    }
                }
            })
            .child(
                div()
                    .id("puffin-flame-canvas")
                    .h(content_height)
                    .w_full()
                    .on_scroll(
                        cx.listener(|this, ev: &ScrollEvent, _, cx| this.on_canvas_scroll(ev, cx)),
                    )
                    .on_pinch(
                        cx.listener(|this, ev: &PinchEvent, _, cx| this.on_canvas_pinch(ev, cx)),
                    )
                    .child(
                        canvas(
                            |_, _, _| {},
                            {
                                let snapshot = snapshot.clone();
                                let layout = layout.clone();
                                let opts = opts.clone();
                                let panel = panel.clone();
                                move |bounds, _, window, cx| {
                                    // Remember where the canvas lives for hit-testing next
                                    // frame; no notify (paint-phase mutation only).
                                    let _ = panel.update(cx, |this, _| {
                                        this.flame_bounds = bounds;
                                    });
                                    paint_flamegraph(
                                        &bounds, &snapshot, &layout, &opts, mouse, hovered, window,
                                        cx,
                                    );
                                }
                            },
                        )
                        .h_full()
                        .w_full(),
                    ),
            )
            .context_menu({
                let panel = panel.clone();
                move |menu, _, cx| flame_context_menu(&panel, menu, cx)
            })
            .into_any_element()
    }

    fn hovered(&self) -> Option<usize> {
        self.mouse?;
        let layout = self.layout.as_ref()?;
        let opts = &self.flame;
        let mouse = self.canvas_mouse()?;
        layout.hit_test(
            opts,
            self.flame_bounds.size.width.as_f32(),
            (mouse.x.as_f32(), mouse.y.as_f32()),
        )
    }

    fn canvas_mouse(&self) -> Option<Point<Pixels>> {
        let mouse = self.mouse?;
        Some(point(
            mouse.x - self.flame_bounds.origin.x,
            mouse.y - self.flame_bounds.origin.y,
        ))
    }

    fn on_canvas_mouse_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let pos = ev.position;
        let inside = self.flame_bounds.contains(&pos);

        if let Some(last) = self.last_mouse {
            let dx = pos.x - last.x;
            let dy = pos.y - last.y;
            match ev.pressed_button {
                Some(MouseButton::Left) => {
                    // vendor: sideways pan with left drag
                    if inside {
                        self.flame.sideways_pan_px += dx.as_f32();
                    }
                }
                Some(MouseButton::Right) => {
                    // vendor: zoom with secondary-button drag
                    if inside {
                        let factor = (dy.as_f32() * 0.01).exp().clamp(0.5, 2.0);
                        self.zoom(
                            (1.0 / factor) as f64,
                            Some(pos.x.as_f32() - self.flame_bounds.origin.x.as_f32()),
                            cx,
                        );
                    }
                }
                _ => {}
            }
            self.drag_accum += dx.as_f32().abs() + dy.as_f32().abs();
        }
        self.last_mouse = Some(pos);
        self.mouse = inside.then_some(pos);
        cx.notify();
    }

    /// Wheel over the canvas: delta Y zooms anchored at the ns under the
    /// cursor; shift+wheel and the horizontal delta pan sideways in pixels.
    /// The canvas owns the wheel — stop propagation so the vertical scroll
    /// container (inner div bubble handlers run first) does not also scroll.
    fn on_canvas_scroll(&mut self, ev: &ScrollEvent, cx: &mut Context<Self>) {
        if !self.flame_bounds.contains(&ev.position) {
            return;
        }
        let (dx, dy) = match ev.delta {
            ScrollDelta::Pixels(d) => (d.x.as_f32(), d.y.as_f32()),
            ScrollDelta::Lines(d) => (d.x * 16., d.y * 16.),
        };
        if ev.modifiers.shift {
            self.flame.sideways_pan_px += dx + dy;
            cx.notify();
        } else {
            if dx != 0.0 {
                self.flame.sideways_pan_px += dx;
            }
            if dy != 0.0 {
                // vendor zoom math: anchor at the cursor's canvas x.
                let factor = (-dy * 0.01).exp().clamp(0.5, 2.0);
                let center = ev.position.x.as_f32() - self.flame_bounds.origin.x.as_f32();
                self.zoom(factor as f64, Some(center), cx);
                return; // zoom() notified
            }
            if dx != 0.0 {
                cx.notify();
            }
        }
        cx.stop_propagation();
    }

    /// Trackpad pinch: same anchor-at-cursor zoom as the wheel.
    fn on_canvas_pinch(&mut self, ev: &PinchEvent, cx: &mut Context<Self>) {
        if !self.flame_bounds.contains(&ev.position) {
            return;
        }
        let factor = (1.0 + ev.delta).clamp(0.5, 2.0);
        let center = ev.position.x.as_f32() - self.flame_bounds.origin.x.as_f32();
        self.zoom(factor as f64, Some(center), cx);
    }

    fn on_canvas_click(&mut self, ev: &ClickEvent, cx: &mut Context<Self>) {
        // A drag that ended inside the element also produces a click.
        if self.drag_accum > 4.0 {
            self.drag_accum = 0.;
            return;
        }
        let Some(layout) = self.layout.clone() else {
            return;
        };
        let Some(mouse) = self.canvas_mouse() else {
            return;
        };
        let double = matches!(ev, ClickEvent::Mouse(e) if e.up.click_count >= 2);
        let mods = ev.modifiers();

        if double {
            // vendor: double-click resets the view.
            self.reset_zoom(cx);
            return;
        }
        if let Some(thread_ix) = layout.hit_thread_label((mouse.x.as_f32(), mouse.y.as_f32())) {
            // vendor paint_thread_info: click the label to collapse.
            self.toggle_thread_collapse(thread_ix, cx);
            return;
        }
        if mods.control || mods.platform {
            // assignment: ctrl/cmd + click = zoom out.
            self.zoom(2.0, Some(mouse.x.as_f32()), cx);
            return;
        }
        if let Some(entry_ix) = self.hovered() {
            // vendor: click a scope to zoom to it.
            self.zoom_to_scope(entry_ix, cx);
        }
    }

    // -- misc ---------------------------------------------------------------

    /// Vendor `frames_info_ui`.
    fn frames_info(&self, snapshot: &Snapshot) -> String {
        let selection = &snapshot.selected;
        let mut sum_ns = 0;
        let mut sum_scopes = 0;
        for frame in &selection.frames {
            let (min_ns, max_ns) = frame.range_ns();
            sum_ns += max_ns - min_ns;
            sum_scopes += frame.meta.num_scopes;
        }
        let n = selection.frames.len();
        let frame_indices = if n == 1 {
            format!("frame #{}", selection.frames[0].frame_index())
        } else if n as u64
            == selection.frames.last().expect("non-empty").frame_index()
                - selection.frames.first().expect("non-empty").frame_index()
                + 1
        {
            format!(
                "{} frames (#{} - #{})",
                n,
                selection.frames.first().expect("non-empty").frame_index(),
                selection.frames.last().expect("non-empty").frame_index()
            )
        } else {
            format!("{n} frames")
        };
        format!(
            "Showing {frame_indices}, {:.1} ms, {} threads, {sum_scopes} scopes.",
            sum_ns as f64 * 1e-6,
            selection.threads.len(),
        )
    }

    /// Vendor overhead warning: many scopes ≈ measurable overhead.
    fn overhead_warning(&self, snapshot: &Snapshot) -> Option<String> {
        if snapshot.selected.frames.len() != 1 {
            return None;
        }
        let num_scopes = snapshot.selected.frames[0].meta.num_scopes;
        let overhead_ms = num_scopes as f64 * 1.0e-6 * 200.0;
        (overhead_ms > 1.0).then(|| {
            format!(
                "There are {num_scopes} scopes in this frame, which adds around ~{overhead_ms:.1} ms of overhead. \
                 Use the Table view to find scopes that are triggered often."
            )
        })
    }
}

impl EventEmitter<PanelEvent> for PuffinPanel {}

impl Focusable for PuffinPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for PuffinPanel {
    fn panel_name(&self) -> &'static str {
        "PuffinPanel"
    }
}

impl Panel for PuffinPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Puffin"
    }
}

impl Render for PuffinPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let addr_input = self.ensure_addr_input(window, cx);
        let filter_input = self.ensure_filter_input(window, cx);
        let table = self.ensure_table(window, cx);
        self.refresh_snapshot(cx);

        let snapshot = self.active();
        let layout = self.layout.clone();
        let panel = cx.entity().downgrade();

        let connected = self.client.as_ref().map(|c| c.connected());
        let addr = self.addr(cx);
        let (dot, status_text) = match connected {
            Some(true) => (theme::GREEN, format!("connected {addr}")),
            Some(false) => (theme::YELLOW, format!("waiting for game at {addr}…")),
            None => (theme::MUTED, "offline".to_string()),
        };
        let is_flamegraph = self.view == View::Flamegraph;
        let merge = self.flame.merge_scopes;
        let paused = self.paused.is_some();
        let max_latest = self.max_num_latest;
        let sort_by_name = self.flame.sort_by_name;

        // Connection row -------------------------------------------------
        let connect_button = match self.client {
            None => Button::new("puffin-connect")
                .child(fa::sized(fa::PLUG, px(12.)))
                .child("Connect")
                .on_click({
                    let panel = panel.clone();
                    move |_, _, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |this, cx| {
                                let addr = this.addr(cx);
                                this.connect(addr, cx);
                            });
                        }
                    }
                }),
            Some(_) => Button::new("puffin-disconnect")
                .child(fa::sized(fa::PLUG_CIRCLE_XMARK, px(12.)))
                .child("Disconnect")
                .on_click({
                    let panel = panel.clone();
                    move |_, _, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |this, cx| this.disconnect(cx));
                        }
                    }
                }),
        };

        // View toolbar ----------------------------------------------------
        let active_variant = || ButtonCustomVariant::new(cx).foreground(theme::ACCENT);
        let view_buttons = h_flex()
            .gap_1()
            .child(
                Button::new("puffin-view-flame")
                    .child(fa::sized(fa::FLAME, px(12.)))
                    .child("Flamegraph")
                    .when(is_flamegraph, |b| b.custom(active_variant()))
                    .on_click({
                        let panel = panel.clone();
                        move |_, _, cx| {
                            if let Some(panel) = panel.upgrade() {
                                panel.update(cx, |this, cx| this.set_view(View::Flamegraph, cx));
                            }
                        }
                    }),
            )
            .child(
                Button::new("puffin-view-stats")
                    .child(fa::sized(fa::DATABASE, px(12.)))
                    .child("Table")
                    .when(!is_flamegraph, |b| b.custom(active_variant()))
                    .on_click({
                        let panel = panel.clone();
                        move |_, _, cx| {
                            if let Some(panel) = panel.upgrade() {
                                panel.update(cx, |this, cx| this.set_view(View::Stats, cx));
                            }
                        }
                    }),
            );

        let merge_checkbox = Checkbox::new("puffin-merge")
            .label("Merge same-ID children")
            .checked(merge)
            .on_click({
                let panel = panel.clone();
                move |checked: &bool, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| this.set_merge_scopes(*checked, cx));
                    }
                }
            });

        let sort_button = Button::new("puffin-thread-sort")
            .label(if sort_by_name {
                "Sort: name"
            } else {
                "Sort: time"
            })
            .tooltip("Sort flamegraph threads (vendor Settings ▸ Sorting)")
            .on_click({
                let panel = panel.clone();
                move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| {
                            this.flame.sort_by_name = !this.flame.sort_by_name;
                            this.rebuild_layout();
                            cx.notify();
                        });
                    }
                }
            });

        let zoom_buttons = h_flex()
            .gap_1()
            .child(Button::new("puffin-zoom-out").label("−").on_click({
                let panel = panel.clone();
                move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| this.zoom(2.0, None, cx));
                    }
                }
            }))
            .child(Button::new("puffin-zoom-in").label("+").on_click({
                let panel = panel.clone();
                move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| this.zoom(0.5, None, cx));
                    }
                }
            }))
            .child(
                Button::new("puffin-zoom-reset")
                    .child(fa::sized(fa::ROTATE, px(12.)))
                    .tooltip("Reset zoom (vendor: double-click)")
                    .on_click({
                        let panel = panel.clone();
                        move |_, _, cx| {
                            if let Some(panel) = panel.upgrade() {
                                panel.update(cx, |this, cx| this.reset_zoom(cx));
                            }
                        }
                    }),
            );

        let pause_button = Button::new("puffin-pause")
            .when(paused, |b| b.child(fa::sized(fa::PLAY, px(12.))).child("Resume"))
            .when(!paused, |b| b.child(fa::sized(fa::STOP, px(12.))).child("Pause"))
            .tooltip("Freeze the current frame selection (vendor: space)")
            .on_click({
                let panel = panel.clone();
                move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| this.toggle_pause(cx));
                    }
                }
            });

        let copy_frame_button = Button::new("puffin-copy-frame")
            .child(fa::sized(fa::COPY, px(12.)))
            .child("Copy frame")
            .tooltip("Copy the flamegraph of all visible threads as indented text")
            .on_click({
                let panel = panel.clone();
                move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| this.copy_frame(cx));
                    }
                }
            });

        let frames_stepper = h_flex()
            .items_center()
            .gap_1()
            .text_sm()
            .text_color(theme::TEXT_DIM)
            .child(Button::new("puffin-frames-minus").label("−").on_click({
                let panel = panel.clone();
                move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| this.step_frames(-1, cx));
                    }
                }
            }))
            .child(format!("{max_latest} frames"))
            .child(Button::new("puffin-frames-plus").label("+").on_click({
                let panel = panel.clone();
                move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| this.step_frames(1, cx));
                    }
                }
            }));

        // Body ------------------------------------------------------------
        let body: AnyElement = if self.view == View::Stats {
            v_flex()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .text_sm()
                        .text_color(theme::TEXT_DIM)
                        .when_some(self.stats_caption.clone(), |row, caption| {
                            row.child(caption)
                        })
                        .when(self.stats_caption.is_none(), |row| {
                            row.child("No profiling data")
                        }),
                )
                .child(
                    div().flex_1().min_h_0().overflow_hidden().p_1().child(
                        DataTable::new(&table)
                            .stripe(true)
                            .bordered(false)
                            .with_size(Size::Size(px(24.))),
                    ),
                )
                .into_any_element()
        } else if let (Some(snapshot), Some(layout)) = (&snapshot, &layout) {
            let info = self.frames_info(snapshot);
            let warning = self.overhead_warning(snapshot);
            v_flex()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .text_sm()
                        .text_color(theme::TEXT_DIM)
                        .child(info)
                        .when_some(warning, |row, warning| {
                            row.child(div().text_color(theme::YELLOW).child(warning))
                        })
                        .child(
                            div().text_color(theme::FAINT).child(
                                "right-click: copy trace · wheel: zoom · \
                                 drag / shift-wheel: pan · double-click: reset",
                            ),
                        ),
                )
                .child(self.flame_area(snapshot.clone(), layout.clone(), cx))
                .into_any_element()
        } else {
            let text = match connected {
                Some(_) => "No profiling data — is the engine running with puffin scopes on?",
                None => "Offline — enter the engine profiler address and press Connect.",
            };
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme::TEXT_DIM)
                .child(text)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(theme::PANEL)
            // Connection row: status dot, status text, address, connect.
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::BORDER)
                    .child(div().size(px(10.)).rounded_full().bg(dot))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::TEXT_DIM)
                            .child(status_text),
                    )
                    .child(
                        div()
                            .w(px(160.))
                            .text_sm()
                            .child(Input::new(&addr_input).w_full()),
                    )
                    .child(connect_button),
            )
            // View toolbar: views, merge, filter, zoom, pause, frame count.
            .child(
                h_flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme::BORDER)
                    .child(view_buttons)
                    .child(merge_checkbox)
                    .child(sort_button)
                    .child(
                        div()
                            .w(px(150.))
                            .text_sm()
                            .child(Input::new(&filter_input).w_full()),
                    )
                    .child(zoom_buttons)
                    .child(pause_button)
                    .child(copy_frame_button)
                    .child(frames_stepper),
            )
            .child(body)
    }
}

// ---------------------------------------------------------------------------
// Flamegraph painting (vendor flamegraph.rs paint_* port)
// ---------------------------------------------------------------------------

/// Right-click menu on the flamegraph, built at open time from what is under
/// the cursor: a hovered scope offers its span/thread trace; anything offers
/// the whole-frame copy. Snapshots the text immediately so the menu content
/// matches what the user pointed at.
fn flame_context_menu(
    panel: &WeakEntity<PuffinPanel>,
    menu: PopupMenu,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(panel) = panel.upgrade() else {
        return menu;
    };
    let this = panel.read(cx);
    let opts = this.flame.clone();
    let snapshot = this.active();
    let layout = this.layout.clone();
    let mouse = this.canvas_mouse();
    let hovered = this.hovered();
    let thread_label = mouse
        .as_ref()
        .and_then(|m| layout.as_ref()?.hit_thread_label((m.x.as_f32(), m.y.as_f32())));

    let mut menu = menu;
    if let (Some(snapshot), Some(layout)) = (&snapshot, &layout) {
        let sel = &snapshot.selected;
        let collection = &snapshot.collection;

        if let Some(entry) = hovered.and_then(|ix| layout.scopes.get(ix))
            && let Some(text) = puffin_flame::span_trace_text(sel, &opts, collection, entry)
        {
            menu = menu.item(
                PopupMenuItem::new("Copy span trace").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                }),
            );
        }

        let thread_ix = thread_label
            .or_else(|| hovered.and_then(|ix| layout.scopes.get(ix).map(|e| e.thread_ix)));
        if let Some(thread_ix) = thread_ix
            && let Some(text) = puffin_flame::thread_trace_text(sel, &opts, collection, thread_ix)
        {
            menu = menu.item(
                PopupMenuItem::new("Copy thread trace").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                }),
            );
        }

        let text = puffin_flame::frame_trace_text(sel, &opts, collection);
        menu = menu.item(
            PopupMenuItem::new("Copy frame (all threads)").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }),
        );
    }
    menu
}

#[allow(clippy::too_many_arguments)]
fn paint_flamegraph(
    bounds: &Bounds<Pixels>,
    snapshot: &Snapshot,
    layout: &puffin_flame::FlameLayout,
    opts: &puffin_flame::FlameOptions,
    mouse: Option<Point<Pixels>>,
    hovered: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) {
    window.paint_quad(fill(*bounds, Hsla::from(rgb(FLAME_BG))));
    if opts.canvas_width_ns <= 0.0 || bounds.size.width.as_f32() <= 0.0 {
        return;
    }
    let canvas_w = bounds.size.width.as_f32();
    let x_of = |ns: NanoSecond| -> f32 {
        bounds.origin.x.as_f32()
            + opts.sideways_pan_px
            + canvas_w * (ns - layout.start_ns) as f32 / opts.canvas_width_ns as f32
    };

    paint_timeline(bounds, opts, window, cx);

    // Pass 1: thread separators + clickable "⏵/⏷ name" labels.
    for thread in &layout.threads {
        // Separator line above the thread label (vendor line_segment).
        let line_y = bounds.origin.y.as_f32() + thread.label_y() - 2.0;
        window.paint_quad(fill(
            Bounds {
                origin: point(bounds.origin.x, px(line_y)),
                size: size(bounds.size.width, px(1.)),
            },
            white(0.4),
        ));

        // Thread label: "⏷ name" on a dark plate; click toggles collapse.
        let glyph = if thread.collapsed { "⏵ " } else { "⏷ " };
        let label = format!("{glyph}{}", thread.info.name);
        let label_line = shaped(label.into(), FLAME_FONT_SIZE, white(0.9), window, cx);
        let label_origin = point(
            bounds.origin.x + px(2.),
            px(bounds.origin.y.as_f32() + thread.label_y()),
        );
        window.paint_quad(fill(
            Bounds {
                origin: label_origin,
                size: size(label_line.width() + px(4.), FLAME_LINE_HEIGHT),
            },
            black(0.55),
        ));
        let _ = label_line.paint(
            label_origin + point(px(2.), px(0.)),
            FLAME_LINE_HEIGHT,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    // Pass 2: scope rects in pre-order; `hovered` indexes the same
    // `layout.scopes` vec produced by hit_test.
    for (ix, entry) in layout.scopes.iter().enumerate() {
        let Some(thread) = layout.threads.get(entry.thread_ix) else {
            continue;
        };
        let details = snapshot.collection.fetch_by_id(&entry.id);
        paint_scope_rect(
            bounds,
            opts,
            thread.scopes_top_y,
            entry,
            hovered == Some(ix),
            details,
            &x_of,
            window,
            cx,
        );
    }

    // Tooltip last (vendor Tooltip::always_open at pointer).
    if let Some(mouse) = mouse
        && let Some(entry_ix) = hovered
        && let Some(entry) = layout.scopes.get(entry_ix)
    {
        let details = snapshot.collection.fetch_by_id(&entry.id);
        paint_tooltip(bounds, mouse, entry, details, opts, window, cx);
    }
}

fn paint_timeline(
    bounds: &Bounds<Pixels>,
    opts: &puffin_flame::FlameOptions,
    window: &mut Window,
    cx: &mut App,
) {
    let canvas_w = bounds.size.width.as_f32();
    let max_lines = ((canvas_w / 4.0).max(1.0)) as f64;
    let mut grid_spacing_ns = 1_000i64; // vendor grid_spacing_micros = 1
    while opts.canvas_width_ns / grid_spacing_ns as f64 > max_lines {
        grid_spacing_ns *= 10;
    }
    let scale = canvas_w / opts.canvas_width_ns as f32;

    let mut grid_ns = 0i64;
    loop {
        let line_x = bounds.origin.x.as_f32() + opts.sideways_pan_px + (grid_ns as f32 * scale);
        if line_x > bounds.origin.x.as_f32() + canvas_w {
            break;
        }
        if line_x >= bounds.origin.x.as_f32() {
            let big = grid_ns % (grid_spacing_ns * 100) == 0;
            let medium = grid_ns % (grid_spacing_ns * 10) == 0;
            let alpha = if big {
                0.5
            } else if medium {
                0.25
            } else {
                0.1
            };
            window.paint_quad(fill(
                Bounds {
                    origin: point(px(line_x), bounds.origin.y),
                    size: size(px(1.), bounds.size.height),
                },
                white(alpha),
            ));
            // Label the big/medium lines (vendor labels top + bottom).
            if big || medium {
                let text = puffin_flame::grid_text(grid_ns);
                let line = shaped(text.into(), FLAME_FONT_SIZE, white(0.5), window, cx);
                let origin = point(px(line_x + 4.), bounds.origin.y + px(1.));
                let _ = line.paint(origin, FLAME_LINE_HEIGHT, TextAlign::Left, None, window, cx);
            }
        }
        grid_ns += grid_spacing_ns;
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_scope_rect(
    bounds: &Bounds<Pixels>,
    opts: &puffin_flame::FlameOptions,
    scopes_top_y: f32,
    entry: &puffin_flame::ScopeEntry,
    is_hovered: bool,
    details: Option<&Arc<ScopeDetails>>,
    x_of: &impl Fn(NanoSecond) -> f32,
    window: &mut Window,
    cx: &mut App,
) {
    let start_x = x_of(entry.start_ns);
    let stop_x = x_of(entry.end_ns);
    // Offscreen cull (vendor culled by canvas intersection).
    if stop_x < bounds.origin.x.as_f32()
        || start_x > bounds.origin.x.as_f32() + bounds.size.width.as_f32()
    {
        return;
    }

    let top_y = bounds.origin.y.as_f32()
        + scopes_top_y
        + entry.depth as f32 * (puffin_flame::RECT_HEIGHT + puffin_flame::SPACING);
    let rect = Bounds {
        origin: point(px(start_x), px(top_y)),
        size: size(
            px((stop_x - start_x).max(0.1)),
            px(puffin_flame::RECT_HEIGHT),
        ),
    };

    let name = details
        .map(|d| d.name().to_string())
        .unwrap_or_else(|| "<missing>".to_string());

    let (r, g, b, a) = puffin_flame::color_from_duration(entry.duration_ns());
    let mut min_width = 1.0; // vendor Options::min_width
    let (fade_r, fade_g, fade_b) = if !opts.filter.is_empty() {
        if opts.filter.include(&name) {
            min_width *= 2.0; // keep it visible even when thin
            (r, g, b)
        } else {
            // fade non-matching scopes to highlight the rest (vendor lerp to black)
            (r * 0.075, g * 0.075, b * 0.075)
        }
    } else {
        (r, g, b)
    };
    let rect_color: Hsla = if is_hovered {
        HOVER_RGBA.into()
    } else {
        Rgba {
            r: fade_r,
            g: fade_g,
            b: fade_b,
            a,
        }
        .into()
    };

    let width = rect.size.width.as_f32();
    if width <= min_width {
        // vendor: draw thin scopes as min-width lines
        window.paint_quad(fill(
            Bounds {
                origin: point(px(start_x), rect.origin.y),
                size: size(px(min_width), rect.size.height),
            },
            rect_color,
        ));
    } else {
        window.paint_quad(quad(
            rect,
            px(2.),
            rect_color,
            px(0.),
            black(0.0),
            BorderStyle::default(),
        ));
        if is_hovered {
            window.paint_quad(outline(rect, black(0.35), BorderStyle::default()));
        }
    }

    // Label (vendor: text when wider than 32px, clipped to the rect).
    if width > 32.0 {
        let duration_ms = puffin_flame::to_ms(entry.duration_ns());
        let core = if entry.data.is_empty() {
            format!("{name} {duration_ms:6.3} ms")
        } else {
            // vendor: data shown un-escaped
            format!("{name} '{}' {duration_ms:6.3} ms", entry.data)
        };
        let text = if entry.merge_pieces > 0 {
            format!("{}× {core}", entry.merge_pieces)
        } else {
            core
        };
        let max_w = px(width - 8.0);
        let line = fitted(&text, max_w, FLAME_FONT_SIZE, black(0.85), window, cx);
        window.paint_layer(rect, |window| {
            let origin = point(rect.origin.x + px(4.), rect.origin.y + px(1.));
            let _ = line.paint(origin, FLAME_LINE_HEIGHT, TextAlign::Left, None, window, cx);
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_tooltip(
    bounds: &Bounds<Pixels>,
    mouse: Point<Pixels>,
    entry: &puffin_flame::ScopeEntry,
    details: Option<&Arc<ScopeDetails>>,
    opts: &puffin_flame::FlameOptions,
    window: &mut Window,
    cx: &mut App,
) {
    let mut lines: Vec<(String, Hsla)> = vec![(format!("id {}", entry.id.0), theme::TEXT)];
    if let Some(details) = details {
        lines.push((format!("function  {}", details.function_name), theme::TEXT));
        if let Some(scope_name) = &details.scope_name {
            lines.push((format!("scope     {scope_name}"), theme::TEXT));
        }
        if !details.file_path.is_empty() {
            lines.push((format!("location  {}", details.location()), theme::TEXT_DIM));
        }
        if !entry.data.is_empty() {
            lines.push((format!("data      {}", entry.data), theme::TEXT));
        }
        lines.push((
            format!("type      {}", details.scope_type().type_str()),
            theme::TEXT_DIM,
        ));
    }
    lines.push((
        format!(
            "duration  {:7.3} ms",
            puffin_flame::to_ms(entry.duration_ns())
        ),
        theme::TEXT,
    ));
    if entry.merge_pieces > 0 {
        lines.push((format!("pieces    {}", entry.merge_pieces), theme::TEXT_DIM));
        lines.push((format!("children  {}", entry.child_count), theme::TEXT_DIM));
    } else {
        lines.push((format!("children  {}", entry.child_count), theme::TEXT_DIM));
    }
    if opts.merge_scopes {
        lines.push(("merge view".into(), theme::FAINT));
    }

    let width = lines
        .iter()
        .map(|(l, _)| l.chars().count() as f32 * 6.6)
        .fold(0.0f32, f32::max)
        + 16.0;
    let height = lines.len() as f32 * (FLAME_LINE_HEIGHT.as_f32() + 2.0) + 8.0;
    let mut origin = point(mouse.x + px(14.), mouse.y + px(14.));
    if origin.x.as_f32() + width > bounds.origin.x.as_f32() + bounds.size.width.as_f32() {
        origin.x = px(mouse.x.as_f32() - width - 14.0);
    }
    if origin.y.as_f32() + height > bounds.origin.y.as_f32() + bounds.size.height.as_f32() {
        origin.y = px(mouse.y.as_f32() - height - 14.0);
    }
    let tooltip_bounds = Bounds {
        origin,
        size: size(px(width), px(height)),
    };
    window.paint_quad(quad(
        tooltip_bounds,
        px(4.),
        theme::PANEL_ALT,
        px(1.),
        theme::BORDER,
        BorderStyle::default(),
    ));
    for (ix, (text, color)) in lines.iter().enumerate() {
        let line = shaped(text.as_str().into(), FLAME_FONT_SIZE, *color, window, cx);
        let y = origin.y.as_f32() + 4.0 + ix as f32 * (FLAME_LINE_HEIGHT.as_f32() + 2.0);
        let _ = line.paint(
            point(origin.x + px(8.), px(y)),
            FLAME_LINE_HEIGHT,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

fn shaped(
    text: SharedString,
    font_size: Pixels,
    color: Hsla,
    window: &Window,
    cx: &App,
) -> ShapedLine {
    let font = Font {
        family: cx.theme().mono_font_family.clone(),
        ..Font::default()
    };
    let run = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(text, font_size, &[run], None)
}

/// Shaped line truncated with an ellipsis to fit `max_w` (assignment:
/// "текст метки с обрезкой (ellipsis)").
fn fitted(
    text: &str,
    max_w: Pixels,
    font_size: Pixels,
    color: Hsla,
    window: &Window,
    cx: &App,
) -> ShapedLine {
    let line = shaped(text.to_string().into(), font_size, color, window, cx);
    if line.width() <= max_w {
        return line;
    }
    let chars = text.chars().count().max(1) as f32;
    let avg = line.width().as_f32() / chars;
    let max_chars = ((max_w.as_f32() / avg.max(1.0)) as usize)
        .saturating_sub(1)
        .max(1);
    let cut: String = text.chars().take(max_chars).collect::<String>() + "…";
    shaped(cut.into(), font_size, color, window, cx)
}

fn white(alpha: f32) -> Hsla {
    hsla(0., 0., 1., alpha)
}

fn black(alpha: f32) -> Hsla {
    hsla(0., 0., 0., alpha)
}

// ---------------------------------------------------------------------------
// Stats table (vendor stats.rs view)
// ---------------------------------------------------------------------------

/// Stats table delegate: prebuilt rows (filtered + sorted), sort state owned
/// here so header clicks survive panel re-syncs.
#[derive(Default)]
struct StatsDelegate {
    rows: Arc<Vec<puffin_flame::StatRow>>,
    sort: puffin_flame::SortOrder,
}

impl StatsDelegate {
    fn set_rows(&mut self, rows: Vec<puffin_flame::StatRow>) {
        let mut rows = rows;
        puffin_flame::sort_rows(&mut rows, self.sort);
        self.rows = Arc::new(rows);
    }

    fn row(&self, ix: usize) -> Option<&puffin_flame::StatRow> {
        self.rows.get(ix)
    }
}

impl TableDelegate for StatsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        8
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("location", "Location")
                .width(px(220.))
                .min_width(px(90.))
                .sortable(),
            1 => Column::new("function", "Function Name")
                .width(px(220.))
                .min_width(px(90.))
                .sortable(),
            2 => Column::new("scope_name", "Scope Name")
                .width(px(180.))
                .min_width(px(80.))
                .sortable(),
            3 => Column::new("count", "Count")
                .width(px(70.))
                .min_width(px(48.))
                .sortable(),
            4 => Column::new("size", "Size")
                .width(px(80.))
                .min_width(px(56.))
                .sortable(),
            5 => Column::new("total", "Total self")
                .width(px(110.))
                .min_width(px(72.))
                .sortable(),
            6 => Column::new("mean", "Mean self")
                .width(px(100.))
                .min_width(px(72.))
                .sortable(),
            _ => Column::new("max", "Max self")
                .width(px(100.))
                .min_width(px(72.))
                .sortable(),
        }
    }

    /// Vendor `SortOrder::toggle` via the table's column-sort cycle.
    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let key = match col_ix {
            0 => puffin_flame::SortKey::Location,
            1 => puffin_flame::SortKey::FunctionName,
            2 => puffin_flame::SortKey::ScopeName,
            3 => puffin_flame::SortKey::Count,
            4 => puffin_flame::SortKey::Size,
            5 => puffin_flame::SortKey::TotalSelfTime,
            6 => puffin_flame::SortKey::MeanSelfTime,
            _ => puffin_flame::SortKey::MaxSelfTime,
        };
        let rev = match sort {
            ColumnSort::Ascending => false,
            // Default (first click on a new column) and Descending → desc,
            // matching the vendor's `rev: true` on a fresh sort key.
            _ => true,
        };
        if self.sort.key != key {
            self.sort = puffin_flame::SortOrder { key, rev };
        } else {
            self.sort.rev = rev;
        }
        let mut rows = self.rows.as_ref().clone();
        puffin_flame::sort_rows(&mut rows, self.sort);
        self.rows = Arc::new(rows);
        cx.notify();
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let Some(row) = self.row(row_ix) else {
            return div().into_any_element();
        };
        let cell = |text: String, color: Hsla| {
            div()
                .font_family(mono.clone())
                .text_size(px(12.))
                .line_height(relative(1.2))
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let stats = &row.stats;
        match col_ix {
            0 => cell(row.location.clone(), theme::TEXT_DIM),
            1 => cell(row.function_name.clone(), theme::TEXT),
            2 => cell(row.name.clone(), theme::TEXT_DIM),
            3 => {
                // vendor count color thresholds
                let color = if stats.count < 1_000 {
                    theme::TEXT
                } else if stats.count < 10_000 {
                    theme::YELLOW
                } else {
                    theme::RED
                };
                cell(format!("{:>5}", stats.count), color)
            }
            4 => cell(
                format!("{:>6.1} kB", stats.bytes as f32 * 1e-3),
                theme::TEXT,
            ),
            5 => cell(
                format!("{:>8.1} µs", stats.total_self_ns as f32 * 1e-3),
                theme::TEXT,
            ),
            6 => cell(
                format!(
                    "{:>8.1} µs",
                    stats.total_self_ns as f32 * 1e-3 / stats.count as f32
                ),
                theme::TEXT,
            ),
            _ => cell(
                format!("{:>8.1} µs", stats.max_ns as f32 * 1e-3),
                theme::TEXT,
            ),
        }
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let row = self.row(row_ix)?;
        let text = match col_ix {
            0 => &row.location,
            1 => &row.function_name,
            2 => &row.name,
            3 => return Some(format!("{:>5}", row.stats.count).into()),
            4 => return Some(format!("{:>6.1} kB", row.stats.bytes as f32 * 1e-3).into()),
            5 => return Some(format!("{:>8.1} µs", row.stats.total_self_ns as f32 * 1e-3).into()),
            6 => {
                return Some(
                    format!(
                        "{:>8.1} µs",
                        row.stats.total_self_ns as f32 * 1e-3 / row.stats.count as f32
                    )
                    .into(),
                );
            }
            _ => return Some(format!("{:>8.1} µs", row.stats.max_ns as f32 * 1e-3).into()),
        };
        (!text.is_empty()).then(|| text.clone().into())
    }
}

#[cfg(test)]
mod tests {
    use super::{AutoConnectPolicy, RETRY_MAX};
    use deerpunk_studio::runner::RunState;
    use std::time::{Duration, Instant};

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn running_entry_attempts_once() {
        let mut p = AutoConnectPolicy::new();
        let t = t0();
        // Engine not running yet: no attempt.
        assert!(!p.evaluate(&RunState::Idle, false, false, t));
        // Engine run starts → immediate attempt.
        assert!(p.evaluate(&RunState::Running, false, false, t));
        // Same state again: no second attempt (client self-retries).
        assert!(!p.evaluate(&RunState::Running, false, false, t + Duration::from_millis(50)));
        // Next run re-arms.
        assert!(!p.evaluate(&RunState::Idle, false, false, t + Duration::from_secs(1)));
        assert!(p.evaluate(&RunState::Running, false, false, t + Duration::from_secs(2)));
    }

    #[test]
    fn project_open_triggers_then_backs_off() {
        let mut p = AutoConnectPolicy::new();
        let t = t0();
        // Project opened (engine started outside the studio) → attempt.
        assert!(p.evaluate(&RunState::Idle, true, false, t));
        // Backoff: first retry waits retry_delay(1)=2s, then retry_delay(2)=4s.
        assert!(!p.evaluate(&RunState::Idle, true, false, t + Duration::from_millis(1999)));
        assert!(p.evaluate(&RunState::Idle, true, false, t + Duration::from_secs(2)));
        assert!(!p.evaluate(&RunState::Idle, true, false, t + Duration::from_secs(3)));
        assert!(!p.evaluate(&RunState::Idle, true, false, t + Duration::from_secs(5)));
        assert!(p.evaluate(&RunState::Idle, true, false, t + Duration::from_secs(6)));
        // Delays double, capped at 30s.
        assert_eq!(AutoConnectPolicy::retry_delay(0), Duration::from_secs(1));
        assert_eq!(AutoConnectPolicy::retry_delay(4), Duration::from_secs(16));
        assert_eq!(AutoConnectPolicy::retry_delay(9), RETRY_MAX);
    }

    #[test]
    fn no_project_no_attempt() {
        let mut p = AutoConnectPolicy::new();
        let t = t0();
        assert!(!p.evaluate(&RunState::Idle, false, false, t));
        assert!(!p.evaluate(&RunState::Building, false, false, t + Duration::from_secs(60)));
    }

    #[test]
    fn existing_client_suppresses_attempts() {
        let mut p = AutoConnectPolicy::new();
        let t = t0();
        assert!(!p.evaluate(&RunState::Running, true, true, t));
        assert!(!p.evaluate(&RunState::Idle, true, true, t + Duration::from_secs(10)));
    }

    #[test]
    fn manual_disconnect_suppressed_until_ream() {
        let mut p = AutoConnectPolicy::new();
        let t = t0();
        assert!(p.evaluate(&RunState::Idle, true, false, t));
        p.manual_disconnect = true; // as `disconnect()` sets it
        // Suppressed while the project stays open.
        assert!(!p.evaluate(&RunState::Idle, true, false, t + Duration::from_secs(60)));
        // Re-armed by the next Running entry.
        assert!(p.evaluate(&RunState::Running, true, false, t + Duration::from_secs(61)));
        // …and by a project re-open.
        p.manual_disconnect = true;
        assert!(!p.evaluate(&RunState::Idle, true, false, t + Duration::from_secs(90)));
        assert!(!p.evaluate(&RunState::Idle, false, false, t + Duration::from_secs(91)));
        assert!(p.evaluate(&RunState::Idle, true, false, t + Duration::from_secs(92)));
    }

    #[test]
    fn connect_clears_manual_disconnect_semantics() {
        // Mirrors connect()/disconnect(): policy re-armed on evaluate paths.
        let mut p = AutoConnectPolicy::new();
        let t = t0();
        assert!(p.evaluate(&RunState::Idle, true, false, t));
        assert_eq!(p.attempts, 1);
        // Run entry resets the backoff counter.
        assert!(p.evaluate(&RunState::Running, true, false, t + Duration::from_secs(60)));
        assert_eq!(p.attempts, 1);
    }
}
