//! Headless RmlUi widget renderer: load one `.rml`, inject data-model state
//! and software-rasterize the engine draw list to a PNG — same asset
//! resolution and frame semantics as the live preview, no GUI needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use deerpunk_engine_common::ui_draw::{UiDraw, UiFrame, UiTextureUpload, UiVertex};
use deerpunk_ui::{DataModelValue, UiDirAssets, UiRuntime, drain_commands};

const USAGE: &str = "\
usage: deerpunk-studio shot <file.rml> [options]

  <file.rml>       widget document (base = its nearest `ui` ancestor dir)
  --state <toml>   flat TOML table injected into the document's data model
  -o, --out <png>  output file (default <rml stem>.png in the current dir)
  --size <WxH>     render size (default 1280x720)
  --frames <N>     max settle frames (default 30)";

/// CLI entry: `deerpunk-studio shot <file.rml> [--state state.toml] [-o out.png]
/// [--size WxH] [--frames N]`. Returns the process exit code (0 ok, 1 on a
/// usage or runtime error, with the reason on stderr).
pub fn run(args: &[String]) -> i32 {
    let opts = match parse_args(args) {
        Ok(opts) => opts,
        Err(reason) => {
            eprintln!("shot: {reason}\n{USAGE}");
            return 1;
        }
    };

    let base = base_for(&opts.rml);
    let rel = opts
        .rml
        .strip_prefix(&base)
        .map(|rel| {
            rel.components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default();

    // RmlUi is process-global: exactly one runtime, a plain local (the type
    // is UI-thread-only and never Send, which a CLI call site satisfies).
    // The context must be named "main": widget Lua resolves
    // `rmlui.contexts["main"]` (Ui.main_context) for onshow handlers and
    // `ctx:OpenDataModel` — the registration `data_model_set` writes to.
    let runtime = UiRuntime::new(Box::new(UiDirAssets::new(base.clone())), opts.size, "main");
    if !runtime.is_ready() {
        eprintln!("shot: no usable fonts under {} — need fonts/*.ttf", base.display());
        return 1;
    }
    let Some(doc) = runtime.load_document(&rel) else {
        eprintln!("shot: failed to load {rel} under {}", base.display());
        return 1;
    };
    doc.show();

    // Inject AFTER show(): the document's onshow Lua defaults apply first,
    // so explicit state wins.
    let mut state_fields = 0usize;
    if let Some(path) = &opts.state {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("shot: cannot read {}: {e}", path.display());
                return 1;
            }
        };
        let table: toml::Table = match toml::from_str(&text) {
            Ok(table) => table,
            Err(e) => {
                eprintln!("shot: invalid TOML in {}: {e}", path.display());
                return 1;
            }
        };
        for (field, value) in toml_to_state(&table) {
            if doc.data_model_set(&field, value) {
                state_fields += 1;
            } else {
                eprintln!("shot: warning: data model rejected field {field:?}");
            }
        }
    }

    // RmlUi updates are wall-clock driven: poll with sleeps so animations
    // tick, stop once three consecutive frames carried no change.
    let mut shot = ShotState::default();
    let mut misses = 0u32;
    for _ in 0..opts.frames {
        match runtime.frame() {
            Some(frame) => {
                shot.apply(&frame);
                misses = 0;
            }
            None => {
                misses += 1;
                if misses >= 3 {
                    break;
                }
            }
        }
        // Lua cmd() output is meaningless headless — drop it every update.
        let _ = drain_commands();
        std::thread::sleep(Duration::from_millis(16));
    }

    let (w, h) = if shot.screen != [0, 0] {
        (shot.screen[0], shot.screen[1])
    } else {
        opts.size
    };
    let png = raster(&shot, w, h);
    if let Err(e) = image::RgbaImage::from_raw(w, h, png).unwrap().save(&opts.out) {
        eprintln!("shot: cannot write {}: {e}", opts.out.display());
        return 1;
    }
    println!(
        "{}: {}×{}, {} draws, {} textures, state {} fields",
        opts.out.display(),
        w,
        h,
        shot.draws.len(),
        shot.textures.len(),
        state_fields
    );
    0
}

// ----------------------------------------------------------------- options

/// Parsed `shot` command line.
struct ShotOpts {
    rml: PathBuf,
    state: Option<PathBuf>,
    out: PathBuf,
    size: (u32, u32),
    frames: u32,
}

