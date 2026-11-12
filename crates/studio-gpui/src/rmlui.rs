//! Rmlui panel (center tab): live RmlUi preview. Port of the egui viewer
//! (src/viewers/rmlui.rs): the engine's `deerpunk-ui` runtime loads the
//! open document with the real asset resolver (RCSS, fonts, `ui://` refs —
//! exactly like in-game) and its draw lists are rasterized into a gpui
//! [`RenderImage`] — on the GPU through the engine's own wgpu `UiPass`
//! ([`gpu_raster`], offscreen, the game's `shaders/ui.wgsl`), with the
//! in-app software rasterizer ([`raster`]) as fallback. The panel owns the
//! session (RmlUi is process-global; this panel is the only in-process
//! owner — the egui version used a static for the same reason).
//!
//! Layout: the preview zone is a 2D canvas — grid background anchored at
//! the image origin, wheel/pinch zoom to the cursor (0.1–8×), left-drag /
//! shift+wheel pan, double-click fit — with the frame painted via
//! [`Window::paint_image`]; the pointer still passes plain clicks and hovers
//! into the RmlUi document (positions mapped through zoom/pan). To the
//! right, a fixed-width DataModel panel ([`datamodel`]) lists the open
//! document's binding map — inputs (`data-event-*`/`data-for`) and outputs
//! (`{{field}}`); live field values are unavailable (RmlUi does not
//! enumerate the model outward), so this is a structural map.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use deerpunk_ui::{UiDirAssets, UiDocument, UiRuntime};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::*;

use crate::rml;
use crate::state::StudioState;
use crate::theme;

#[path = "rml_datamodel.rs"]
mod datamodel;

use datamodel::DataModel;

#[path = "raster.rs"]
mod raster;

#[path = "gpu_raster.rs"]
mod gpu_raster;

use gpu_raster::GpuRaster;
use raster::RasterCache;

/// Smallest usable preview surface; smaller panes downscale from this.
const MIN_SCREEN: (f32, f32) = (320.0, 240.0);

/// DataModel panel width, right of the canvas.
const PANEL_W: f32 = 280.0;
/// Fit margin around the preview image, so the grid stays visible at fit.
const FIT_MARGIN: f32 = 16.0;
/// Grid spacing in document pixels: minor lines every 8, majors every 64.
const GRID_MINOR: f32 = 8.0;
const GRID_MAJOR: f32 = 64.0;
/// Screen spacing below which a grid tier is dropped (zoom-adaptive density).
const GRID_MIN_SPACING: f32 = 3.0;
/// Zoom clamp (multiplier over the fit-to-canvas scale).
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.0;
/// Drag distance after which a left press turns into a pan (below it, the
/// click still reaches the RmlUi document).
const DRAG_THRESHOLD: f32 = 4.0;

const LOAD_FAILED: &str = "document load failed — engine log has details";

/// Preview repaint cadence while a session is live: RmlUi animations and
/// timers only tick when `frame()` is called (the egui viewer requested a
/// repaint every frame for the same reason).
const TICK: Duration = Duration::from_millis(33);

/// One live preview session (or the sticky failure of one). `doc` is
/// declared before `runtime` so it drops first: `UiDocument::drop` closes
/// the document on the still-live context, `UiRuntime::drop` then tears the
/// context down.
enum Session {
    Ready {
        runtime: UiRuntime,
        doc: UiDocument,
        base: PathBuf,
        file: PathBuf,
        mtime: Option<SystemTime>,
        cache: RasterCache,
        /// GPU rasterizer over the engine's `UiPass`; `None` (no adapter,
        /// missing shader, or a runtime error) sticks to the CPU
        /// [`RasterCache`] until the session is rebuilt.
        gpu: Option<GpuRaster>,
        screen: (u32, u32),
        image: Option<Arc<RenderImage>>,
        image_size: [u32; 2],
    },
    Failed {
        base: PathBuf,
        file: PathBuf,
        reason: String,
    },
}

pub struct RmluiPanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    session: Option<Session>,
    /// File requested via [`RmluiPanel::open`], applied on the next render.
    pending_open: Option<(PathBuf, String)>,
    /// Currently previewed file and its ui-root-relative uri (`None` until
    /// the first `open`).
    open: Option<(PathBuf, String)>,
    /// Bounds of the preview region (the pane minus the status bar) from the
    /// last paint; decides the RmlUi render target. Captured by a prepaint
    /// listener; `Rc` because that listener runs inside this entity's own
    /// paint and cannot re-update the entity.
    pane_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Bounds of the canvas area (the row left of the DataModel panel) from
    /// the last paint, captured by the canvas paint callback. Decides the
    /// RmlUi render target once known; `Rc` for the same reason as
    /// [`RmluiPanel::pane_bounds`].
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// True while a repaint-timer chain keeps the preview animating.
    ticking: bool,
    /// Left button currently forwarded to the runtime (edge state: press /
    /// release are each sent exactly once, like the egui viewer's
    /// memory-based edge detect).
    mouse_down: bool,
    /// Displayed image scale from the last render: `fit * zoom`
    /// (0 = nothing shown); maps pointer positions into document pixels.
    view_scale: f32,
    /// Zoom multiplier over the fit-to-canvas scale (1 = fit).
    zoom: f32,
    /// Pan offset of the image center from the canvas center, screen px.
    pan: Point<Pixels>,
    /// Base fit-to-canvas scale from the last render (0 = canvas unknown).
    fit: f32,
    /// RmlUi document size (the rasterized screen) of the shown image.
    doc_size: [u32; 2],
    /// Last mouse position inside the canvas, for drag deltas.
    last_mouse: Option<Point<Pixels>>,
    /// Accumulated drag distance since the last press.
    drag_accum: f32,
    /// True once an ongoing left press became a pan (past the threshold).
    panning: bool,
    /// Binding map of the open document; rebuilt on open / hot-reload only
    /// (keyed on file + mtime, never per tick). `Err` = read/parse failure,
    /// reported as the panel placeholder.
    datamodel: Option<Result<DataModel, String>>,
    /// The (file, mtime) the current [`RmluiPanel::datamodel`] was built from.
    datamodel_src: Option<(PathBuf, Option<SystemTime>)>,
}

