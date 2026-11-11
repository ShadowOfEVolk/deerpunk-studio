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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Instant;

    use super::*;
    use super::super::raster::RasterCache;
    use deerpunk_engine_common::ui_draw::{UiDraw, UiMesh, UiTextureUpload, UiVertex};
    use deerpunk_ui::{UiDirAssets, UiRuntime};

    /// Copy of the game's `<gamedir>/shaders/ui.wgsl` (ohmyblock), so the
    /// tests exercise the real shader + preprocessor include resolution.
    const UI_WGSL: &str = r#"
#include "inc/color_space.wgsl"

struct Globals {
    screen: vec2<f32>,
    _pad: vec2<f32>,
}

@group(0) @binding(0) var<uniform> globals: Globals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
// Per-draw translations. Instance 0 is reserved: it stays (0,0) and the hud
// blit quad draws with it. UI draws read their own translation.
@group(2) @binding(0) var<storage, read> draw_translation: array<vec2<f32>>;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}

@vertex
fn vs_main(v: VsIn, @builtin(instance_index) instance: u32) -> VsOut {
    var o: VsOut;
    let pos = v.pos + draw_translation[instance];
    let x = (pos.x / globals.screen.x) * 2.0 - 1.0;
    let y = 1.0 - (pos.y / globals.screen.y) * 2.0;
    o.clip = vec4<f32>(x, y, 0.0, 1.0);
    o.uv = v.uv;
    o.color = v.color;
    return o;
}

@fragment
fn fs_main(v: VsOut) -> @location(0) vec4<f32> {
    // UI texture: Rgba8UnormSrgb → sample is linear. Vertex color: sRGB bytes /255 from RmlUi.
    let t_lin = textureSample(tex, samp, v.uv);
    let v_srgb = clamp(v.color, vec4<f32>(0.0), vec4<f32>(1.0));
    let rgb_lin = t_lin.rgb * srgb_to_linear(v_srgb.rgb);
    let a = t_lin.a * v_srgb.a;
    return vec4<f32>(rgb_lin, a);
}
"#;

    /// Copy of `<gamedir>/shaders/inc/color_space.wgsl` — the `#include`
    /// target resolved by [`ShaderPreprocessor::process_at`].
    const COLOR_SPACE_WGSL: &str = r#"
