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
#[path = "../src/inspector.rs"]
mod inspector;
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

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use deerpunk_studio::diagnostics::{Diag, DiagSeverity};
use image::RgbaImage;

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

// ===== Virtualization: big-list headless render behavior =====
//
// Both panels render through `uniform_list` (flat visible-row vector, fixed
// row height). These are functional tests only — a several-hundred-row
// synthetic payload renders headless, and collapsing a row (chevron handler
// body) changes the rasterized frame. Timing/perf measurements are run
// one-off by hand, never asserted here.

/// Synthetic entity with `components × props` scalar properties (plus the
/// transform group each component carries) — around 200 logical rows with
/// the default call: big next to the ~30-row viewport, cheap to render.
fn big_entity(components: usize, props: usize) -> serde_json::Value {
    let comps: Vec<serde_json::Value> = (0..components)
        .map(|c| {
            let kv: serde_json::Map<String, serde_json::Value> = (0..props)
                .map(|p| {
                    let v = if p % 10 == 0 {
                        serde_json::json!(format!("long-value-{}-{}", p, "x".repeat(80)))
                    } else {
                        serde_json::json!(p * 3)
                    };
                    (format!("prop_{p}"), v)
                })
                .collect();
            serde_json::json!({
                "slug": format!("comp_{c}"),
                "data": {
                    "transform": { "x": c, "y": c * 7, "z": 0 },
                    "props": kv,
                },
            })
        })
        .collect();
    serde_json::json!({ "index": 1, "name": "bench_entity", "prefabId": 7, "components": comps })
}

/// Distinct-pixel count between two rasters.
fn diff_pixels(a: &RgbaImage, b: &RgbaImage) -> usize {
    a.pixels().zip(b.pixels()).filter(|(x, y)| x.0 != y.0).count()
}

#[test]
fn live_detail_large_list_headless_renders_and_collapses() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    render_live_detail_large_list();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("live_detail big list: skipped; GPUI has no headless renderer on this platform");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render_live_detail_large_list() {
    use gpui_kit::{
        AppContext as _, Bounds, HeadlessAppContext, NoopTextSystem, Point, WindowBounds,
        WindowOptions, assets::Assets, px, size,
    };

    let entity = big_entity(4, 40);
    let map: HashMap<u32, String> = [(7u32, "player".to_string())].into_iter().collect();

    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(NoopTextSystem::new()),
        Arc::new(Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        fa::register(cx);
        gpui_kit::init(cx);
        theme::apply(cx);
    });

    let (window, view) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| live_detail::LiveDetail::new(&entity, &map)),
            )
            .expect("open headless live-detail window")
        });

    cx.update_window(window, |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .expect("render live-detail frame");
    let shot_open = cx.capture_screenshot(window).expect("capture open frame");

    // The ~240-row payload must actually paint (card fills/borders, chevron
    // icon shapes — text itself is noop under NoopTextSystem).
    let mut colors = HashSet::new();
    for y in 20..shot_open.height() - 20 {
        for x in 10..shot_open.width() - 10 {
            colors.insert(shot_open.get_pixel(x, y).0);
        }
    }
    assert!(
        colors.len() >= 8,
        "live-detail body is nearly uniform ({} colors): the list did not render",
        colors.len()
    );

    // Collapse the first component's card through the same toggle the
    // chevron click handler runs: its ~43 rows must drop out of the frame.
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| v.toggle("comp_0", cx));
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .expect("collapse re-render");
    let shot_collapsed = cx
        .capture_screenshot(window)
        .expect("capture collapsed frame");
    let diff = diff_pixels(&shot_open, &shot_collapsed);
    assert!(
        diff > 1000,
        "collapsing comp_0 changed only {diff} pixels: collapse does not alter the rendered list"
    );
}