impl RmluiPanel {
    pub fn new(state: Entity<StudioState>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            state,
            session: None,
            pending_open: None,
            open: None,
            pane_bounds: Rc::new(Cell::new(None)),
            canvas_bounds: Rc::new(Cell::new(None)),
            ticking: false,
            mouse_down: false,
            view_scale: 0.0,
            zoom: 1.0,
            pan: Point::default(),
            fit: 0.0,
            doc_size: [0, 0],
            last_mouse: None,
            drag_accum: 0.0,
            panning: false,
            datamodel: None,
            datamodel_src: None,
        }
    }

    /// Preview a `.rml` document: `path` is the absolute file, `rel` its uri
    /// form under the UI asset root (e.g. `widgets/loading.rml` with base
    /// `<…>/ui`). Re-opening the same file retries a failed load; a changed
    /// file (or base) rebuilds the session; a changed mtime hot-reloads it.
    pub fn open(&mut self, path: PathBuf, rel: String, cx: &mut Context<Self>) {
        self.pending_open = Some((path, rel));
        cx.notify();
    }

    /// Left button release forwarded exactly once per press. Also ends an
    /// ongoing pan.
    fn release_mouse(&mut self) {
        self.panning = false;
        self.drag_accum = 0.0;
        if !self.mouse_down {
            return;
        }
        self.mouse_down = false;
        if let Some(Session::Ready { runtime, .. }) = self.session.as_ref() {
            runtime.mouse_button(0, false);
        }
        // Console commands queued by RmlUi Lua are meaningless in a studio
        // preview — drop them (egui drains after forwarding too).
        let _ = deerpunk_ui::drain_commands();
    }

    /// Displayed image rect on the current canvas (`None` before the first
    /// paint or when nothing is shown).
    fn image_rect(&self) -> Option<Bounds<Pixels>> {
        let canvas = self.canvas_bounds.get()?;
        Some(image_rect(canvas, self.doc_size, self.view_scale, self.pan))
    }

    /// Pointer position in RmlUi document pixels — the pointer offset inside
    /// the displayed image rect divided by the displayed scale (`fit * zoom`,
    /// so zoom and pan are part of the mapping); `None` when the pointer is
    /// outside the displayed image (egui gated on `resp.hovered()`).
    fn doc_pos(&self, pos: Point<Pixels>) -> Option<Point<i32>> {
        if self.view_scale <= 0.0 {
            return None;
        }
        let bounds = self.image_rect()?;
        let dx = f32::from(pos.x) - f32::from(bounds.origin.x);
        let dy = f32::from(pos.y) - f32::from(bounds.origin.y);
        let inside = dx >= 0.0
            && dy >= 0.0
            && dx <= f32::from(bounds.size.width)
            && dy <= f32::from(bounds.size.height);
        inside.then(|| point((dx / self.view_scale) as i32, (dy / self.view_scale) as i32))
    }

    /// Zoom by `factor` anchored at `cursor`: the document pixel under the
    /// cursor stays put, the rest of the image scales around it. Clamped to
    /// `MIN_ZOOM..MAX_ZOOM`; no-op before the first canvas paint.
    pub fn zoom_at_cursor(&mut self, cursor: Point<Pixels>, factor: f32, cx: &mut Context<Self>) {
        let Some(canvas) = self.canvas_bounds.get() else {
            return;
        };
        let (zoom, pan) = zoom_at(canvas, self.doc_size, self.fit, self.zoom, self.pan, cursor, factor);
        self.zoom = zoom;
        self.pan = pan;
        cx.notify();
    }

    /// Double-click fit-to-canvas: zoom back to 1 (the fit scale), pan to
    /// center.
    pub fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.zoom = 1.0;
        self.pan = Point::default();
        cx.notify();
    }

    /// Bring the session in line with `file`: create the runtime when the
    /// base changed, hot-reload the document on mtime/file change, resize on
    /// pane change — then fold this pass's frame deltas and refresh the
    /// raster when anything changed. Errors report as `Err(reason)` and
    /// stick until the file or base changes (or `retry` re-opens it).
    fn sync_session(
        &mut self,
        file: &Path,
        rel: &str,
        retry: bool,
        target: (u32, u32),
    ) -> Result<(), String> {
        let base = base_of(file, rel);
        let mtime = std::fs::metadata(file).and_then(|m| m.modified()).ok();

        // DataModel map: refresh on open / hot-reload only — keyed on
        // (file, mtime), so steady-state ticks never re-parse. Works for
        // failed sessions too (the map needs only the source text).
        let key = (file.to_path_buf(), mtime);
        if self.datamodel_src.as_ref() != Some(&key) {
            self.datamodel = load_datamodel(file);
            self.datamodel_src = Some(key);
        }

        // Failed sessions stick until the file or base directory changes.
        if let Some(Session::Failed {
            base: fb,
            file: ff,
            reason,
        }) = self.session.as_ref()
        {
            if !retry && *fb == base && *ff == file {
                return Err(reason.clone());
            }
        }

        // Same base: hot-reload the document in place (different file or
        // changed mtime). Runtime, mesh/texture ids and the raster cache
        // stay valid — the following frames carry the release/update deltas.
        let mut rebuild = true;
        let mut load_failed = false;
        if let Some(Session::Ready {
            base: sb,
            file: sf,
            mtime: sm,
            runtime,
            doc,
            ..
        }) = self.session.as_mut()
        {
            if *sb == base {
                rebuild = false;
                if *sf != file || *sm != mtime {
                    match runtime.load_document(rel) {
                        Some(new_doc) => {
                            new_doc.show();
                            *doc = new_doc;
                            *sf = file.to_path_buf();
                            *sm = mtime;
                        }
                        None => load_failed = true,
                    }
                }
            }
        }
        if load_failed {
            return Err(self.mark_failed(base, file, LOAD_FAILED.into()));
        }

        if rebuild {
            // Drop the old session first: `UiDocument::drop` closes the
            // document server-side, `UiRuntime::drop` tears down RmlUi.
            self.session = None;
            let runtime = UiRuntime::new(
                Box::new(UiDirAssets::new(base.clone())),
                target,
                "studio_preview",
            );
            if !runtime.is_ready() {
                let reason = format!(
                    "no usable fonts under {} — need fonts/*.ttf",
                    base.display()
                );
                return Err(self.mark_failed(base, file, reason));
            }
            let Some(doc) = runtime.load_document(rel) else {
                return Err(self.mark_failed(base, file, LOAD_FAILED.into()));
            };
            doc.show();
            // Shader root is the gamedir — the parent of the `ui` asset
            // root (`<gamedir>/shaders/ui.wgsl`, the engine's SHADER_DIR).
            let gpu = GpuRaster::new(base.parent().unwrap_or(base.as_path()));
            self.session = Some(Session::Ready {
                runtime,
                doc,
                base,
                file: file.to_path_buf(),
                mtime,
                cache: RasterCache::default(),
                gpu,
                screen: target,
                image: None,
                image_size: [0, 0],
            });
        }

        // Resize when the pane changed, then take this pass's frame deltas.
        let fresh = match self
            .session
            .as_mut()
            .expect("session is Ready after the rebuild block")
        {
            Session::Ready {
                runtime, screen, ..
            } => {
                if *screen != target {
                    runtime.resize(target.0, target.1);
                    *screen = target;
                }
                runtime.frame()
            }
            Session::Failed { .. } => unreachable!("session is Ready after the rebuild block"),
        };

        // Re-raster only when a new frame landed or the output size changed.
        let session = self
            .session
            .as_mut()
            .expect("session is Ready after the rebuild block");
        let Session::Ready {
            cache,
            gpu,
            image,
            image_size,
            ..
        } = session
        else {
            unreachable!("session is Ready after the rebuild block");
        };
        if let Some(frame) = &fresh {
            cache.apply(frame);
        }
        let shown = if cache.screen() == [0, 0] {
            [target.0, target.1]
        } else {
            cache.screen()
        };
        if fresh.is_some() || *image_size != shown {
            // GPU first; the first runtime error pins the CPU fallback for
            // the session — no per-frame adapter retries.
            let gpu_buf = match gpu {
                Some(g) => match g.raster(fresh.as_ref(), shown) {
                    Some(buf) => Some(buf),
                    None => {
                        *gpu = None;
                        eprintln!(
                            "rmlui preview: gpu raster failed — CPU fallback for this session"
                        );
                        None
                    }
                },
                None => None,
            };
            let buf = gpu_buf.unwrap_or_else(|| cache.raster(shown[0], shown[1]));
            let buf = image::RgbaImage::from_raw(shown[0], shown[1], buf)
                .expect("raster buffer dimensions match");
            *image = Some(Arc::new(RenderImage::new([image::Frame::new(buf)])));
            *image_size = shown;
        }
        // Console commands queued by RmlUi Lua during the update are
        // meaningless in a studio preview — drop them once per pass, like
        // the egui viewer.
        let _ = deerpunk_ui::drain_commands();
        Ok(())
    }

    /// Replace the session with a `Failed` marker and report the reason.
    fn mark_failed(&mut self, base: PathBuf, file: &Path, reason: String) -> String {
        self.session = Some(Session::Failed {
            base,
            file: file.to_path_buf(),
            reason: reason.clone(),
        });
        reason
    }

    /// The interactive canvas: the grid background with the rasterized
    /// frame painted at `fit * zoom` (offset by `pan`), wheel/pinch zoom
    /// anchored at the cursor, left-drag pan, double-click fit — and pointer
    /// pass-through into the RmlUi document for plain hovers/clicks. Bounds
    /// are captured by the paint callback for the next frame's hit-testing
    /// (`Rc`/`Cell`: paint runs inside this entity's own paint phase).
    fn canvas_element(
        &mut self,
        image: Arc<RenderImage>,
        shown: [u32; 2],
        fit: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let k = fit * self.zoom;
        self.fit = fit;
        self.view_scale = k;
        self.doc_size = shown;
        let canvas_bounds = self.canvas_bounds.clone();
        let pan = self.pan;
        div()
            .id("rmlui-canvas")
            .flex_1()
            .min_w_0()
            .relative()
            .overflow_hidden()
            // Left-drag pans once the press passes the drag threshold; plain
            // moves and small jitter pass through to the document (hover and
            // click-drag-within-threshold). While panning, moves stop being
            // forwarded — the press release still is (below), so the
            // document never keeps a stuck button.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let Some(last) = this.last_mouse {
                    let dx = ev.position.x - last.x;
                    let dy = ev.position.y - last.y;
                    if ev.pressed_button == Some(MouseButton::Left) {
                        this.drag_accum += dx.as_f32().abs() + dy.as_f32().abs();
                        if this.panning || this.drag_accum > DRAG_THRESHOLD {
                            this.panning = true;
                            this.pan.x += dx;
                            this.pan.y += dy;
                            this.last_mouse = Some(ev.position);
                            cx.notify();
                            return;
                        }
                    }
                }
                this.last_mouse = Some(ev.position);
                let Some(doc) = this.doc_pos(ev.position) else {
                    return;
                };
                if let Some(Session::Ready { runtime, .. }) = this.session.as_ref() {
                    runtime.mouse_move(doc.x, doc.y);
                }
                let _ = deerpunk_ui::drain_commands();
            }))
            // Presses and releases are edge-detected via `mouse_down` so
            // each is sent exactly once (egui used egui-memory for the same
            // effect).
            .on_mouse_down(MouseButton::Left, {
                cx.listener(move |this, ev: &MouseDownEvent, _, _| {
                    this.drag_accum = 0.0;
                    let Some(doc) = this.doc_pos(ev.position) else {
                        return;
                    };
                    if let Some(Session::Ready { runtime, .. }) = this.session.as_ref() {
                        runtime.mouse_move(doc.x, doc.y);
                        runtime.mouse_button(0, true);
                    }
                    this.mouse_down = true;
                    let _ = deerpunk_ui::drain_commands();
                })
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.release_mouse();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.release_mouse()),
            )
            // Double-click (not a drag) fits the view. A quick double click
            // on a widget control therefore also fits — documented tradeoff
            // for having both click pass-through and a fit gesture.
            .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                let double = matches!(ev, ClickEvent::Mouse(e) if e.up.click_count >= 2);
                if double && this.drag_accum <= DRAG_THRESHOLD {
                    this.reset_view(cx);
                }
            }))
            // The canvas owns the wheel/pinch — stop propagation so outer
            // scrollers never also scroll (same as the puffin canvas).
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
                let Some(canvas) = this.canvas_bounds.get() else {
                    return;
                };
                if !canvas.contains(&ev.position) {
                    return;
                }
                let (dx, dy) = match ev.delta {
                    ScrollDelta::Pixels(d) => (d.x.as_f32(), d.y.as_f32()),
                    ScrollDelta::Lines(d) => (d.x * 16., d.y * 16.),
                };
                if ev.modifiers.shift {
                    // Shift+wheel pans (both axes), like the puffin canvas.
                    this.pan.x += px(dx);
                    this.pan.y += px(dy);
                    cx.notify();
                } else {
                    if dy != 0.0 {
                        this.zoom_at_cursor(ev.position, (-dy * 0.01).exp().clamp(0.5, 2.0), cx);
                    }
                    if dx != 0.0 {
                        this.pan.x += px(dx);
                        cx.notify();
                    }
                }
                cx.stop_propagation();
            }))
            .on_pinch(cx.listener(|this, ev: &PinchEvent, _, cx| {
                let Some(canvas) = this.canvas_bounds.get() else {
                    return;
                };
                if !canvas.contains(&ev.position) {
                    return;
                }
                this.zoom_at_cursor(ev.position, (1.0 + ev.delta).clamp(0.5, 2.0), cx);
            }))
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds: Bounds<Pixels>, _, window, _| {
                        // Remember where the canvas lives for the next
                        // frame's hit-testing; no notify (paint-phase
                        // mutation only).
                        canvas_bounds.set(Some(bounds));
                        paint_canvas(bounds, shown, k, pan, &image, window);
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }

    /// The right-hand DataModel panel: the open document's static binding
    /// map — Inputs (`data-event-*` handlers and `data-for` iterations),
    /// Outputs (`{{field}}` mustaches) and the merged field rollup.
    fn datamodel_panel(&self) -> AnyElement {
        let panel = div()
            .id("rmlui-datamodel")
            .w(px(PANEL_W))
            .flex_none()
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_y_scroll()
            .bg(theme::PANEL)
            .border_l_1()
            .border_color(theme::BORDER)
            .child(section_header("DataModel".into()));
        let Some(Ok(model)) = self.datamodel.as_ref() else {
            let why = match &self.datamodel {
                Some(Err(reason)) => reason.clone(),
                _ => "Open a .rml file to inspect its data model".into(),
            };
            let dim = self.datamodel.is_none();
            return panel
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_3()
                        .child(
                            div()
                                .text_sm()
                                .text_color(if dim { theme::TEXT_DIM } else { theme::YELLOW })
                                .child(why),
                        ),
                )
                .into_any_element();
        };
        if model.is_empty() {
            return panel.child(placeholder("no bindings in document")).into_any_element();
        }
        panel
            .child(binding_section("Inputs", &model.inputs))
            .child(binding_section("Outputs", &model.outputs))
            .child(fields_section(model))
            .into_any_element()
    }
}