/// Hand-rolled parse: one positional plus `--state`, `-o`/`--out`, `--size`,
/// `--frames`.
/// UI asset root for `rml`: its nearest ancestor directory named `ui` —
/// the engine's layout (`<gamedir>/ui/…`) — falling back to the file's
/// parent (same rule as the gpui live preview).
fn base_for(rml: &Path) -> PathBuf {
    rml.ancestors()
        .skip(1) // a file *named* `ui` is not its own base directory
        .find(|p| p.file_name().is_some_and(|n| n == "ui"))
        .or_else(|| rml.parent())
        .unwrap_or(Path::new(""))
        .to_path_buf()
}

/// Hand-rolled parse: one positional plus `--state`, `-o`/`--out`, `--size`,
/// `--frames`.
fn parse_args(argv: &[String]) -> Result<ShotOpts, String> {
    let mut rml = None;
    let mut state = None;
    let mut out = None;
    let mut size = (1280, 720);
    let mut frames = 30u32;
    let mut i = 0;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        match arg {
            "--state" => state = Some(PathBuf::from(value(argv, &mut i, arg)?)),
            "-o" | "--out" => out = Some(PathBuf::from(value(argv, &mut i, arg)?)),
            "--size" => size = parse_size(&value(argv, &mut i, arg)?)?,
            "--frames" => frames = parse_frames(&value(argv, &mut i, arg)?)?,
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
            _ => {
                if rml.is_some() {
                    return Err(format!("unexpected argument {arg:?}"));
                }
                rml = Some(PathBuf::from(arg));
            }
        }
        i += 1;
    }
    let rml = rml.ok_or("missing <file.rml>")?;
    let out = out.unwrap_or_else(|| {
        let stem = rml.file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty());
        PathBuf::from(format!("{}.png", stem.unwrap_or("shot")))
    });
    Ok(ShotOpts { rml, state, out, size, frames })
}

/// Value of the flag at `i`; advances `i` past it.
fn value(argv: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    match argv.get(*i + 1) {
        Some(v) => {
            *i += 1;
            Ok(v.clone())
        }
        None => Err(format!("{flag} needs a value")),
    }
}

/// `WxH`, both positive.
fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("invalid --size {s:?} — want WxH, e.g. 1280x720");
    let (w, h) = s.split_once('x').ok_or_else(bad)?;
    let w = w.parse().map_err(|_| bad())?;
    let h = h.parse().map_err(|_| bad())?;
    if w == 0 || h == 0 {
        return Err(bad());
    }
    Ok((w, h))
}

/// Positive frame count.
fn parse_frames(s: &str) -> Result<u32, String> {
    s.parse::<u32>()
        .ok()
        .filter(|&n| n >= 1)
        .ok_or_else(|| format!("invalid --frames {s:?} — want a positive count"))
}

/// Flatten a state table into `(field, value)` pairs: bools, numbers and
/// strings map to their [`DataModelValue`]; nested tables/arrays are skipped
/// (data models are flat).
fn toml_to_state(map: &toml::Table) -> Vec<(String, DataModelValue)> {
    map.iter()
        .filter_map(|(field, value)| {
            let value = match value {
                toml::Value::Boolean(b) => DataModelValue::Bool(*b),
                toml::Value::Integer(i) => DataModelValue::Number(*i as f64),
                toml::Value::Float(f) => DataModelValue::Number(*f),
                toml::Value::String(s) => DataModelValue::String(s.clone()),
                _ => return None,
            };
            Some((field.clone(), value))
        })
        .collect()
}

// ------------------------------------------------------------------ raster

/// Accumulated engine UI state, folded from frame deltas exactly like the
/// egui paint cache: meshes/textures persist until released, `draws` and
/// `screen` are replaced wholesale by every frame.
#[derive(Default)]
struct ShotState {
    textures: HashMap<u32, UiTextureUpload>,
    meshes: HashMap<u32, (Vec<UiVertex>, Vec<u32>)>,
    draws: Vec<UiDraw>,
    screen: [u32; 2],
}