#[test]
fn diagnostics_list_headless_renders_and_expands() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    render_diagnostics_list();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("diagnostics big list: skipped; GPUI has no headless renderer on this platform");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render_diagnostics_list() {
    use gpui_kit::{
        AppContext as _, Bounds, HeadlessAppContext, NoopTextSystem, Point, WindowBounds,
        WindowOptions, assets::Assets, px, size,
    };

    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(NoopTextSystem::new()),
        Arc::new(Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        fa::register(cx);
        gpui_kit::init(cx);
        theme::apply(cx);
    });

    let state = cx.update(|cx| cx.new(|_| state::StudioState::new()));
    let (window, panel) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |_, cx| {
                    cx.new(|cx| diagnostics::DiagnosticsPanel::with_bus(state.clone(), None, cx))
                },
            )
            .expect("open headless diagnostics window")
        });

    // 200 diagnostics (10:1 warnings:errors), two of the leading ones
    // expanded so their snippet boxes rasterize (text and font icons are
    // noop glyphs under NoopTextSystem).
    let diags: Vec<Diag> = (0..200)
        .map(|i| {
            let error = i % 10 == 0;
            let line = 10 + (i % 90) as u32;
            Diag {
                severity: if error {
                    DiagSeverity::Error
                } else {
                    DiagSeverity::Warning
                },
                message: format!(
                    "unused variable `value_{i}` in `frame_handler_{i}` — consider removing it or prefixing with an underscore"
                ),
                rendered: Some(format!(
                    "warning: unused variable `value_{i}`\n  --> src/game/frame_{i}.rs:{line}:9\n   |\n{line}  |     let value_{i} = compute({i});\n   |         ^^^^^^^ help: prefix with an underscore\n"
                )),
                file: Some(format!("src/game/frame_{i}.rs")),
                line: Some(line),
                col: Some(9),
                code: Some(if error {
                    "E0382".to_string()
                } else {
                    "unused_variables".to_string()
                }),
            }
        })
        .collect();
    cx.update(|cx| panel.update(cx, |p, _| p.push_for_test(diags)));
    cx.update(|cx| {
        panel.update(cx, |p, _| {
            // Both are errors → both live in the first section, on screen.
            p.set_expanded_for_test(0, true);
            p.set_expanded_for_test(10, true);
        })
    });

    cx.update_window(window, |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .expect("render diagnostics frame");
    let shot_open = cx.capture_screenshot(window).expect("capture open frame");

    // List body painted: snippet CARD fills + severity-colored borders, not
    // just the empty state's centered check glyph.
    let mut colors = HashSet::new();
    for y in 40..shot_open.height() - 20 {
        for x in 10..shot_open.width() - 10 {
            colors.insert(shot_open.get_pixel(x, y).0);
        }
    }
    assert!(
        colors.len() >= 16,
        "diagnostics body is nearly uniform ({} colors): the 200-row list did not render",
        colors.len()
    );

    // Collapse the second expanded diagnostic (its snippet sits inside the
    // visible rows): the raster must change.
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |p, _| p.set_expanded_for_test(10, false));
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .expect("collapse re-render");
    let shot_collapsed = cx
        .capture_screenshot(window)
        .expect("capture collapsed frame");
    let diff = diff_pixels(&shot_open, &shot_collapsed);
    assert!(
        diff > 500,
        "collapsing a diagnostic changed only {diff} pixels: snippet rows do not render"
    );
}

// ===== Entity Inspector: center-tab summon + single-line entity rows =====
//
// The Live sidebar's detail host moved to a shared Inspector center tab:
// every entity-row click applies the selection to `StudioState` and pings
// `LiveBus`; the workspace summons (first click) or re-activates (later
// clicks) that one tab. There is no mouse synthesis without gpui-kit's
// test-support feature (taffy pin conflict, deliberately off), so the
// "click" is the exact body the row's on_click runs: `live::select_row`.
// Entity payloads reuse `big_entity` — real component cards rasterize
// (borders/fills/chevrons), text stays noop under NoopTextSystem.

use deerpunk_studio::protocol::{CollectionDescriptor, CollectionGroup, CollectionMode};

/// A `serverentities` collection descriptor (id_field `index`, egui parity
/// with the entity rail button); `get_mode` adds the Get mode.
fn entities_descriptor(get_mode: bool) -> CollectionDescriptor {
    CollectionDescriptor {
        id: "serverentities".into(),
        label: "Entities".into(),
        group: CollectionGroup::Runtime,
        modes: if get_mode {
            vec![CollectionMode::List, CollectionMode::Get]
        } else {
            vec![CollectionMode::List]
        },
        id_field: "index".into(),
        nested_in: None,
    }
}

fn count_bright(shot: &RgbaImage, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
    let mut bright = 0usize;
    for y in y0..y1.min(shot.height()) {
        for x in x0..x1.min(shot.width()) {
            let [r, g, b, _] = shot.get_pixel(x, y).0;
            let luma = (2126 * r as u32 + 7152 * g as u32 + 722 * b as u32) / 10_000;
            if luma >= 140 {
                bright += 1;
            }
        }
    }
    bright
}

