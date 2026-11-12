//! GPU rasterization for the RmlUi live preview: the engine's `UiPass`
//! renders the accumulated [`UiFrame`] state offscreen (own wgpu device, no
//! surface) and the result is read back as dense BGRA8 — the byte convention
//! of gpui's `RenderImage`, same as the CPU fallback ([`super::raster`]).
//!
//! `UiPass` folds mesh/texture deltas internally exactly like `RasterCache`
//! (`sync_textures` + mesh uploads on every `encode` with `upload: true`);
//! draw lists arrive wholesale with every frame and are *not* folded, so the
//! last one is kept here to re-render on resize when no fresh frame landed.
//!
//! Target format is `Bgra8UnormSrgb`, not plain `Bgra8Unorm`: `UiPass`
//! composites in linear space through its sRGB HUD (`Rgba8UnormSrgb`), so an
//! un-encoded target would store linear bytes (mid-gray ≈ 54 instead of 128)
//! and gpui would render the preview washed-out dark. The sRGB target
//! re-encodes on store, landing in the same byte domain the CPU rasterizer
//! produces.

use std::path::Path;

use deerpunk_engine_common::material::ShaderPreprocessor;
use deerpunk_engine_common::ui_draw::{UiDraw, UiFrame};
use deerpunk_engine_renderer::ui_pass::UiPass;

/// sRGB-encoded BGRA: `UiPass` composites in linear space through its sRGB
/// HUD, so storing into the un-encoded `Bgra8Unorm` would hand gpui linear
/// bytes (mid-gray ≈ 54 instead of 128). The sRGB target re-encodes on
/// store, matching the CPU rasterizer's byte domain; alpha stays linear
/// (straight), also like the CPU path.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

/// Readback rows are `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`-aligned.
const READBACK_PAD: u32 = 256;

pub struct GpuRaster {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pass: UiPass,
    /// Color target + readback buffer, (re)created on resize.
    target: Option<Target>,
    /// Draw list of the last applied frame (draws are replaced wholesale
    /// per frame, like `RasterCache::draws`) — replayed on resize.
    draws: Vec<UiDraw>,
    /// True once a frame with a non-empty draw list was encoded: `UiPass`'s
    /// HUD target exists from then on, and later draw-less frames would
    /// blit that stale HUD over the cleared target unless blanked again.
    ever_drawn: bool,
}

struct Target {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    size: [u32; 2],
    bytes_per_row: u32,
}