/// One DataModel section: a header with the row count and one row per
/// binding (`field`, then `context · element` underneath).
fn binding_section(title: &str, rows: &[datamodel::Binding]) -> Div {
    let mut body = div().flex().flex_col();
    for b in rows {
        body = body.child(
            div()
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme::BORDER)
                .child(div().text_sm().text_color(theme::TEXT).child(b.field.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::TEXT_DIM)
                        .child(format!("{} · {}", b.detail, b.element)),
                ),
        );
    }
    div()
        .flex()
        .flex_col()
        .child(section_header(format!("{title} ({})", rows.len())))
        .child(body)
}

/// The Fields section: unique fields with their usage marks (`in` / `out`).
fn fields_section(model: &DataModel) -> Div {
    let mut body = div().flex().flex_col();
    for f in &model.fields {
        let mut uses = String::new();
        if f.input {
            uses.push_str("in");
        }
        if f.output {
            if !uses.is_empty() {
                uses.push_str(" + ");
            }
            uses.push_str("out");
        }
        body = body.child(
            div()
                .flex()
                .justify_between()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme::BORDER)
                .child(div().text_sm().text_color(theme::TEXT).child(f.field.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::TEXT_DIM)
                        .child(uses.clone()),
                ),
        );
    }
    div()
        .flex()
        .flex_col()
        .child(section_header(format!("Fields ({})", model.fields.len())))
        .child(body)
}