fn count_colors(shot: &RgbaImage, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
    let mut colors = HashSet::new();
    for y in y0..y1.min(shot.height()) {
        for x in x0..x1.min(shot.width()) {
            colors.insert(shot.get_pixel(x, y).0);
        }
    }
    colors.len()
}

#[test]
fn inspector_panel_headless_renders_detail_and_updates() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    render_inspector_panel();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("inspector panel: skipped; GPUI has no headless renderer on this platform");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render_inspector_panel() {
    use gpui_kit::{
        AppContext as _, Bounds, HeadlessAppContext, NoopTextSystem, Point, WindowBounds,
        WindowOptions, assets::Assets, px, size,
    };

    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(NoopTextSystem::new()),
        Arc::new(Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        fa::register(cx);
        gpui_kit::init(cx);
        theme::apply(cx);
    });

    let state = cx.update(|cx| cx.new(|_| state::StudioState::new()));
    let (window, _panel) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|cx| inspector::InspectorPanel::new(state.clone(), cx)),
            )
            .expect("open headless inspector window")
        });

    let draw = |cx: &mut gpui_kit::HeadlessAppContext,
                window|
     -> image::RgbaImage {
        cx.update_window(window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .expect("render inspector frame");
        cx.capture_screenshot(window).expect("capture inspector frame")
    };

    // No selection: the centered "Select an item" placeholder — a flat
    // background (its text is noop under NoopTextSystem).
    let shot_placeholder = draw(&mut cx, window);
    assert!(
        count_colors(&shot_placeholder, 0, 0, shot_placeholder.width(), shot_placeholder.height()) <= 4,
        "placeholder frame should be a flat background"
    );

    // An entity is selected (as an entity-row click would set it): the
    // panel rebuilds its memoized view and component cards rasterize.
    let entity = big_entity(3, 20);
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.prefab_map = HashMap::from([(7u32, "player".to_string())]);
            s.detail = Some(entity.clone());
            cx.notify();
        })
    });
    let shot_detail = draw(&mut cx, window);
    let (w, h) = (shot_detail.width(), shot_detail.height());
    assert!(
        count_colors(&shot_detail, 10, 20, w - 10, h - 20) >= 8,
        "inspector body is nearly uniform: the detail cards did not render"
    );
    let diff = diff_pixels(&shot_placeholder, &shot_detail);
    assert!(
        diff > 1000,
        "selecting an entity changed only {diff} pixels: the panel did not rebuild its view"
    );

    // A different selection swaps the snapshot (the memo is keyed on the
    // detail value, not built once).
    let mut other = big_entity(2, 12);
    other["name"] = serde_json::json!("second_entity");
    other["index"] = serde_json::json!(9);
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.detail = Some(other.clone());
            cx.notify();
        })
    });
    let shot_second = draw(&mut cx, window);
    let diff = diff_pixels(&shot_detail, &shot_second);
    assert!(
        diff > 500,
        "switching the selection changed only {diff} pixels: the detail view is stale"
    );
}

