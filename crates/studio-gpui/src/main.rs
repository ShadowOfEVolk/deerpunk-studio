//! deerpunk studio — gpui-kit frontend entry point.

mod app;
mod console;
mod cvars;
mod diagnostics;
mod fa;
mod files;
mod inspector;
mod launch_settings;
mod live;
mod live_detail;
mod menus;
mod modals;
mod puffin;
mod puffin_flame;
mod rail;
mod rml;
mod rmlui;
mod state;
mod statusbar;
mod theme;
mod topbar;
mod viewers;

use gpui_kit::*;

const WINDOW_TITLE: &str = "deerpunk studio";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("shot") => std::process::exit(deerpunk_studio::widgetshot::run(&args[1..])),
        Some("tree") => std::process::exit(deerpunk_studio::widgettree::run(&args[1..])),
        _ => {}
    }
    application().with_assets(assets::Assets).run(|cx| {
        // The FA icon font must be in the text system before anything
        // lays out a glyph in `fa::family()` — a miss panics layout.
        fa::register(cx);
        init(cx);
        theme::apply(cx);
        // Native menu bar (File → Open Project… / Open Recent ▸) — must
        // be declared at startup, before the window opens.
        menus::init(cx);

        let bounds = Bounds::centered(None, size(px(1600.), px(900.)), cx);
        let (handle, _) = open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(gpui_kit::Size {
                    width: px(640.),
                    height: px(480.),
                }),
                // The TopBar renders inside the gpui-component TitleBar,
                // which owns dragging and the traffic lights — the
                // window must be created with the matching options.
                ..gpui_kit::component::TitleBar::window_options()
            },
            cx,
            |window, cx| cx.new(|cx| app::Workspace::new(window, cx)),
        )
        .expect("failed to open studio window");

        handle
            .update(cx, |_, window, _| {
                window.set_window_title(WINDOW_TITLE);
                window.activate_window();
            })
            .ok();
    });
}
