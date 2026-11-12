//! Behavioral headless UI tests for the studio gpui frontend.
//!
//! `studio-gpui` is a bin-only crate (no lib target), so an integration test
//! cannot import it — the production module tree is mirrored here via
//! `#[path]` includes instead, and the real `Workspace` is built in a
//! headless window exactly the way `src/main.rs` builds it.
//!
//! A headless window is still opened through the same gpui-kit recipe the
//! render tests used (a `HeadlessAppContext` with the kit asset source and
//! the platform's offscreen renderer,
//! `gpui_kit::platform::current_headless_renderer`), so click handlers run
//! against real element trees. Assertions are behavioral: application and
//! entity state, plus element bounds through `debug_selector` (gpui's
//! test-support) — never rasterized pixel statistics. The earlier
//! pixel-statistical assertions (distinct colors, bright pixels, frame
//! diffs) only helped while authoring a component and are renderer-fragile:
//! Metal and WGPU antialiasing quantize differently, which already caused a
//! false CI failure.
//!
//! The text system is gpui's `NoopTextSystem` — the real platform text
//! systems require main-thread platform creation, which the cargo test
//! harness does not provide; noop glyphs lay out with deterministic
//! advances and rasterize to nothing, so nothing here depends on glyph
//! pixels.
//!
//! Element lookup through gpui-kit's `TestWindowExt::find` is unavailable —
//! the kit's `test` module is gated behind the kit's own `test-support`
//! feature, which this crate deliberately does not enable (taffy pin
//! conflict through gpui-fast) — so synthetic clicks run on gpui's own
//! `TestAppContext` instead.

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

use std::{collections::HashMap, sync::Arc};

/// Synthetic entity with `components × props` scalar properties (plus the
/// transform group each component carries) — around 200 logical rows with
/// the default call.
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

// ===== Entity Inspector: center-tab summon + single-line entity rows =====
//
// The Live sidebar's detail host moved to a shared Inspector center tab:
// every entity-row click applies the selection to `StudioState` and pings
// `LiveBus`; the workspace summons (first click) or re-activates (later
// clicks) that one tab. There is no mouse synthesis without gpui-kit's
// test-support feature (taffy pin conflict, deliberately off), so the
// "click" is the exact body the row's on_click runs: `live::select_row`.

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
}

#[test]
fn live_rows_headless_click_selects() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    run_live_rows_click();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("live rows: skipped; GPUI has no headless renderer on this platform");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run_live_rows_click() {
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

    // One list+get collection with three positioned rows.
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

    // Click the second row (get-mode): the selection is recorded the same
    // way the row's on_click records it.
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
}

// ===== Files tree: folder click-toggle + persistence across rebuilds =====
//
// gpui-kit's tree expands/collapses a folder row on click (TreeState's own
// entry on_click — the panel supplies only content). The Files panel records
// those toggles and re-applies them when a new scan rebuilds the items
// (`set_items` resets expansion otherwise). Rows are located through the
// per-row `debug_selector` ("files-row-<rel>"), recorded only with gpui's
// test-support; this needs synthetic clicks, so it runs on gpui's
// `TestAppContext` (TestPlatform) instead of the headless renderer —
// assertions are element-level (painted row bounds).

/// Expands into a gpui test running the body on macOS/Linux, skipping
/// elsewhere (TestPlatform itself is portable, but the FA/theme init used
/// below mirrors the headless panels).
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