#[test]
fn entity_click_summons_inspector_center_tab() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    render_entity_click_summons();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("inspector summon: skipped; GPUI has no headless renderer on this platform");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render_entity_click_summons() {
    use gpui_kit::{
        AppContext as _, Bounds, HeadlessAppContext, NoopTextSystem, Point, WindowBounds,
        WindowOptions, assets::Assets, px, size,
    };

    let row1 = big_entity(3, 20);
    let mut row2 = big_entity(2, 12);
    row2["name"] = serde_json::json!("second_entity");
    row2["index"] = serde_json::json!(2);
    let raw = vec![row1.clone(), row2.clone()];

    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(NoopTextSystem::new()),
        Arc::new(Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        fa::register(cx);
        gpui_kit::init(cx);
        theme::apply(cx);
    });

    let (window, workspace) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1600.), px(900.)),
                    })),
                    focus: false,
                    show: false,
                    ..gpui_kit::component::TitleBar::window_options()
                },
                cx,
                |window, cx| cx.new(|cx| app::Workspace::new(window, cx)),
            )
            .expect("open headless studio window")
        });

    let (state, bus) = cx.update(|cx| {
        let ws = workspace.read(cx);
        (
            ws.studio_state_for_test().clone(),
            ws.live_bus_for_test().clone(),
        )
    });

    // The real list state a Live session would hold: cached rows for a
    // list-only collection, one of them selected by a click.
    cx.update(|cx| {
        state.update(cx, |s, _| {
            s.collections = vec![entities_descriptor(false)];
            s.active_collection = Some("serverentities".into());
            s.prefab_map = HashMap::from([(7u32, "player".to_string())]);
            s.lists.insert(
                "serverentities".into(),
                serde_json::json!({"time": 0, "count": 2, "rows": [row1, row2]}),
            );
        })
    });

    let tab = |cx: &mut gpui_kit::HeadlessAppContext| {
        cx.update(|cx| workspace.read(cx).inspector_tab_for_test(cx))
    };
    assert_eq!(
        tab(&mut cx),
        (false, false),
        "the Inspector tab must not exist before any entity click"
    );

    // First click: summons the Inspector as a center tab (an add activates
    // it) and stores the raw row as the detail snapshot.
    cx.update_window(window, |_, window, cx| {
        live::select_row(&state, &bus, false, "serverentities", "1", &raw, 0, cx);
        window.refresh();
    })
    .expect("first entity click");
    assert_eq!(
        tab(&mut cx),
        (true, true),
        "the first entity click must summon and select the Inspector center tab"
    );
    assert_eq!(cx.update(|cx| state.read(cx).detail.clone()), Some(raw[0].clone()));

    // Second click on another row: the same tab stays selected (one tab,
    // not one per entity) and the snapshot follows the new selection.
    cx.update_window(window, |_, window, cx| {
        live::select_row(&state, &bus, false, "serverentities", "2", &raw, 1, cx);
        window.refresh();
    })
    .expect("second entity click");
    assert_eq!(tab(&mut cx), (true, true));
    assert_eq!(cx.update(|cx| state.read(cx).detail.clone()), Some(raw[1].clone()));

    // The summoned tab actually paints the detail: card fills/borders and
    // chevron shapes rasterize in the center zone (text is noop).
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .expect("draw summoned inspector");
    let shot = cx.capture_screenshot(window).expect("capture summoned frame");
    let (w, h) = (shot.width(), shot.height());
    // Center zone: from the center content's left edge (rail + 250px left
    // dock ≈ 18% of the frame) across the detail cards — their borders and
    // chevron icons hug that edge and are the pixels that rasterize (text
    // is noop, fills are flat).
    let colors = count_colors(&shot, w * 3 / 16, h / 8, w - 20, h * 2 / 3);
    assert!(
        colors >= 8,
        "the Inspector tab body did not render the selected entity's detail ({colors} colors)"
    );
}