fn section_header(text: String) -> Div {
    div()
        .px_2()
        .py_1()
        .bg(theme::HEADER)
        .child(div().text_xs().text_color(theme::TEXT_DIM).child(text))
}

fn placeholder(text: &str) -> Div {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .p_3()
        .child(div().text_sm().text_color(theme::TEXT_DIM).child(text.to_string()))
}

/// Parse `file` and extract its binding map; a read or parse failure comes
/// back as the panel's placeholder reason.
fn load_datamodel(file: &Path) -> Option<Result<DataModel, String>> {
    let Ok(src) = std::fs::read_to_string(file) else {
        return Some(Err("file unreadable".into()));
    };
    match rml::parse(&src) {
        Ok(doc) => Some(Ok(datamodel::extract(&doc))),
        Err(e) => Some(Err(format!("parse failed: {e}"))),
    }
}

/// Displayed image rect: `shown` document pixels scaled by `k`, centered on
/// the canvas plus the `pan` offset (offset of image center from canvas
/// center).
fn image_rect(canvas: Bounds<Pixels>, shown: [u32; 2], k: f32, pan: Point<Pixels>) -> Bounds<Pixels> {
    let img = size(px(shown[0] as f32 * k), px(shown[1] as f32 * k));
    let img_w = f32::from(img.width);
    let img_h = f32::from(img.height);
    let center = point(
        canvas.origin.x + px(f32::from(canvas.size.width) / 2.0 + f32::from(pan.x)),
        canvas.origin.y + px(f32::from(canvas.size.height) / 2.0 + f32::from(pan.y)),
    );
    Bounds {
        origin: point(center.x - px(img_w / 2.0), center.y - px(img_h / 2.0)),
        size: img,
    }
}

