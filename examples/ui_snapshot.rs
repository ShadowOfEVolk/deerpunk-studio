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
                Some(v) if v.starts_with("live:") => {
                    Some(View::Live(v.strip_prefix("live:").unwrap().to_string()))
                }
                _ => None,
            };
            if view.is_none() {
                return Err("--view expects files|cvars|launch|puffin|live:<id>".into());
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
    std::thread::sleep(std::time::Duration::from_millis(1500));
    for _ in 0..6 {
        harness.step();
    }

    // Live collections can answer slowly: the real engine builds a 3k-row
    // reply in ~350ms AFTER connect+initialize, which can outrun the fixed
    // settle above. Keep pumping until the active collection arrives (or a
    // generous deadline) so snapshots capture populated views.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let waiting_for = match &harness.state_mut().active_view {
            View::Live(id) => Some(id.clone()),
            _ => None,
        };
        let Some(id) = waiting_for else { break };
        if harness.state_mut().lists.contains_key(&id) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        for _ in 0..2 {
            harness.step();
        }
    }
    for _ in 0..4 {
        harness.step();
    }

    // SNAPSHOT_SELECT="col:index" selects a row after data settles, so a
    // snapshot can capture the detail panel (entity.get round-trip).
    if let Ok(sel) = std::env::var("SNAPSHOT_SELECT") {
        if let Some((col, idx)) = sel.split_once(':') {
            harness.state_mut().select(col.to_string(), idx.to_string());
            for _ in 0..4 {
                harness.step();
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            for _ in 0..6 {
                harness.step();
            }
        }
    }

    // kittest stores snapshots under tests/snapshots/<name>.png; allow a
    // distinct name so concurrent runs don't clobber each other's frame.
    let name = std::env::var("SNAPSHOT_NAME").unwrap_or_else(|_| "ui".to_string());
    harness.snapshot(&name);

    let stored_path = format!("tests/snapshots/{name}.png");
    let stored = std::path::Path::new(&stored_path);
    if stored.is_file() {
        std::fs::copy(stored, &out)?;
        println!("saved {out}");
    } else {
        println!("snapshot file not found at {}", stored.display());
    }
    Ok(())
}