impl ShotState {
    fn apply(&mut self, frame: &UiFrame) {
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
}

/// Rasterize `state` into a `w×h` RGBA8 (premultiplied) buffer: every draw
/// in order, each fragment composited premultiplied-over with rounded u32
/// math (clamped — straight-alpha texels can push rgb past src alpha).
fn raster(state: &ShotState, w: u32, h: u32) -> Vec<u8> {
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    paint(state, w, h, &mut |i, src| {
        let dst = &mut buf[i * 4..i * 4 + 4];
        let inv = 255 - u32::from(src[3]);
        for ch in 0..4 {
            let acc = u32::from(src[ch]) + (u32::from(dst[ch]) * inv + 127) / 255;
            dst[ch] = acc.min(255) as u8;
        }
    });
    buf
}

/// Paint every draw in order (painter's algorithm), calling `px(pixel_index,
/// fragment)` for each covered pixel. Draws with unknown mesh/texture or an
/// empty scissor are skipped — same skip rules as the egui painter.
fn paint(state: &ShotState, w: u32, h: u32, px: &mut dyn FnMut(usize, [u8; 4])) {
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
            let tri = [verts[idx[0] as usize], verts[idx[1] as usize], verts[idx[2] as usize]];
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
                [tex.rgba[o], tex.rgba[o + 1], tex.rgba[o + 2], tex.rgba[o + 3]]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn vert(pos: [f32; 2], color: [u8; 4]) -> UiVertex {
        UiVertex { pos, uv: [0.0, 0.0], color }
    }

    /// Two triangles over a square quad `(0,0)–(4,4)` sharing the diagonal.
    fn quad_mesh(id: u32, color: [u8; 4]) -> (u32, (Vec<UiVertex>, Vec<u32>)) {
        (
            id,
            (
                vec![
                    vert([0.0, 0.0], color),
                    vert([4.0, 0.0], color),
                    vert([4.0, 4.0], color),
                    vert([0.0, 4.0], color),
                ],
                vec![0, 1, 2, 0, 2, 3],
            ),
        )
    }

    /// One triangle covering the 1×1 target (its only pixel center (0.5, 0.5)).
    fn tri_mesh(id: u32, color: [u8; 4]) -> (u32, (Vec<UiVertex>, Vec<u32>)) {
        (
            id,
            (
                vec![
                    vert([0.0, 0.0], color),
                    vert([2.0, 0.0], color),
                    vert([0.0, 2.0], color),
                ],
                vec![0, 1, 2],
            ),
        )
    }

    fn tex(id: u32, width: u32, height: u32, rgba: Vec<u8>) -> (u32, UiTextureUpload) {
        (
            id,
            UiTextureUpload {
                width,
                height,
                rgba,
                filter_nearest: true,
            },
        )
    }

    fn draw(mesh_id: u32, texture_id: u32, scissor: Option<[i32; 4]>) -> UiDraw {
        UiDraw {
            mesh_id,
            translation: [0.0; 2],
            texture_id,
            scissor,
        }
    }

    fn state(
        meshes: Vec<(u32, (Vec<UiVertex>, Vec<u32>))>,
        textures: Vec<(u32, UiTextureUpload)>,
        draws: Vec<UiDraw>,
        screen: [u32; 2],
    ) -> ShotState {
        ShotState {
            meshes: meshes.into_iter().collect(),
            textures: textures.into_iter().collect(),
            draws,
            screen,
        }
    }

    fn close(px: &[u8], want: [u8; 4], tol: u8) {
        for c in 0..4 {
            assert!(
                (i32::from(px[c]) - i32::from(want[c])).abs() <= i32::from(tol),
                "{px:?} not within {tol} of {want:?}"
            );
        }
    }

    #[test]
    fn parse_args_defaults() {
        let opts = parse_args(&args(&["w/loading.rml"])).unwrap();
        assert_eq!(opts.rml, PathBuf::from("w/loading.rml"));
        assert_eq!(opts.state, None);
        assert_eq!(opts.out, PathBuf::from("loading.png"));
        assert_eq!(opts.size, (1280, 720));
        assert_eq!(opts.frames, 30);
    }

    #[test]
    fn parse_args_options() {
        let opts = parse_args(&args(&[
            "w/loading.rml",
            "--state",
            "/tmp/s.toml",
            "--out",
            "/tmp/x.png",
            "--size",
            "640x480",
            "--frames",
            "5",
        ]))
        .unwrap();
        assert_eq!(opts.state, Some(PathBuf::from("/tmp/s.toml")));
        assert_eq!(opts.out, PathBuf::from("/tmp/x.png"));
        assert_eq!(opts.size, (640, 480));
        assert_eq!(opts.frames, 5);
    }

    #[test]
    fn parse_args_rejects_bad_input() {
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["--state", "s.toml"])).is_err());
        assert!(parse_args(&args(&["a.rml", "b.rml"])).is_err());
        assert!(parse_args(&args(&["a.rml", "--wat"])).is_err());
        assert!(parse_args(&args(&["a.rml", "--size", "64"])).is_err());
        assert!(parse_args(&args(&["a.rml", "--size", "0x720"])).is_err());
        assert!(parse_args(&args(&["a.rml", "--frames", "0"])).is_err());
        assert!(parse_args(&args(&["a.rml", "-o"])).is_err());
    }

    #[test]
    fn toml_to_state_maps_scalars() {
        let table: toml::Table = toml::from_str(
            "flag = true\nwidth = 173\nscale = 0.5\nname = \"hi\"\n[nested]\nx = 1",
        )
        .unwrap();
        let fields = toml_to_state(&table);
        assert_eq!(fields.len(), 4, "nested tables are skipped: {fields:?}");
        assert!(fields.contains(&(("flag").to_string(), DataModelValue::Bool(true))));
        assert!(fields.contains(&("width".to_string(), DataModelValue::Number(173.0))));
        assert!(fields.contains(&("scale".to_string(), DataModelValue::Number(0.5))));
        assert!(fields.contains(&("name".to_string(), DataModelValue::String("hi".into()))));
    }

    #[test]
    fn quad_covers_each_pixel_exactly_once() {
        let st = state(
            vec![quad_mesh(7, [255, 255, 255, 255])],
            vec![tex(1, 4, 4, vec![255; 4 * 4 * 4])],
            vec![draw(7, 1, None)],
            [4, 4],
        );
        // Centers on the shared diagonal must be painted by exactly one tri.
        let mut covered = vec![0u32; 16];
        paint(&st, 4, 4, &mut |i, _| covered[i] += 1);
        assert!(covered.iter().all(|&n| n == 1), "coverage {covered:?}");
        // Opaque white over transparent stays exactly opaque white.
        let buf = raster(&st, 4, 4);
        for px in buf.chunks_exact(4) {
            assert_eq!(px, [255, 255, 255, 255]);
        }
    }

    #[test]
    fn premult_over_composite() {
        let white = vec![255u8; 4];
        // 50% premultiplied red over the transparent background stays itself:
        // src + dst·(1 − src_a) with dst = 0.
        let red_only = state(
            vec![tri_mesh(2, [128, 0, 0, 128])],
            vec![tex(1, 1, 1, white.clone())],
            vec![draw(2, 1, None)],
            [1, 1],
        );
        close(&raster(&red_only, 1, 1), [128, 0, 0, 128], 1);

        // …and over opaque white: [255, 127, 127, 255] — red adds nothing to
        // g/b, dst keeps (1 − 128/255) of its white.
        let over_white = state(
            vec![tri_mesh(1, [255, 255, 255, 255]), tri_mesh(2, [128, 0, 0, 128])],
            vec![tex(1, 1, 1, white)],
            vec![draw(1, 1, None), draw(2, 1, None)],
            [1, 1],
        );
        close(&raster(&over_white, 1, 1), [255, 127, 127, 255], 2);
    }

    #[test]
    fn scissor_clips_to_rect() {
        let st = state(
            vec![quad_mesh(7, [255, 255, 255, 255])],
            vec![tex(1, 4, 4, vec![255; 4 * 4 * 4])],
            vec![draw(7, 1, Some([0, 0, 2, 4]))],
            [4, 4],
        );
        let mut covered = vec![0u32; 16];
        paint(&st, 4, 4, &mut |i, _| covered[i] += 1);
        for y in 0..4usize {
            for x in 0..4usize {
                assert_eq!(covered[y * 4 + x], u32::from(x < 2), "pixel {x},{y}");
            }
        }
        let buf = raster(&st, 4, 4);
        let pixel = |x: usize, y: usize| &buf[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(pixel(1, 0), [255, 255, 255, 255]);
        assert_eq!(pixel(3, 0), [0, 0, 0, 0]);
    }
}