/// New `(zoom, pan)` after zooming by `factor` anchored at `cursor`: the
/// document pixel under the cursor keeps its screen position. `fit <= 0`
/// (no canvas yet) or a clamped no-op leaves the view unchanged.
fn zoom_at(
    canvas: Bounds<Pixels>,
    shown: [u32; 2],
    fit: f32,
    zoom: f32,
    pan: Point<Pixels>,
    cursor: Point<Pixels>,
    factor: f32,
) -> (f32, Point<Pixels>) {
    if fit <= 0.0 || shown[0] == 0 || shown[1] == 0 {
        return (zoom, pan);
    }
    let k = fit * zoom;
    let k2 = (fit * zoom * factor).clamp(fit * MIN_ZOOM, fit * MAX_ZOOM);
    if k <= 0.0 || (k2 - k).abs() < 1e-6 {
        return (zoom, pan);
    }
    let img = image_rect(canvas, shown, k, pan);
    // Anchor: the cursor's offset inside the image, rescaled to the new
    // size, re-anchored under the cursor.
    let ax = f32::from(cursor.x) - f32::from(img.origin.x);
    let ay = f32::from(cursor.y) - f32::from(img.origin.y);
    let r = k2 / k;
    let origin_x = f32::from(cursor.x) - ax * r;
    let origin_y = f32::from(cursor.y) - ay * r;
    let canvas_cx = f32::from(canvas.origin.x) + f32::from(canvas.size.width) / 2.0;
    let canvas_cy = f32::from(canvas.origin.y) + f32::from(canvas.size.height) / 2.0;
    (
        k2 / fit,
        point(
            px(origin_x + shown[0] as f32 * k2 / 2.0 - canvas_cx),
            px(origin_y + shown[1] as f32 * k2 / 2.0 - canvas_cy),
        ),
    )
}