impl GpuRaster {
    /// Create the offscreen device and compile `<base>/shaders/ui.wgsl` —
    /// `base` is the game's shader root (gamedir), the engine's `SHADER_DIR`
    /// equivalent; `#include`s resolve against `<base>/shaders/` via the
    /// engine's preprocessor, same as `load_wgsl_required` in-game.
    /// `None` — no adapter/device, missing shader, preprocessing error —
    /// is a sticky per-session fallback to the CPU rasterizer.
    pub fn new(base: &Path) -> Option<Self> {
        let shader_path = base.join("shaders").join("ui.wgsl");
        let source = match std::fs::read_to_string(&shader_path) {
            Ok(src) => src,
            Err(err) => {
                eprintln!("gpu raster: no {} ({err}) — CPU fallback", shader_path.display());
                return None;
            }
        };
        let Some(shader_dir) = shader_path.parent() else {
            return None;
        };
        let processed = match ShaderPreprocessor::process_at(&source, &[], Some(shader_dir)) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("gpu raster: ui.wgsl preprocess failed ({err}) — CPU fallback");
                return None;
            }
        };
        let instance = wgpu::Instance::default();
        let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: None,
            ..Default::default()
        })) {
            Ok(a) => a,
            Err(err) => {
                eprintln!("gpu raster: no adapter ({err:?}) — CPU fallback");
                return None;
            }
        };
        let (device, queue) = match pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor::default(),
        )) {
            Ok(dq) => dq,
            Err(err) => {
                eprintln!("gpu raster: no device ({err:?}) — CPU fallback");
                return None;
            }
        };
        let pass = UiPass::new(&device, processed.wgsl);
        Some(Self {
            device,
            queue,
            pass,
            target: None,
            draws: Vec::new(),
            ever_drawn: false,
        })
    }

    /// Rasterize the accumulated UI state into a `w×h` dense BGRA8
    /// (straight-alpha, sRGB-encoded) buffer. `Some(frame)` folds that
    /// frame's deltas first (meshes/textures inside `UiPass`, the draw list
    /// here); `None` re-renders the last draw list — the resize path, where
    /// the CPU cache re-rasters the same draws at the new size. Empty draws
    /// rasterize to a fully transparent buffer, like the CPU path. `None`
    /// return = runtime error; the caller pins the CPU fallback for the
    /// session after the first one.
    pub fn raster(&mut self, frame: Option<&UiFrame>, screen: [u32; 2]) -> Option<Vec<u8>> {
        let [w, h] = screen;
        if w == 0 || h == 0 {
            return Some(Vec::new());
        }
        if let Some(f) = frame {
            self.draws = f.draws.clone();
        }

        // (Re)create the color target + readback buffer on resize.
        if !self.target.as_ref().is_some_and(|t| t.size == [w, h]) {
            let bytes_per_row = (w * 4).div_ceil(READBACK_PAD) * READBACK_PAD;
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("gpui ui raster target"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TARGET_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gpui ui raster readback"),
                size: bytes_per_row as u64 * h as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.target = Some(Target {
                tex,
                view,
                readback,
                size: [w, h],
                bytes_per_row,
            });
        }
        let target = self.target.as_mut().expect("target just created");

        // Frame to encode: the fresh frame (deltas + draws), screen-
        // normalized when frame and output sizes disagree (resize
        // transient); without a fresh frame, a replay of the last draw list
        // at the requested size (resize re-render, like the CPU cache).
        let replay: Option<UiFrame>;
        let encode_frame: Option<&UiFrame> = match frame {
            Some(f) if f.screen == screen => Some(f),
            Some(f) => {
                replay = Some(UiFrame {
                    screen,
                    ..f.clone()
                });
                replay.as_ref()
            }
            None => {
                replay = (!self.draws.is_empty()).then(|| UiFrame {
                    draws: self.draws.clone(),
                    screen,
                    ..UiFrame::default()
                });
                replay.as_ref()
            }
        };

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("gpui ui raster"),
        });
        // `UiPass` blits with `LoadOp::Load` (over the in-game scene), so
        // the offscreen target is cleared here every frame.
        clear_target(&mut encoder, &target.view);
        if let Some(f) = encode_frame {
            if !f.is_empty() {
                self.pass.encode(
                    &self.device,
                    &self.queue,
                    &mut encoder,
                    &target.view,
                    TARGET_FORMAT,
                    f,
                    true,
                    None,
                );
                if f.draws.is_empty() && self.ever_drawn {
                    // Draw-less frame after content existed: `UiPass` still
                    // blitted the stale HUD; blank it again — the CPU cache
                    // paints nothing for an empty draw list.
                    clear_target(&mut encoder, &target.view);
                }
            }
            if !f.draws.is_empty() {
                self.ever_drawn = true;
            }
        }
        encoder.copy_texture_to_buffer(
            target.tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &target.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(target.bytes_per_row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let (tx, rx) = std::sync::mpsc::channel();
        target.readback.slice(..).map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        if let Err(err) = self.device.poll(wgpu::PollType::wait_indefinitely()) {
            eprintln!("gpu raster: poll failed ({err:?}) — CPU fallback");
            return None;
        }
        let mapped = rx.recv().ok().and_then(|r| r.ok());
        if mapped.is_none() {
            eprintln!("gpu raster: readback map failed — CPU fallback");
            return None;
        }
        let data = target
            .readback
            .slice(..)
            .get_mapped_range()
            .map_err(|err| eprintln!("gpu raster: map range failed ({err:?}) — CPU fallback"))
            .ok()?;

        // Strip the 256-byte row padding into a dense w×h buffer.
        let stride = target.bytes_per_row as usize;
        let row_bytes = (w * 4) as usize;
        let mut out = Vec::with_capacity(row_bytes * h as usize);
        for row in 0..h as usize {
            out.extend_from_slice(&data[row * stride..row * stride + row_bytes]);
        }
        drop(data);
        target.readback.unmap();

        // gpui's `RenderImage` is straight-alpha; the pass accumulates
        // premultiplied — unpremultiply like the CPU path's final pass.
        for px in out.chunks_exact_mut(4) {
            let a = u32::from(px[3]);
            if (1..255).contains(&a) {
                for ch in 0..3 {
                    px[ch] = ((u32::from(px[ch]) * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        Some(out)
    }
}

fn clear_target(encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
    let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("gpui ui raster clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    drop(pass);
}