#[test]
fn live_rows_headless_render_and_click_selects() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    render_live_rows_click();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("live rows: skipped; GPUI has no headless renderer on this platform");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render_live_rows_click() {
    use gpui_kit::{
        AppContext as _, Bounds, HeadlessAppContext, NoopTextSystem, Point, WindowBounds,
        WindowOptions, assets::Assets, px, size,
    };

    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(NoopTextSystem::new()),
        Arc::new(Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        fa::register(cx);
        gpui_kit::init(cx);
        theme::apply(cx);
    });

    let state = cx.update(|cx| cx.new(|_| state::StudioState::new()));
    let bus = cx.update(|cx| cx.new(|_| live::LiveBus::new()));
    let (window, _panel) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|cx| live::LivePanel::new(state.clone(), bus.clone(), cx)),
            )
            .expect("open headless live window")
        });

    // One list+get collection with three positioned rows (the sidebar is
    // the only content — no pills, no accent anywhere before the click).
    let rows: Vec<serde_json::Value> = (0..3)
        .map(|i| {
            serde_json::json!({
                "index": i + 1,
                "name": format!("ent_{i}"),
                "prefabId": 7,
                "position": [i as f32 * 2.0, 0.0, -i as f32],
                "components": [],
            })
        })
        .collect();
    let raw = rows.clone();
    cx.update(|cx| {
        state.update(cx, |s, _| {
            s.collections = vec![entities_descriptor(true)];
            s.active_collection = Some("serverentities".into());
            s.prefab_map = HashMap::from([(7u32, "player".to_string())]);
            s.lists.insert(
                "serverentities".into(),
                serde_json::json!({"time": 0, "count": 3, "rows": rows}),
            );
        })
    });

    let draw = |cx: &mut gpui_kit::HeadlessAppContext,
                window|
     -> image::RgbaImage {
        cx.update_window(window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .expect("render live frame");
        cx.capture_screenshot(window).expect("capture live frame")
    };

    let shot_before = draw(&mut cx, window);
    let (w, h) = (shot_before.width(), shot_before.height());
    // Sidebar zone: the 250px column left of the detail split (never wider
    // than 1/4 of the frame at this window size); full height — a short
    // list hugs the top, so a band would miss it.
    let accent_before = count_bright(&shot_before, 0, 0, w / 4, h);
    assert!(
        accent_before <= 10,
        "unexpected accent pixels before the click ({accent_before})"
    );

    // Click the second row (get-mode): the selection is recorded and the
    // clicked row inverts to the accent fill — still one uniform line.
    cx.update_window(window, |_, _, cx| {
        live::select_row(&state, &bus, true, "serverentities", "2", &raw, 1, cx);
    })
    .expect("entity click");
    let selection = cx.update(|cx| {
        state
            .read(cx)
            .selection
            .as_ref()
            .map(|s| (s.col.clone(), s.index_str.clone()))
    });
    assert_eq!(
        selection,
        Some(("serverentities".into(), "2".into())),
        "the click must record the get-mode selection"
    );

    let shot_after = draw(&mut cx, window);
    let accent_after = count_bright(&shot_after, 0, 0, w / 4, h);
    assert!(
        accent_after >= accent_before + 100,
        "the selected row did not invert to the accent fill ({accent_before} → {accent_after})"
    );
    let diff = diff_pixels(&shot_before, &shot_after);
    assert!(
        diff > 500,
        "clicking a row changed only {diff} pixels: the list did not re-render its selection"
    );
}

// ===== RmlUi preview: canvas grid, zoom/fit, DataModel panel =====
//
// RmlUi is process-global, so the whole preview smoke stays one test. It
// previews the fixture project (tests/fixtures/ui — fonts/font.ttf, one of
// the exact names engine-ui registers, and widgets/binding_test.rml) in a
// headless window and asserts on rasterized pixels: the widget paints with
// RmlUi's own font engine (real glyph pixels even under NoopTextSystem),
// the canvas paints the grid around/behind the fitted frame, zoom scales
// the widget, and the DataModel panel column renders next to it.

#[test]
fn rmlui_canvas_grid_zoom_and_bindings() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    render_rmlui_canvas();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("rmlui canvas: skipped; GPUI has no headless renderer on this platform");
}