/// Grid lines for one axis: positions of minor (every [`GRID_MINOR`] doc px)
/// and major ([`GRID_MAJOR`]) lines, anchored at `origin0` (the image
/// origin on that axis, so the grid stays glued to the document under
/// zoom/pan) and scaled by `k`. Tiers denser than [`GRID_MIN_SPACING`] on
/// screen are dropped; each tier's positions are strictly increasing and
/// distinct.
fn grid_lines(lo: f32, hi: f32, origin0: f32, k: f32) -> Vec<(f32, bool)> {
    let mut out = Vec::new();
    if k <= 0.0 || hi <= lo {
        return out;
    }
    for (doc_step, is_major) in [(GRID_MINOR, false), (GRID_MAJOR, true)] {
        let spacing = doc_step * k;
        if spacing < GRID_MIN_SPACING {
            continue;
        }
        let first = ((lo - origin0) / spacing).ceil();
        let last = ((hi - origin0) / spacing).floor();
        let mut n = first;
        while n <= last {
            let x = origin0 + n * spacing;
            if x >= lo && x <= hi {
                out.push((x, is_major));
            }
            n += 1.0;
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// Paint the canvas: background, grid (minors then majors so majors
/// overdraw), then the preview image clipped to the canvas.
fn paint_canvas(
    canvas: Bounds<Pixels>,
    shown: [u32; 2],
    k: f32,
    pan: Point<Pixels>,
    image: &Arc<RenderImage>,
    window: &mut Window,
) {
    window.paint_quad(fill(canvas, theme::BG));
    if k <= 0.0 || shown[0] == 0 || shown[1] == 0 {
        return;
    }
    let img = image_rect(canvas, shown, k, pan);
    let (x0, y0) = (f32::from(canvas.origin.x), f32::from(canvas.origin.y));
    let (w, h) = (f32::from(canvas.size.width), f32::from(canvas.size.height));
    let verticals = grid_lines(x0, x0 + w, f32::from(img.origin.x), k);
    let horizontals = grid_lines(y0, y0 + h, f32::from(img.origin.y), k);
    for is_major in [false, true] {
        for (vertical, lines) in [(true, &verticals), (false, &horizontals)] {
            for (pos, major) in lines {
                if *major != is_major {
                    continue;
                }
                let quad = if vertical {
                    Bounds {
                        origin: point(px(*pos), canvas.origin.y),
                        size: size(px(1.), canvas.size.height),
                    }
                } else {
                    Bounds {
                        origin: point(canvas.origin.x, px(*pos)),
                        size: size(canvas.size.width, px(1.)),
                    }
                };
                window.paint_quad(fill(quad, if is_major { theme::HOVER } else { theme::FAINT }));
            }
        }
    }
    let _ = window.paint_image(canvas, img, Corners::default(), image.clone(), 0, false);
}

impl EventEmitter<PanelEvent> for RmluiPanel {}

impl Focusable for RmluiPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for RmluiPanel {
    fn panel_name(&self) -> &'static str {
        "RmluiPanel"
    }
}

impl Panel for RmluiPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Rmlui"
    }
}

