//! Headless UI snapshot: renders the real StudioApp offscreen and saves a
//! PNG of the current frame. Usage:
//!   cargo run --example ui_snapshot -- [project_path] [out.png]
//! Optional: `--view <files|cvars|launch|puffin>` to open a different rail
//! view before rendering (stripped from the args passed to the app).

use eframe::egui;

use deerpunk_studio::app::View;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Pull out `--view <name>` so the app never sees it (it would treat it
    // as the project path).
    let mut view: Option<View> = None;
    let mut clean: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--view" {
            view = match it.next().as_deref() {
                Some("cvars") => Some(View::Cvars),
                Some("launch") => Some(View::LaunchSettings),
                Some("puffin") => Some(View::Puffin),
                _ => None,
            };
            if view.is_none() {
                return Err("--view expects files|cvars|launch|puffin".into());
            }
        } else {
            clean.push(a);
        }
    }
    args = clean;

    let out = if args.len() > 1 {
        args.pop().unwrap()
    } else {
        "snapshot.png".to_string()
    };

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_eframe(|cc| deerpunk_studio::app::StudioApp::new(cc, &args));

    if let Some(v) = view {
        harness.state_mut().active_view = v;
    }

    // A few frames immediately, then let background threads deliver data
    // (asset scan), then settle again.
    for _ in 0..4 {
        harness.step();
    }
    std::thread::sleep(std::time::Duration::from_millis(400));
    for _ in 0..6 {
        harness.step();
    }

    harness.snapshot("ui");

    // kittest stores snapshots under tests/snapshots/<name>.png.
    let stored = std::path::Path::new("tests/snapshots/ui.png");
    if stored.is_file() {
        std::fs::copy(stored, &out)?;
        println!("saved {out}");
    } else {
        println!("snapshot file not found at {}", stored.display());
    }
    Ok(())
}