/// Pixels within `tol` per channel of `rgb` in the column strip
/// `x0..x1`, full height.
fn count_near(shot: &RgbaImage, x0: u32, x1: u32, rgb: [u8; 3], tol: i32) -> usize {
    let mut n = 0usize;
    for y in 0..shot.height() {
        for x in x0..x1.min(shot.width()) {
            let p = shot.get_pixel(x, y).0;
            if (p[0] as i32 - rgb[0] as i32).abs() <= tol
                && (p[1] as i32 - rgb[1] as i32).abs() <= tol
                && (p[2] as i32 - rgb[2] as i32).abs() <= tol
            {
                n += 1;
            }
        }
    }
    n
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render_rmlui_canvas() {
    use gpui_kit::{
        AppContext as _, Bounds, HeadlessAppContext, NoopTextSystem, Point, WindowBounds,
        WindowOptions, assets::Assets, point, px, size,
    };

    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui");
    let rml_path = fixture.join("widgets/binding_test.rml");
    let rel = "widgets/binding_test.rml".to_string();

    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(NoopTextSystem::new()),
        Arc::new(Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        fa::register(cx);
        gpui_kit::init(cx);
        theme::apply(cx);
    });

    let state = cx.update(|cx| cx.new(|_| state::StudioState::new()));
    let (window, panel) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|cx| rmlui::RmluiPanel::new(state.clone(), cx)),
            )
            .expect("open headless rmlui window")
        });

    cx.update(|cx| panel.update(cx, |p, cx| p.open(rml_path.clone(), rel.clone(), cx)));

    let draw = |cx: &mut gpui_kit::HeadlessAppContext, window| -> image::RgbaImage {
        cx.update_window(window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .expect("render rmlui frame");
        cx.capture_screenshot(window).expect("capture rmlui frame")
    };

    // Two frames: the open applies on the first render and the canvas paint
    // callback pins the canvas bounds; the second frame paints with them
    // (matching the live repaint cadence).
    let _ = draw(&mut cx, window);
    std::thread::sleep(std::time::Duration::from_millis(40));
    let shot_fit = draw(&mut cx, window);
    let (w, h) = (shot_fit.width(), shot_fit.height());
    let canvas_w = w - 280; // canvas area = row left of the DataModel panel

    // Widget painted: the fixture body fill (rgb(38,52,80)) must cover a
    // large area — the 400×300 doc rect at the fit scale.
    let body_fit = count_near(&shot_fit, 0, canvas_w, [38, 52, 80], 6);
    assert!(
        body_fit >= 50_000,
        "widget body painted only {body_fit} px: the preview did not render"
    );

    // Grid painted: canvas pixels that are neither the theme background nor
    // widget colors — the grid lines around and behind the fitted frame
    // (minors #6e6682, majors #3a3344, theme BG #1e1a24). The fixture
    // document is 400×300 of a ~920×770 screen, so most of the canvas shows
    // grid.
    let bg = [30, 26, 36];
    let widget = [[38, 52, 80], [90, 60, 110], [167, 139, 250], [60, 110, 90]];
    let near_any = |p: &[u8; 4]| {
        (0..3).all(|c| (p[c] as i32 - bg[c] as i32).abs() <= 4)
            || widget
                .iter()
                .any(|rgb| (0..3).all(|c| (p[c] as i32 - rgb[c] as i32).abs() <= 6))
    };
    let mut grid_px = 0usize;
    let mut grid_colors = HashSet::new();
    for y in 26..h {
        for x in 0..canvas_w {
            let p = shot_fit.get_pixel(x, y).0;
            if !near_any(&p) && p[3] == 255 {
                grid_px += 1;
                grid_colors.insert(p);
            }
        }
    }
    assert!(
        grid_px >= 1_500,
        "grid painted only {grid_px} px: no grid lines around the preview"
    );
    assert!(
        grid_colors.len() >= 2,
        "grid has {} distinct colors: minor and major tiers did not both render",
        grid_colors.len()
    );

    // Zoom 2x anchored inside the widget box (the exact handler the wheel
    // runs): the widget's painted area must grow substantially.
    cx.update_window(window, |_, _, cx| {
        panel.update(cx, |p, cx| {
            p.zoom_at_cursor(point(px(150.), px(150.)), 2.0, cx)
        })
    })
    .expect("zoom");
    let shot_zoom = draw(&mut cx, window);
    let body_zoom = count_near(&shot_zoom, 0, canvas_w, [38, 52, 80], 6);
    assert!(
        body_zoom >= body_fit * 3 / 2,
        "zoom did not scale the widget up ({body_fit} → {body_zoom} px)"
    );

    // Double-click fit (the same body the click handler runs): back to the
    // fitted view.
    cx.update_window(window, |_, _, cx| panel.update(cx, |p, cx| p.reset_view(cx)))
        .expect("reset view");
    let shot_reset = draw(&mut cx, window);
    let body_reset = count_near(&shot_reset, 0, canvas_w, [38, 52, 80], 6);
    assert!(
        body_reset >= body_fit * 3 / 4 && body_reset <= body_fit * 5 / 4,
        "fit-to-pane did not restore the fitted view ({body_fit} → {body_reset} px)"
    );

    // DataModel panel painted: the fixed right column must carry the panel
    // background plus section headers/row separators (≥2 colors; text is
    // noop under NoopTextSystem).
    let mut panel_colors = HashSet::new();
    for y in h / 4..3 * h / 4 {
        for x in (canvas_w + 1)..w {
            panel_colors.insert(shot_fit.get_pixel(x, y).0);
        }
    }
    assert!(
        panel_colors.len() >= 2,
        "DataModel panel column is uniform ({} colors): it did not render",
        panel_colors.len()
    );
}

// ===== Files tree: folder click-toggle + persistence across rebuilds =====
//
// gpui-kit's tree expands/collapses a folder row on click (TreeState's own
// entry on_click — the panel supplies only content). The Files panel records
// those toggles and re-applies them when a new scan rebuilds the items
// (`set_items` resets expansion otherwise). Rows are located through the
// per-row `debug_selector` ("files-row-<rel>"), recorded only with gpui's
// test-support; unlike the pixel tests above this needs synthetic clicks, so
// it runs on gpui's `TestAppContext` (TestPlatform) instead of the headless
// renderer — assertions are element-level (painted row bounds), which
// NoopTextSystem pixels cannot provide for rows that differ only by glyphs.

