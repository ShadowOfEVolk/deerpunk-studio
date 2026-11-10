//! Rmlui panel (center tab): live RmlUi preview. Port of the egui viewer
//! (src/viewers/rmlui.rs): the engine's `deerpunk-ui` runtime loads the
//! open document with the real asset resolver (RCSS, fonts, `ui://` refs —
//! exactly like in-game) and its draw lists are software-rasterized into a
//! gpui [`RenderImage`] via the [`raster`] module. The panel owns the
//! session (RmlUi is process-global; this panel is the only in-process
//! owner — the egui version used a static for the same reason).

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use deerpunk_ui::{UiDirAssets, UiDocument, UiRuntime};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::*;

use crate::state::StudioState;
use crate::theme;

#[path = "raster.rs"]
mod raster;

use raster::RasterCache;

/// Smallest usable preview surface; smaller panes downscale from this.
const MIN_SCREEN: (f32, f32) = (320.0, 240.0);

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
    /// Bounds of the displayed preview image from the last paint; maps
    /// pointer positions into document pixels.
    img_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// True while a repaint-timer chain keeps the preview animating.
    ticking: bool,
    /// Left button currently forwarded to the runtime (edge state: press /
    /// release are each sent exactly once, like the egui viewer's
    /// memory-based edge detect).
    mouse_down: bool,
    /// Scale of the preview image from the last render (0 = nothing shown).
    view_scale: f32,
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
            img_bounds: Rc::new(Cell::new(None)),
            ticking: false,
            mouse_down: false,
            view_scale: 0.0,
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

    /// Left button release forwarded exactly once per press.
    fn release_mouse(&mut self) {
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

    /// Pointer position in RmlUi document pixels; `None` when the pointer is
    /// outside the displayed image (egui gated on `resp.hovered()`).
    fn doc_pos(&self, pos: Point<Pixels>) -> Option<Point<i32>> {
        if self.view_scale <= 0.0 {
            return None;
        }
        let bounds = self.img_bounds.get()?;
        let dx = f32::from(pos.x) - f32::from(bounds.origin.x);
        let dy = f32::from(pos.y) - f32::from(bounds.origin.y);
        let inside = dx >= 0.0
            && dy >= 0.0
            && dx <= f32::from(bounds.size.width)
            && dy <= f32::from(bounds.size.height);
        inside.then(|| point((dx / self.view_scale) as i32, (dy / self.view_scale) as i32))
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
            self.session = Some(Session::Ready {
                runtime,
                doc,
                base,
                file: file.to_path_buf(),
                mtime,
                cache: RasterCache::default(),
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
            let buf = cache.raster(shown[0], shown[1]);
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

    /// The interactive preview element: the rasterized frame centered in the
    /// pane, with pointer pass-through into the RmlUi document.
    fn preview_element(
        &mut self,
        image: Arc<RenderImage>,
        shown: [u32; 2],
        scale: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        self.view_scale = scale;
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            // Pointer pass-through, scaled to the displayed image; presses
            // and releases are edge-detected via `mouse_down` so each is
            // sent exactly once (egui used egui-memory for the same effect).
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, _| {
                let Some(doc) = this.doc_pos(ev.position) else {
                    return;
                };
                if let Some(Session::Ready { runtime, .. }) = this.session.as_ref() {
                    runtime.mouse_move(doc.x, doc.y);
                }
                let _ = deerpunk_ui::drain_commands();
            }))
            .on_mouse_down(MouseButton::Left, {
                cx.listener(move |this, ev: &MouseDownEvent, _, _| {
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
            .child(
                img(image)
                    .w(px(shown[0] as f32 * scale))
                    .h(px(shown[1] as f32 * scale)),
            )
    }
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

        let pane_size = self
            .pane_bounds
            .get()
            .map(|b| b.size)
            .unwrap_or_else(|| size(px(MIN_SCREEN.0), px(MIN_SCREEN.1)));
        let target = screen_for(pane_size);

        let mut alive = false;
        let (status, body) = match self.open.clone() {
            None => (
                String::new(),
                div().flex_1().flex().items_center().justify_center().child(
                    div()
                        .text_sm()
                        .text_color(theme::TEXT_DIM)
                        .child("Open a .rml file to preview it"),
                ),
            ),
            Some((file, rel)) => match self.sync_session(&file, &rel, retry, target) {
                Err(reason) => {
                    self.view_scale = 0.0;
                    (
                        String::new(),
                        div()
                            .flex_1()
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
                            .child(div().text_sm().text_color(theme::YELLOW).child(reason)),
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
                    let scale = fit_scale(shown, pane_size);
                    let status = format!(
                        "{} · {}×{} · {} draws",
                        base.display(),
                        target.0,
                        target.1,
                        cache.draw_count()
                    );
                    match (scale > 0.0, image.clone()) {
                        (true, Some(image)) => {
                            (status, self.preview_element(image, shown, scale, cx))
                        }
                        _ => {
                            self.view_scale = 0.0;
                            (status, div().flex_1())
                        }
                    }
                }
            },
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
        let img_bounds = self.img_bounds.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::BG)
            // Direct children of this column: [#0 status bar, #1 body]. The
            // body's bounds are the actual preview region (pane minus the
            // status bar) and decide the RmlUi render target on the next
            // frame (the ticker picks the change up).
            .on_children_prepainted(move |bounds: Vec<Bounds<Pixels>>, _, _| {
                pane_bounds.set(bounds.get(1).copied());
            })
            // Status line, right-aligned in the top bar (weak, like egui's).
            .child(
                div()
                    .flex()
                    .justify_end()
                    .px_2()
                    .py_1()
                    .child(div().text_xs().text_color(theme::TEXT_DIM).child(status)),
            )
            // The body's own child (the displayed image) maps pointer events
            // into document pixels; captured by [`RmluiPanel::preview_element`]'s
            // sibling listener on the body.
            .child(
                body.on_children_prepainted(move |bounds: Vec<Bounds<Pixels>>, _, _| {
                    img_bounds.set(bounds.first().copied());
                }),
            )
    }
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