impl Render for RmluiPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Apply a pending open() so this pass already previews it; a fresh
        // open also retries a previously failed session.
        let retry = self.pending_open.is_some();
        if let Some((path, rel)) = self.pending_open.take() {
            self.open = Some((path, rel));
        }

        // Canvas area: exact bounds from the last paint, else the pane
        // estimate (pane minus the DataModel panel). Drives the RmlUi render
        // target and the fit scale; the canvas paint callback corrects the
        // estimate on the next frame.
        let pane_size = self
            .pane_bounds
            .get()
            .map(|b| b.size)
            .unwrap_or_else(|| size(px(MIN_SCREEN.0), px(MIN_SCREEN.1)));
        let canvas_size = self
            .canvas_bounds
            .get()
            .map(|b| b.size)
            .unwrap_or_else(|| {
                size(
                    px((f32::from(pane_size.width) - PANEL_W).max(MIN_SCREEN.0)),
                    px(f32::from(pane_size.height).max(MIN_SCREEN.1)),
                )
            });
        let target = screen_for(canvas_size);

        let mut alive = false;
        let (status, canvas_area) = match self.open.clone() {
            None => (String::new(), empty_canvas().into_any_element()),
            Some((file, rel)) => match self.sync_session(&file, &rel, retry, target) {
                Err(reason) => {
                    self.view_scale = 0.0;
                    (
                        String::new(),
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_3()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::TEXT_DIM)
                                    .child("preview unavailable"),
                            )
                            .child(div().text_sm().text_color(theme::YELLOW).child(reason))
                            .into_any_element(),
                    )
                }
                Ok(()) => {
                    alive = true;
                    let Session::Ready {
                        cache, image, base, ..
                    } = self
                        .session
                        .as_ref()
                        .expect("session is Ready after sync_session Ok")
                    else {
                        unreachable!("session is Ready after sync_session Ok");
                    };
                    let shown = if cache.screen() == [0, 0] {
                        [target.0, target.1]
                    } else {
                        cache.screen()
                    };
                    // Fit inside the canvas with a margin so the grid stays
                    // visible at the default view.
                    let fit = fit_scale(
                        shown,
                        size(
                            px((f32::from(canvas_size.width) - 2.0 * FIT_MARGIN).max(1.0)),
                            px((f32::from(canvas_size.height) - 2.0 * FIT_MARGIN).max(1.0)),
                        ),
                    );
                    let status = format!(
                        "{} · {}×{} · {} draws",
                        base.display(),
                        target.0,
                        target.1,
                        cache.draw_count()
                    );
                    match (fit > 0.0, image.clone()) {
                        (true, Some(image)) => (status, self.canvas_element(image, shown, fit, cx)),
                        _ => {
                            self.view_scale = 0.0;
                            (
                                status,
                                div().flex_1().min_w_0().into_any_element(),
                            )
                        }
                    }
                }
            },
        };
        let hint = if alive {
            "wheel zoom · shift+wheel pan · drag pan · dblclick fit"
        } else {
            ""
        };

        // Keep re-rendering so RmlUi animations/timers keep ticking; the
        // chain dies on drop (weak update fails) or when nothing is live.
        if alive && !self.ticking {
            self.ticking = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    let Ok(()) = this.update(cx, |this, cx| {
                        this.ticking = false;
                        cx.notify();
                    }) else {
                        break;
                    };
                }
            })
            .detach();
        }

        let pane_bounds = self.pane_bounds.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::BG)
            // Direct children of this column: [#0 status bar, #1 body row].
            // The row's bounds are the actual preview region (pane minus the
            // status bar) and feed the RmlUi render target on the next frame
            // (the ticker picks the change up).
            .on_children_prepainted(move |bounds: Vec<Bounds<Pixels>>, _, _| {
                pane_bounds.set(bounds.get(1).copied());
            })
            // Status line: interaction hint left, session info right (weak,
            // like egui's).
            .child(
                div()
                    .flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(div().text_xs().text_color(theme::TEXT_DIM).child(hint))
                    .child(div().text_xs().text_color(theme::TEXT_DIM).child(status)),
            )
            // Body row: preview canvas (flex) + DataModel panel (fixed).
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(canvas_area)
                    .child(self.datamodel_panel()),
            )
    }
}

/// Canvas-area placeholder when no document is open.
fn empty_canvas() -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .text_sm()
                .text_color(theme::TEXT_DIM)
                .child("Open a .rml file to preview it"),
        )
}

/// UI asset root for `file`: with a known `rel` it is `file` minus the `rel`
/// suffix (the caller's authoritative split); otherwise the nearest ancestor
/// directory named `ui` — the engine's layout (`<gamedir>/ui/…`) — falling
/// back to the file's parent (same rule as the egui viewer).
fn base_of(file: &Path, rel: &str) -> PathBuf {
    let skip = rel.split(['/', '\\']).filter(|s| !s.is_empty()).count();
    if skip > 0 {
        let mut comps: Vec<_> = file.components().collect();
        if comps.len() > skip {
            comps.truncate(comps.len() - skip);
            return comps.iter().collect::<PathBuf>();
        }
    }
    file.ancestors()
        .skip(1) // a file *named* `ui` is not its own base directory
        .find(|p| p.file_name().is_some_and(|n| n == "ui"))
        .or_else(|| file.parent())
        .unwrap_or(Path::new(""))
        .to_path_buf()
}

/// Integer preview screen for `pane`: the pane size, but never smaller than
/// [`MIN_SCREEN`] (smaller panes just get a downscaled preview).
fn screen_for(pane: Size<Pixels>) -> (u32, u32) {
    (
        (f32::from(pane.width).round() as u32).max(MIN_SCREEN.0 as u32),
        (f32::from(pane.height).round() as u32).max(MIN_SCREEN.1 as u32),
    )
}

/// Largest scale ≤ 1.0 fitting `screen` into `avail` (0 when degenerate).
fn fit_scale(screen: [u32; 2], avail: Size<Pixels>) -> f32 {
    let (w, h) = (f32::from(avail.width), f32::from(avail.height));
    if screen[0] == 0 || screen[1] == 0 || w <= 0.0 || h <= 0.0 {
        return 0.0;
    }
    (w / screen[0] as f32).min(h / screen[1] as f32).min(1.0)
}

#[cfg(test)]
mod tests {
    // NOT `use super::*`: the `gpui_kit::*` glob in the parent re-exports a
    // `test` proc-macro that shadows the builtin `#[test]` attribute (same
    // as viewers.rs's tests).
    use super::{image_rect, load_datamodel, zoom_at, MAX_ZOOM, MIN_ZOOM};
    use gpui_kit::{point, px, size, Bounds, Pixels, Point};
    use std::path::PathBuf;

