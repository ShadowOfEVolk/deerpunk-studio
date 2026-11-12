//! Software rasterizer for engine UI draw lists: folds [`UiFrame`] deltas
//! and paints them into a BGRA8 buffer — the byte order + alpha convention
//! of gpui's `RenderImage` (BGRA, straight alpha, matching gpui's own image
//! decoders, which swap RGBA→BGRA on upload). Ported from the headless
//! PNG-shot pipeline (src/widgetshot.rs), minus the PNG encode.

use std::collections::HashMap;

use deerpunk_engine_common::ui_draw::{UiDraw, UiFrame, UiTextureUpload, UiVertex};

/// Accumulated engine UI state, folded from frame deltas exactly like the
/// egui paint cache: meshes/textures persist until released, `draws` and
/// `screen` are replaced wholesale by every frame.
#[derive(Default)]
pub struct RasterCache {
    textures: HashMap<u32, UiTextureUpload>,
    meshes: HashMap<u32, (Vec<UiVertex>, Vec<u32>)>,
    draws: Vec<UiDraw>,
    screen: [u32; 2],
}

impl RasterCache {
    /// Fold one frame's deltas in.
    pub fn apply(&mut self, frame: &UiFrame) {
        for (id, upload) in &frame.textures {
            self.textures.insert(*id, upload.clone());
        }
        for id in &frame.released {
            self.textures.remove(id);
        }
        for mesh in &frame.meshes {
            self.meshes
                .insert(mesh.id, (mesh.vertices.clone(), mesh.indices.clone()));
        }
        for id in &frame.released_meshes {
            self.meshes.remove(id);
        }
        self.draws = frame.draws.clone();
        self.screen = frame.screen;
    }

    /// Screen size carried by the last applied frame (`[0, 0]` before the
    /// first one).
    pub fn screen(&self) -> [u32; 2] {
        self.screen
    }

    /// Draw count of the last applied frame.
    pub fn draw_count(&self) -> usize {
        self.draws.len()
    }

    /// Rasterize into a `w×h` BGRA8 (straight-alpha) buffer: every draw in
    /// order, each fragment composited premultiplied-over with rounded u32
    /// math (clamped — straight-alpha texels can push rgb past src alpha),
    /// then unpremultiplied and R/B-swapped for gpui.
    pub fn raster(&self, w: u32, h: u32) -> Vec<u8> {
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        paint(self, w, h, &mut |i, src| {
            let dst = &mut buf[i * 4..i * 4 + 4];
            let inv = 255 - u32::from(src[3]);
            for ch in 0..4 {
                let acc = u32::from(src[ch]) + (u32::from(dst[ch]) * inv + 127) / 255;
                dst[ch] = acc.min(255) as u8;
            }
        });
        for px in buf.chunks_exact_mut(4) {
            let a = u32::from(px[3]);
            if a > 0 {
                for ch in 0..3 {
                    px[ch] = ((u32::from(px[ch]) * 255 + a / 2) / a).min(255) as u8;
                }
            }
            px.swap(0, 2); // RGBA → BGRA
        }
        buf
    }
}

/// Paint every draw in order (painter's algorithm), calling `px(pixel_index,
/// fragment)` for each covered pixel. Draws with unknown mesh/texture or an
/// empty scissor are skipped — same skip rules as the egui painter.
fn paint(state: &RasterCache, w: u32, h: u32, px: &mut dyn FnMut(usize, [u8; 4])) {
    for draw in &state.draws {
        let Some(mesh) = state.meshes.get(&draw.mesh_id) else {
            continue;
        };
        let Some(tex) = state.textures.get(&draw.texture_id) else {
            continue;
        };
        let Some(clip) = clip_rect(draw.scissor, w, h) else {
            continue;
        };
        let (verts, indices) = mesh;
        for idx in indices.chunks_exact(3) {
            let tri = [
                verts[idx[0] as usize],
                verts[idx[1] as usize],
                verts[idx[2] as usize],
            ];
            raster_tri(&tri, draw.translation, tex, clip, w, px);
        }
    }
}

