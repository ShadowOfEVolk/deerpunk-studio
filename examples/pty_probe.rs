//! Throwaway diagnostics: spawn the engine exactly like the studio runner
//! does and dump raw PTY output. Delete after debugging.

use std::process::Command;
use std::time::{Duration, Instant};

fn main() {
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

    let mut pty = match deerpunk_studio::pty::PtySession::spawn(&mut cmd) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[spawn failed] {e}");
            return;
        }
    };

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match pty.read(&mut buf) {
            Ok(0) => {
                println!("[EOF]");
                break;
            }
            Ok(n) => {
                use std::io::Write;
                std::io::stdout().write_all(&buf[..n]).unwrap();
                std::io::stdout().flush().unwrap();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                println!("[read error: {e}]");
                break;
            }
        }
        if pty.child.try_wait().map(|s| s.is_some()).unwrap_or(false) {
            let status = pty.child.wait().unwrap();
            println!("[child exited: {status}]");
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("[probe end]");
    let _ = pty.child.kill();
    let _ = pty.child.wait();
}
