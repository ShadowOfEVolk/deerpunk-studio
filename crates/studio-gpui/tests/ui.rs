//! Headless UI smoke test for the studio gpui frontend.
//!
//! `studio-gpui` is a bin-only crate (no lib target), so an integration test
//! cannot import it — the production module tree is mirrored here via
//! `#[path]` includes instead, and the real `Workspace` is built in a
//! headless window exactly the way `src/main.rs` builds it.
//!
//! The recipe follows gpui-kit's own `tests/rendering.rs`: a
//! `HeadlessAppContext` with the kit asset source and the platform's
//! offscreen renderer (`gpui_kit::platform::current_headless_renderer`); a
//! frame is drawn through `Window::draw` and captured with
//! `capture_screenshot`. The text system is gpui's `NoopTextSystem` — the
//! real platform text systems require main-thread platform creation, which
//! the cargo test harness does not provide; noop glyphs lay out with
//! deterministic advances and rasterize to nothing, so the assertions below
//! key on fills, borders and accent tints rather than glyph pixels.
//!
//! Element lookup (`gpui_kit::test::TestWindowExt::find`) is likewise
//! unavailable — the kit's `test` module is gated behind the kit's own
//! `test-support` feature, which this crate deliberately does not enable
//! (taffy pin conflict through gpui-fast) — so chrome presence is asserted
//! statistically from the captured pixels: top bar, view rail, dock and
//! status bar must each paint a non-uniform region, with or without a
//! restored `last_project`.

#![allow(dead_code)] // mirrored modules carry more surface than this test uses

#[path = "../src/app.rs"]
mod app;
#[path = "../src/console.rs"]
mod console;
#[path = "../src/cvars.rs"]
mod cvars;
#[path = "../src/diagnostics.rs"]
mod diagnostics;
#[path = "../src/fa.rs"]
mod fa;
#[path = "../src/files.rs"]
mod files;
#[path = "../src/launch_settings.rs"]
mod launch_settings;
#[path = "../src/live.rs"]
mod live;
#[path = "../src/live_detail.rs"]
mod live_detail;
#[path = "../src/modals.rs"]
mod modals;
#[path = "../src/puffin.rs"]
mod puffin;
#[path = "../src/puffin_flame.rs"]
mod puffin_flame;
#[path = "../src/rail.rs"]
mod rail;
#[path = "../src/rml.rs"]
mod rml;
#[path = "../src/rmlui.rs"]
mod rmlui;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/statusbar.rs"]
mod statusbar;
#[path = "../src/theme.rs"]
mod theme;
#[path = "../src/topbar.rs"]
mod topbar;
#[path = "../src/viewers.rs"]
mod viewers;

use std::{collections::HashSet, path::PathBuf, sync::Arc};