/// Draw scissor (`x, y, w, h`, y-down) ∩ target as exclusive `x0, y0, x1, y1`
/// pixel bounds; `None` when empty.
fn clip_rect(scissor: Option<[i32; 4]>, w: u32, h: u32) -> Option<(i32, i32, i32, i32)> {
    let (x0, y0, x1, y1) = match scissor {
        Some([sx, sy, sw, sh]) if sw > 0 && sh > 0 => (sx, sy, sx + sw, sy + sh),
        Some(_) => return None,
        None => (0, 0, w as i32, h as i32),
    };
    let (x0, x1) = (x0.clamp(0, w as i32), x1.clamp(0, w as i32));
    let (y0, y1) = (y0.clamp(0, h as i32), y1.clamp(0, h as i32));
    (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
}

/// cross(b − a, p − a): positive when `p` is on the inward side of the a→b
/// edge of a winding-normalized triangle.
fn edge(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

/// Rasterize one translated triangle into `clip`, calling `px` per covered
/// pixel center. Top-left fill rule: centers exactly on a shared edge belong
/// to the triangle whose edge is top/left, so quad diagonals never
/// double-blend. Samples `tex` NEAREST (uv clamped to [0, 1)); fragment =
/// texel ⊙ vertex color, both premultiplied.
fn raster_tri(
    tri: &[UiVertex; 3],
    translation: [f32; 2],
    tex: &UiTextureUpload,
    clip: (i32, i32, i32, i32),
    w: u32,
    px: &mut dyn FnMut(usize, [u8; 4]),
) {
    if tex.width == 0 || tex.height == 0 {
        return;
    }
    let at = |mut v: UiVertex| {
        v.pos = [v.pos[0] + translation[0], v.pos[1] + translation[1]];
        v
    };
    let v0 = at(tri[0]);
    let mut v1 = at(tri[1]);
    let mut v2 = at(tri[2]);
    let area = edge(v0.pos, v1.pos, v2.pos);
    if area == 0.0 {
        return;
    }
    // Normalize winding so `inside` means "every edge function ≥ 0".
    if area < 0.0 {
        std::mem::swap(&mut v1, &mut v2);
    }
    // Top-left rule: an E == 0 boundary counts only for horizontal edges
    // going right (top edge, interior below) and edges going up (left edge,
    // interior to the right) — y-down coordinates.
    let boundary_ok = |d: [f32; 2]| (d[1] == 0.0 && d[0] > 0.0) || d[1] < 0.0;
    let (ok0, ok1, ok2) = (
        boundary_ok([v2.pos[0] - v1.pos[0], v2.pos[1] - v1.pos[1]]),
        boundary_ok([v0.pos[0] - v2.pos[0], v0.pos[1] - v2.pos[1]]),
        boundary_ok([v1.pos[0] - v0.pos[0], v1.pos[1] - v0.pos[1]]),
    );
    // Pixel centers inside the bbox ∩ clip: x ∈ [ceil(min − ½), floor(max − ½)].
    let xs = ((v0.pos[0].min(v1.pos[0]).min(v2.pos[0]) - 0.5).ceil() as i32).max(clip.0);
    let xe = ((v0.pos[0].max(v1.pos[0]).max(v2.pos[0]) - 0.5).floor() as i32).min(clip.2 - 1);
    let ys = ((v0.pos[1].min(v1.pos[1]).min(v2.pos[1]) - 0.5).ceil() as i32).max(clip.1);
    let ye = ((v0.pos[1].max(v1.pos[1]).max(v2.pos[1]) - 0.5).floor() as i32).min(clip.3 - 1);

    for y in ys..=ye {
        let cy = y as f32 + 0.5;
        for x in xs..=xe {
            let c = [x as f32 + 0.5, cy];
            let e0 = edge(v1.pos, v2.pos, c);
            let e1 = edge(v2.pos, v0.pos, c);
            let e2 = edge(v0.pos, v1.pos, c);
            let covers = |e: f32, ok: bool| e > 0.0 || (e == 0.0 && ok);
            if !covers(e0, ok0) || !covers(e1, ok1) || !covers(e2, ok2) {
                continue;
            }
            let total = e0 + e1 + e2;
            if total <= 0.0 {
                continue; // float-guard; covered centers always sum positive
            }
            let (b0, b1, b2) = (e0 / total, e1 / total, e2 / total);
            let u = b0 * v0.uv[0] + b1 * v1.uv[0] + b2 * v2.uv[0];
            let v = b0 * v0.uv[1] + b1 * v1.uv[1] + b2 * v2.uv[1];
            let tw = tex.width as usize;
            let th = tex.height as usize;
            let tx = ((u.clamp(0.0, 1.0) * tw as f32) as usize).min(tw - 1);
            let ty = ((v.clamp(0.0, 1.0) * th as f32) as usize).min(th - 1);
            let o = (ty * tw + tx) * 4;
            let texel = if o + 3 < tex.rgba.len() {
                [
                    tex.rgba[o],
                    tex.rgba[o + 1],
                    tex.rgba[o + 2],
                    tex.rgba[o + 3],
                ]
            } else {
                [0, 0, 0, 0] // short upload reads transparent, like the preview
            };
            let mut frag = [0u8; 4];
            for ch in 0..4 {
                let lerp = b0 * f32::from(v0.color[ch])
                    + b1 * f32::from(v1.color[ch])
                    + b2 * f32::from(v2.color[ch]);
                let vert = lerp.round().clamp(0.0, 255.0) as u8;
                frag[ch] = ((u16::from(texel[ch]) * u16::from(vert) + 127) / 255) as u8;
            }
            px(y as usize * w as usize + x as usize, frag);
        }
    }
}