    fn canvas() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(100.), px(50.)),
            size: size(px(800.), px(600.)),
        }
    }

    fn fit() -> f32 {
        0.5
    }

    /// Doc size at `fit` exactly filling the canvas.
    fn doc() -> [u32; 2] {
        [1600, 1200]
    }

    #[test]
    fn image_rect_centers_and_zoom_grows_it() {
        let c = canvas();
        let fit = fit();
        let doc = doc();
        let zero = Point::default();

        // Zoom 1: centered on the canvas, exact fit size.
        let at_fit = image_rect(c, doc, fit, zero);
        assert_eq!(f32::from(at_fit.origin.x), 100.0);
        assert_eq!(f32::from(at_fit.origin.y), 50.0);
        assert_eq!(f32::from(at_fit.size.width), 800.0);
        assert_eq!(f32::from(at_fit.size.height), 600.0);

        // Zoom 2: twice the size, same center (100+400, 50+300).
        let zoomed = image_rect(c, doc, fit * 2.0, zero);
        assert_eq!(f32::from(zoomed.size.width), 1600.0);
        assert_eq!(f32::from(zoomed.size.height), 1200.0);
        let cx = f32::from(zoomed.origin.x) + f32::from(zoomed.size.width) / 2.0;
        let cy = f32::from(zoomed.origin.y) + f32::from(zoomed.size.height) / 2.0;
        assert!((cx - 500.0).abs() < 1e-4 && (cy - 350.0).abs() < 1e-4);

        // Pan moves the center one-to-one.
        let panned = image_rect(c, doc, fit, point(px(40.), px(-20.)));
        let cx = f32::from(panned.origin.x) + f32::from(panned.size.width) / 2.0;
        let cy = f32::from(panned.origin.y) + f32::from(panned.size.height) / 2.0;
        assert!((cx - 540.0).abs() < 1e-4 && (cy - 330.0).abs() < 1e-4);
    }

    #[test]
    fn zoom_at_keeps_cursor_anchor_still() {
        let c = canvas();
        let fit = fit();
        let doc = doc();
        let zero = Point::default();
        let cursor = point(px(300.), px(200.));

        // The doc pixel under the cursor before the zoom...
        let before = image_rect(c, doc, fit, zero);
        let doc_x = (f32::from(cursor.x) - f32::from(before.origin.x)) / fit;
        let doc_y = (f32::from(cursor.y) - f32::from(before.origin.y)) / fit;

        // ...must sit under the cursor after zooming 2x.
        let (zoom, pan) = zoom_at(c, doc, fit, 1.0, zero, cursor, 2.0);
        assert!((zoom - 2.0).abs() < 1e-4);
        let after = image_rect(c, doc, fit * zoom, pan);
        let scale = fit * zoom;
        let doc_x2 = (f32::from(cursor.x) - f32::from(after.origin.x)) / scale;
        let doc_y2 = (f32::from(cursor.y) - f32::from(after.origin.y)) / scale;
        assert!((doc_x2 - doc_x).abs() < 1e-3, "{doc_x2} vs {doc_x}");
        assert!((doc_y2 - doc_y).abs() < 1e-3, "{doc_y2} vs {doc_y}");
    }

    #[test]
    fn zoom_at_clamps_and_noops() {
        let c = canvas();
        let doc = doc();

        // Clamp on both ends, anchored anywhere.
        let (zoom_hi, _) = zoom_at(c, doc, fit(), 4.0, Point::default(), point(px(1.), px(1.)), 1e9);
        assert!((zoom_hi - MAX_ZOOM).abs() < 1e-4);
        let (zoom_lo, _) = zoom_at(c, doc, fit(), 1.0, Point::default(), point(px(1.), px(1.)), 1e-9);
        assert!((zoom_lo - MIN_ZOOM).abs() < 1e-4);

        // No canvas metrics yet → view unchanged; neutral factor → unchanged.
        let (z, p) = zoom_at(c, doc, 0.0, 2.0, point(px(5.), px(5.)), point(px(9.), px(9.)), 2.0);
        assert_eq!((z, f32::from(p.x), f32::from(p.y)), (2.0, 5.0, 5.0));
        let (z, p) = zoom_at(c, doc, fit(), 1.5, point(px(5.), px(5.)), point(px(9.), px(9.)), 1.0);
        assert_eq!((z, f32::from(p.x), f32::from(p.y)), (1.5, 5.0, 5.0));
    }

    #[test]
    fn datamodel_of_test_fixture_classifies() {
        // The headless-render fixture (tests/fixtures/ui) through the same
        // loader the panel uses.
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ui/widgets/binding_test.rml");
        let model = load_datamodel(&path)
            .expect("fixture readable")
            .expect("fixture parses");
        assert!(
            model.inputs.iter().any(|b| b.detail == "click" && b.field == "on_play"),
            "data-event-click classified as input: {:?}",
            model.inputs
        );
        assert!(
            model
                .inputs
                .iter()
                .any(|b| b.detail == "for" && b.field.contains("items")),
            "data-for classified as input: {:?}",
            model.inputs
        );
        assert!(
            model
                .outputs
                .iter()
                .any(|b| b.field == "flag" && b.detail == "data-class-active"),
            "{{field}} attr classified as output: {:?}",
            model.outputs
        );
        assert!(
            model.outputs.iter().any(|b| b.field == "count" && b.detail == "text"),
            "{{field}} text classified as output: {:?}",
            model.outputs
        );
    }
}