// sRGB ↔ linear (IEC 61966-2-1). Use for UI + final display encoding via sRGB swapchain.

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let low = c / 12.92;
    let high = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let low = c * 12.92;
    let high = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(high, low, c <= vec3<f32>(0.0031308));
}
"#;

    /// Self-contained shader root: `<root>/shaders/ui.wgsl` + include, like
    /// a gamedir. `tag` keeps parallel tests out of each other's way.
    fn shader_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deerpunk-studio-gpu-raster-test-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("shaders/inc")).expect("create shader root");
        std::fs::write(dir.join("shaders/ui.wgsl"), UI_WGSL).expect("write ui.wgsl");
        std::fs::write(dir.join("shaders/inc/color_space.wgsl"), COLOR_SPACE_WGSL)
            .expect("write color_space.wgsl");
        dir
    }

    fn vert(pos: [f32; 2], uv: [f32; 2], color: [u8; 4]) -> UiVertex {
        UiVertex { pos, uv, color }
    }

    /// Solid uv 0..1 quad, two triangles, integer-aligned.
    fn rect_mesh(id: u32, x: f32, y: f32, w: f32, h: f32, color: [u8; 4]) -> UiMesh {
        UiMesh {
            id,
            vertices: vec![
                vert([x, y], [0.0, 0.0], color),
                vert([x + w, y], [1.0, 0.0], color),
                vert([x + w, y + h], [1.0, 1.0], color),
                vert([x, y + h], [0.0, 1.0], color),
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
        }
    }

    /// Opaque diagonal gradient: black at TL/BR, white at TR/BL — vertex
    /// colors are the GPU/CPU agreement stress (linear round-trip vs byte
    /// barycentric).
    fn gradient_mesh(id: u32, x: f32, y: f32, w: f32, h: f32) -> UiMesh {
        let black = [0, 0, 0, 255];
        let white = [255, 255, 255, 255];
        UiMesh {
            id,
            vertices: vec![
                vert([x, y], [0.0, 0.0], black),
                vert([x + w, y], [1.0, 0.0], white),
                vert([x + w, y + h], [1.0, 1.0], black),
                vert([x, y + h], [0.0, 1.0], white),
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
        }
    }

    fn draw(mesh_id: u32, texture_id: u32, scissor: Option<[i32; 4]>) -> UiDraw {
        UiDraw {
            mesh_id,
            translation: [0.0; 2],
            texture_id,
            scissor,
        }
    }

    fn white_texture() -> (u32, UiTextureUpload) {
        (
            1,
            UiTextureUpload {
                width: 1,
                height: 1,
                rgba: vec![255, 255, 255, 255],
                filter_nearest: true,
            },
        )
    }

    /// 2×2 opaque checker (white/black/white/black), nearest-filtered:
    /// magnified 40× in the parity frame, NEAREST sampling must match.
    fn checker_texture() -> (u32, UiTextureUpload) {
        (
            2,
            UiTextureUpload {
                width: 2,
                height: 2,
                rgba: vec![
                    255, 255, 255, 255, 0, 0, 0, 255, //
                    0, 0, 0, 255, 255, 255, 255, 255, //
                ],
                filter_nearest: true,
            },
        )
    }

    /// Frame 1: mesh + texture deltas, five draws — nearest-textured quad,
    /// vertex-color gradient, scissored draw, offscreen geometry, and one
    /// quad whose resources are released in frame 2.
    fn parity_frame_1() -> UiFrame {
        let white = [255, 255, 255, 255];
        UiFrame {
            meshes: vec![
                rect_mesh(10, 10.0, 10.0, 80.0, 80.0, white),
                gradient_mesh(11, 120.0, 80.0, 240.0, 160.0),
                rect_mesh(12, 300.0, 300.0, 200.0, 120.0, white),
                rect_mesh(13, 600.0, 400.0, 100.0, 60.0, white),
                rect_mesh(14, 400.0, 100.0, 60.0, 60.0, white),
            ],
            draws: vec![
                draw(10, 2, None),
                draw(11, 1, None),
                draw(12, 1, Some([320, 320, 100, 60])),
                draw(13, 1, None),
                draw(14, 1, None),
            ],
            textures: HashMap::from([white_texture(), checker_texture()]),
            screen: [640, 480],
            ..Default::default()
        }
    }

    /// Frame 2: releases (mesh 14, texture 2) while the draw list still
    /// references both — those draws must vanish on both paths.
    fn parity_frame_2() -> UiFrame {
        UiFrame {
            released_meshes: vec![14],
            released: vec![2],
            draws: parity_frame_1().draws,
            screen: [640, 480],
            ..Default::default()
        }
    }

    fn compare(cpu: &[u8], gpu: &[u8], label: &str) {
        assert_eq!(gpu.len(), cpu.len(), "{label}: buffer length");
        let mut max = 0u32;
        let mut differing = 0usize;
        for (pc, pg) in cpu.chunks_exact(4).zip(gpu.chunks_exact(4)) {
            if pc != pg {
                differing += 1;
            }
            for (a, b) in pc.iter().zip(pg.iter()) {
                max = max.max(u32::from(a.abs_diff(*b)));
            }
        }
        eprintln!(
            "parity [{label}]: max channel diff {max}, differing pixels {differing}/{}",
            cpu.len() / 4
        );
        assert!(max <= 2, "{label}: max channel diff {max} > 2");
    }

    /// GPU vs CPU parity across the full session flow: deltas + draws,
    /// releases, empty draw list, resize replay. Tolerance ≤2 per BGRA
    /// channel (sRGB round-trip + blend float noise).
    #[test]
    #[cfg(target_os = "macos")]
    fn gpu_cpu_parity() {
        let root = shader_root("parity");
        let Some(mut gpu) = GpuRaster::new(&root) else {
            eprintln!("gpu_cpu_parity: no adapter/device — skipping");
            let _ = std::fs::remove_dir_all(&root);
            return;
        };
        let mut cache = RasterCache::default();
        let screen = [640u32, 480];

        let f1 = parity_frame_1();
        cache.apply(&f1);
        let cpu = cache.raster(screen[0], screen[1]);
        let g = gpu.raster(Some(&f1), screen).expect("gpu raster");
        compare(&cpu, &g, "frame 1");

        let f2 = parity_frame_2();
        cache.apply(&f2);
        let cpu = cache.raster(screen[0], screen[1]);
        let g = gpu.raster(Some(&f2), screen).expect("gpu raster");
        compare(&cpu, &g, "frame 2 (releases)");

        let f3 = UiFrame {
            draws: Vec::new(),
            screen,
            ..Default::default()
        };
        cache.apply(&f3);
        let cpu = cache.raster(screen[0], screen[1]);
        let g = gpu.raster(Some(&f3), screen).expect("gpu raster");
        compare(&cpu, &g, "empty draws");
        assert!(g.iter().all(|&b| b == 0), "empty draws must be transparent");

        // Resize with no fresh frame: the last draw list re-renders at the
        // new size (CPU: the cache re-rasters the same draws).
        let cpu = cache.raster(400, 300);
        let g = gpu.raster(None, [400, 300]).expect("gpu raster");
        compare(&cpu, &g, "resize replay");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Informational CPU vs GPU timing (no assertions): 1280×720, 200
    /// opaque quads, 60 frames after a warm-up encode.
    #[test]
    #[cfg(target_os = "macos")]
    fn timing_gpu_vs_cpu() {
        let root = shader_root("timing");
        let Some(mut gpu) = GpuRaster::new(&root) else {
            eprintln!("timing_gpu_vs_cpu: no adapter/device — skipping");
            let _ = std::fs::remove_dir_all(&root);
            return;
        };
        let (w, h) = (1280u32, 720u32);
        let white = [255, 255, 255, 255];
        let mut meshes = Vec::new();
        let mut draws = Vec::new();
        let mut id = 100;
        'grid: for row in 0..14u32 {
            for col in 0..15u32 {
                if meshes.len() == 200 {
                    break 'grid;
                }
                let (x, y) = ((col * 76) as f32, (row * 48) as f32);
                meshes.push(rect_mesh(id, x, y, 64.0, 64.0, white));
                draws.push(draw(id, 1, None));
                id += 1;
            }
        }
        let f_all = UiFrame {
            meshes,
            draws: draws.clone(),
            textures: HashMap::from([white_texture()]),
            screen: [w, h],
            ..Default::default()
        };
        let f_draw = UiFrame {
            draws,
            screen: [w, h],
            ..Default::default()
        };

        const FRAMES: usize = 60;
        let mut cache = RasterCache::default();
        cache.apply(&f_all);
        let at = Instant::now();
        for _ in 0..FRAMES {
            std::hint::black_box(cache.raster(w, h));
        }
        let cpu_ms = at.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

        std::hint::black_box(gpu.raster(Some(&f_all), [w, h]).expect("gpu raster"));
        let at = Instant::now();
        for _ in 0..FRAMES {
            let buf = gpu.raster(Some(&f_draw), [w, h]).expect("gpu raster");
            std::hint::black_box(&buf);
        }
        let gpu_ms = at.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

        eprintln!(
            "timing (1280×720, 200 quads, {FRAMES} frames): CPU {cpu_ms:.2} ms/frame, GPU {gpu_ms:.2} ms/frame"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Live smoke against a real game project, opt-in: set
    /// `DEERPUNK_STUDIO_GPU_SMOKE` to a widget path, e.g.
    /// `~/dev/gamedev/ohmyblock/gamedir/ui/widgets/loading_screen.rml`.
    /// Loads the document through the real `deerpunk-ui` runtime (the same
    /// `UiDirAssets` resolver as in-game, real fonts/RCSS) and renders ~30
    /// frames through both paths; the GPU path must never error and must
    /// produce a live image. Real RmlUi content blends translucent fills
    /// and linear-filtered textures — color spaces the two rasterizers
    /// legitimately disagree on — so the CPU comparison is reported, not
    /// asserted.
    #[test]
    #[cfg(target_os = "macos")]
    fn smoke_real_document() {
        let Some(path) = std::env::var_os("DEERPUNK_STUDIO_GPU_SMOKE").map(PathBuf::from) else {
            eprintln!(
                "smoke_real_document: set DEERPUNK_STUDIO_GPU_SMOKE=<file.rml> to run against a real widget"
            );
            return;
        };
        // Base = nearest ancestor dir named `ui` (the widgetshot rule); the
        // shader root is its parent — the gamedir (`shaders/ui.wgsl`).
        let mut base = None;
        let mut cur = path.as_path();
        while let Some(parent) = cur.parent() {
            if parent.file_name().and_then(|n| n.to_str()) == Some("ui") {
                base = Some(parent.to_path_buf());
                break;
            }
            cur = parent;
        }
        let Some(base) = base else {
            panic!("{} has no `ui` ancestor directory", path.display());
        };
        let rel = path
            .strip_prefix(&base)
            .expect("rel under ui base")
            .to_string_lossy()
            .into_owned();

        let runtime = UiRuntime::new(
            Box::new(UiDirAssets::new(base.clone())),
            (640, 480),
            "gpu-smoke",
        );
        assert!(
            runtime.is_ready(),
            "no usable fonts under {} — need fonts/*.ttf",
            base.display()
        );
        let doc = runtime
            .load_document(&rel)
            .unwrap_or_else(|| panic!("failed to load {rel} under {}", base.display()));
        doc.show();

        let shader_root = base.parent().expect("gamedir above ui root").to_path_buf();
        let Some(mut gpu) = GpuRaster::new(&shader_root) else {
            eprintln!("smoke_real_document: no adapter/device — skipping");
            return;
        };
        let mut cache = RasterCache::default();
        let screen = [640u32, 480];
        let mut frames = 0usize;
        let mut last_gpu = Vec::new();
        let mut last_cpu = Vec::new();
        let at = Instant::now();
        for _ in 0..30 {
            if let Some(frame) = runtime.frame() {
                cache.apply(&frame);
                let gpu_buf = gpu.raster(Some(&frame), screen).expect("gpu raster");
                last_cpu = cache.raster(screen[0], screen[1]);
                last_gpu = gpu_buf;
                frames += 1;
            }
            let _ = deerpunk_ui::drain_commands();
        }
        assert!(frames > 0, "runtime produced no frames for {rel:?}");
        if last_cpu.iter().any(|&b| b != 0) {
            assert!(
                last_gpu.iter().any(|&b| b != 0),
                "gpu output transparent while the cpu reference rendered content"
            );
        }
        let differing = last_gpu
            .chunks_exact(4)
            .zip(last_cpu.chunks_exact(4))
            .filter(|(g, c)| g != c)
            .count();
        let max_diff = last_gpu
            .iter()
            .zip(last_cpu.iter())
            .map(|(g, c)| g.abs_diff(*c))
            .max()
            .unwrap_or(0);
        eprintln!(
            "smoke [{rel}]: {frames} frames in {:.1} ms ({:.2} ms/frame), \
             gpu vs cpu: max diff {max_diff}, differing pixels {differing}/{}",
            at.elapsed().as_secs_f64() * 1000.0,
            at.elapsed().as_secs_f64() * 1000.0 / frames as f64,
            last_gpu.len() / 4
        );
    }
}