/// Expands into a gpui test running the body on macOS/Linux, skipping
/// elsewhere (parity with the pixel tests above; TestPlatform itself is
/// portable, but the FA/theme init used below mirrors the gated panels).
#[gpui::test]
fn files_tree_folder_click_toggles_and_persists(cx: &mut gpui::TestAppContext) {
    use deerpunk_studio::assets::{AssetEntry, AssetKind, ScanResult};
    use gpui::Modifiers;
    use gpui_kit::AppContext as _;

    fn entry(rel: &str) -> AssetEntry {
        AssetEntry {
            rel: rel.into(),
            kind: AssetKind::Config,
            size: 1,
            pack: None,
        }
    }
    let scan_full = || {
        vec![
            entry("a.txt"),
            entry("textures/blocks/stone.png"),
            entry("textures/ui.png"),
        ]
    };

    cx.update(|cx| {
        fa::register(cx);
        gpui_kit::init(cx);
        theme::apply(cx);
    });
    let state = cx.update(|cx| cx.new(|_| state::StudioState::new()));
    let bus = cx.update(|cx| cx.new(|_| viewers::ViewerBus::new()));
    let (_panel, window) =
        cx.add_window_view(|_, cx| files::FilesPanel::new(state.clone(), bus.clone(), cx));

    fn set_scan(
        window: &mut gpui::VisualTestContext,
        state: &gpui::Entity<state::StudioState>,
        entries: Vec<AssetEntry>,
    ) {
        window.update(|_, cx| {
            state.update(cx, |s, cx| {
                s.scan = Some(ScanResult {
                    entries,
                    errors: Vec::new(),
                });
                cx.notify();
            });
        });
        window.run_until_parked();
        window.update(|window, cx| window.draw(cx).clear(cx));
    }

    set_scan(window, &state, scan_full());
    let blocks = window
        .debug_bounds("files-row-textures/blocks")
        .expect("nested folder row painted");
    assert!(
        window
            .debug_bounds("files-row-textures/blocks/stone.png")
            .is_none(),
        "deeper folder must start collapsed (top level expanded)"
    );

    // Click 1: the nested folder opens and its file appears below it.
    window.simulate_click(blocks.center(), Modifiers::default());
    window.run_until_parked();
    window.update(|window, cx| window.draw(cx).clear(cx));
    let stone = window
        .debug_bounds("files-row-textures/blocks/stone.png")
        .expect("click on folder must reveal its children");
    assert!(
        stone.origin.y > blocks.origin.y,
        "revealed child must render below the folder row"
    );

    // Click 2: the same folder collapses again.
    window.simulate_click(blocks.center(), Modifiers::default());
    window.run_until_parked();
    window.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        window
            .debug_bounds("files-row-textures/blocks/stone.png")
            .is_none(),
        "second click must collapse the folder"
    );

    // Re-open, then force a tree rebuild (new scan with one more file): the
    // expansion must survive `set_items`.
    window.simulate_click(blocks.center(), Modifiers::default());
    window.run_until_parked();
    let mut rescan = scan_full();
    rescan.push(entry("textures/blocks/moss.png"));
    set_scan(window, &state, rescan);
    window
        .debug_bounds("files-row-textures/blocks/stone.png")
        .expect("expanded folder survives a rebuild");
    window
        .debug_bounds("files-row-textures/blocks/moss.png")
        .expect("new file appears inside the remembered expansion");

    // A folder that vanishes from a scan drops its stale toggle: rescanning
    // without `blocks/`, then back, leaves the folder collapsed by default.
    set_scan(window, &state, vec![entry("a.txt"), entry("textures/ui.png")]);
    set_scan(window, &state, scan_full());
    let blocks_again = window
        .debug_bounds("files-row-textures/blocks")
        .expect("folder row back");
    assert!(
        window
            .debug_bounds("files-row-textures/blocks/stone.png")
            .is_none(),
        "stale expansion must not leak across scans"
    );
    assert!(
        blocks_again.origin.y == blocks.origin.y,
        "folder row position unchanged after the round-trip"
    );
}
