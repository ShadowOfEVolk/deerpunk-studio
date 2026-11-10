//! Headless UI snapshot: renders the real StudioApp offscreen and saves a
//! PNG of the current frame. Usage:
//!   cargo run --example ui_snapshot -- [project_path] [out.png]

use eframe::egui;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let out = if args.len() > 1 {
        args.pop().unwrap()
    } else {
        "snapshot.png".to_string()
    };

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_eframe(|cc| deerpunk_studio::app::StudioApp::new(cc, &args));

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