/// GPUI supplies a real offscreen renderer on macOS (Metal) and Linux (WGPU)
/// only; elsewhere this stays a no-op skip, same gating as gpui-kit's own
/// rendering tests.
#[test]
fn workspace_headless_renders_chrome() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    render_workspace_headless();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("workspace chrome: skipped; GPUI has no headless renderer on this platform");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render_workspace_headless() {
    use gpui_kit::{
        AppContext as _, Bounds, HeadlessAppContext, NoopTextSystem, Point, WindowBounds,
        WindowOptions, assets::Assets, component::TitleBar, px, size,
    };

    // `NoopTextSystem`, not `platform::current_platform(true).text_system()`:
    // the Mac platform asserts main-thread creation, which cargo's test
    // harness does not run on, and gpui's own test harness solves this the
    // same way. Noop glyphs lay out with deterministic em advances but
    // rasterize to nothing — assertions below therefore key on fills,
    // borders and accents, not on glyph antialiasing.
    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(NoopTextSystem::new()),
        Arc::new(Assets),
        gpui_kit::platform::current_headless_renderer,
    );

    // Boot order mirrors src/main.rs: the FA icon font must be in the text
    // system before anything lays out a glyph in `fa::family()` — a miss
    // panics layout — then the kit layers, then the studio theme.
    cx.update(|cx| {
        fa::register(cx);
        gpui_kit::init(cx);
        theme::apply(cx);
    });

    let (window, _workspace) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1600.), px(900.)),
                })),
                focus: false,
                show: false,
                // The TopBar renders inside the gpui-component TitleBar,
                // which owns dragging and the traffic lights — the window
                // must be created with the matching options (see
                // src/main.rs).
                ..TitleBar::window_options()
            },
            cx,
            |window, cx| cx.new(|cx| app::Workspace::new(window, cx)),
        )
        .expect("open headless studio window")
    });

    // Stand-in for gpui-kit's `TestWindowExt::render_frame` (the kit's
    // test-support feature is off): refresh, then draw the frame into the
    // window's scene — `capture_screenshot` renders exactly that scene.
    cx.update_window(window, |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .expect("render workspace frame");

    let shot = cx
        .capture_screenshot(window)
        .expect("offscreen rendering must be available");

    // Snapshot artifact — overwritten on every run.
    let snapshots = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    std::fs::create_dir_all(&snapshots).expect("create tests/snapshots");
    let png_path = snapshots.join("workspace_headless.png");
    shot.save(&png_path).expect("save workspace snapshot");

    assert!(
        shot.width() >= 800 && shot.height() >= 600,
        "unexpectedly small frame: {}x{}",
        shot.width(),
        shot.height()
    );

    // Whole-frame sanity: the theme background is near-black (#1e1a24), but
    // the chrome paints borders, fills and accent tints over it, so the
    // frame must not be uniform and must contain accent-bright pixels
    // (theme::ACCENT #a78bfa ≈ luma 153; the brighter text color never
    // rasterizes under NoopTextSystem).
    let mut min_luma = u32::MAX;
    let mut max_luma = 0u32;
    let mut bright = 0usize; // accent-bright fills (luma >= 140)
    let mut buckets = [0usize; 256]; // luminance histogram
    for pixel in shot.pixels() {
        let [r, g, b, _] = pixel.0;
        let luma = (2126 * r as u32 + 7152 * g as u32 + 722 * b as u32) / 10_000;
        min_luma = min_luma.min(luma);
        max_luma = max_luma.max(luma);
        buckets[luma as usize] += 1;
        if luma >= 140 {
            bright += 1;
        }
    }
    let distinct_luma = buckets.iter().filter(|count| **count > 0).count();
    assert!(
        max_luma - min_luma >= 24,
        "frame is near-uniform (luma {min_luma}..{max_luma}): the studio chrome did not render"
    );
    assert!(
        distinct_luma >= 8,
        "frame lacks gradation ({distinct_luma} luminance levels): chrome did not render"
    );
    assert!(
        bright >= 50,
        "no accent-bright chrome pixels ({bright}): rail buttons, tab indicators and topbar controls are missing"
    );

    // Chrome presence, DPI-independent: each element occupies a fractional
    // region of the frame and must paint at least two distinct colors there.
    // Deliberately content-agnostic — the tab bars, borders and labels of an
    // empty project satisfy this as well as a restored last_project.
    let (w, h) = (shot.width(), shot.height());
    let assert_painted = |name: &str, x0: u32, y0: u32, x1: u32, y1: u32| {
        let mut colors = HashSet::new();
        for y in y0..y1.min(h) {
            for x in x0..x1.min(w) {
                colors.insert(shot.get_pixel(x, y).0);
            }
        }
        assert!(
            colors.len() >= 2,
            "{name} region is uniform: the {name} element did not render"
        );
    };
    // Top bar strip (title bar + launch/connection controls).
    assert_painted("topbar", 0, 0, w, h / 10);
    // Status strip along the bottom edge.
    assert_painted("statusbar", 0, h * 9 / 10, w, h);
    // Far-left view rail between the top and bottom strips.
    assert_painted("rail", 0, h / 10, w / 14, h * 9 / 10);
    // Dock area center, starting inside the tab-bar zone (its labels do not
    // rasterize under NoopTextSystem, but the tab-bar background, bottom
    // edge line and panel borders do) and extending into the panel body.
    assert_painted("dock", w / 4, h / 12, 3 * w / 4, 3 * h / 8);
}
