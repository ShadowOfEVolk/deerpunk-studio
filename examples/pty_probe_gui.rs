//! Diagnostics for the engine-under-PTY launch slice. Spawns the engine from
//! a real GUI parent (eframe app with a live NSApplication/WindowServer
//! connection) through the same `PtySession` the runner uses.
//!
//! Two modes:
//! - default: healthy control — proves the PTY spawn path is sound under a
//!   GUI parent (engine should reach the main menu);
//! - `DEERPUNK_PTY_PROBE_PANIC=1`: fires a main-thread panic on the first
//!   engine output, reproducing the historic studio console-render panic and
//!   the resulting engine teardown (master close → SIGHUP to the session).

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use deerpunk_studio::pty::PtySession;

/// Set by the pump worker after the first engine output; the UI thread then
/// panics exactly like the studio's console renderer does.
static PANIC_NOW: AtomicBool = AtomicBool::new(false);

struct GuiProbe;

impl eframe::App for GuiProbe {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ui, |ui| {
            ui.label("pty probe gui — engine spawn in progress");
        });
        // Keep repainting at display rate like a live studio.
        ui.ctx().request_repaint();
        if force_panic() && PANIC_NOW.load(Ordering::SeqCst) {
            panic!("forced: simulate the studio console-render panic");
        }
    }
}

/// `DEERPUNK_PTY_PROBE_PANIC=1` enables the forced-panic reproduction.
fn force_panic() -> bool {
    std::env::var("DEERPUNK_PTY_PROBE_PANIC").is_ok_and(|v| v == "1")
}

fn spawn_engine() -> Option<PtySession> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir("/Users/theevolk/dev/gamedev/deerpunk")
        .args([
            "run",
            "--no-default-features",
            "--features",
            "full-kira,puffin",
            "--",
            "--gamedir",
            "/Users/theevolk/dev/gamedev/ohmyblock/gamedir",
            "--svlib",
            "/Users/theevolk/dev/gamedev/ohmyblock/gamedir/dlls/server/ohmyblock_amd64.dylib",
            "--cllib",
            "/Users/theevolk/dev/gamedev/ohmyblock/gamedir/dlls/client/client_amd64.dylib",
        ]);
    let engine_target = "/Users/theevolk/dev/gamedev/deerpunk/target/debug";
    let prepend = format!("/Users/theevolk/dev/gamedev/ohmyblock/gamedir/cl_dlls:{engine_target}:");
    let path_env = match std::env::var("PATH") {
        Ok(p) => format!("{prepend}{p}"),
        Err(_) => prepend.trim_end_matches(':').to_string(),
    };
    cmd.env("PATH", path_env);
    match PtySession::spawn(&mut cmd) {
        Ok(p) => Some(p),
        Err(e) => {
            println!("[spawn failed] {e}");
            None
        }
    }
}

fn pump_worker() {
    // Give the NSApplication/window time to fully come up, like the studio.
    std::thread::sleep(Duration::from_secs(2));
    println!(
        "[gui worker] spawning engine from GUI process (forced-panic mode: {})",
        force_panic()
    );
    let Some(mut pty) = spawn_engine() else {
        return;
    };
    let pid = pty.child.id();
    println!("[gui worker] engine pid={pid}");

    let start = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut buf = [0u8; 4096];
    let mut last_beat = Instant::now();
    let mut exited_status: Option<std::process::ExitStatus> = None;
    while Instant::now() < deadline {
        match pty.read(&mut buf) {
            Ok(0) => {
                println!("[gui worker] pty EOF");
                break;
            }
            Ok(n) => {
                use std::io::Write;
                let _ = std::io::stdout().write_all(&buf[..n]);
                let _ = std::io::stdout().flush();
                // First engine output arrived -> fire the UI panic like the
                // studio's console panel does on dim ANSI spans.
                if !PANIC_NOW.swap(true, Ordering::SeqCst) {
                    println!("[gui worker] engine output seen; UI panic armed");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                println!("[gui worker] read error: {e}");
                break;
            }
        }
        if let Some(status) = pty.child.try_wait().expect("try_wait") {
            exited_status = Some(status);
            break;
        }
        if last_beat.elapsed() >= Duration::from_secs(1) {
            last_beat = Instant::now();
            println!(
                "[gui worker] t+{:.1}s engine alive",
                start.elapsed().as_secs_f32()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(status) = exited_status {
        let status = pty.child.wait().unwrap_or(status);
        println!("[gui worker] ENGINE EXIT: {status}");
    } else {
        println!("[gui worker] probe window over; engine still alive -> killing");
    }
    let _ = pty.child.kill();
    let _ = pty.child.wait();
    std::process::exit(0);
}

fn main() {
    std::thread::Builder::new()
        .name("gui-pump".into())
        .spawn(pump_worker)
        .expect("spawn pump worker");

    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([420.0, 240.0]),
        ..Default::default()
    };
    eframe::run_native(
        "pty-probe-gui",
        opts,
        Box::new(|_cc| Ok(Box::new(GuiProbe))),
    )
    .expect("eframe run");
}
