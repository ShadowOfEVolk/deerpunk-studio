//! PTY-backed child process (macOS/Linux). The child gets a real terminal —
//! the engine logger colors its output on a TTY and stdin works for console
//! commands. Terminal echo is disabled so typed input is not duplicated.


use std::os::unix::io::RawFd;
use std::process::{Child, Command, Stdio};

pub struct PtySession {
    pub child: Child,
    master: RawFd,
    pid: u32,
}

impl PtySession {
    /// Spawn `cmd` attached to a fresh PTY in raw mode (no echo, no line
    /// buffering). `cmd` should already have args/cwd/env set; stdio is
    /// replaced by the PTY slave in `pre_exec`.
    pub fn spawn(cmd: &mut Command) -> Result<PtySession, String> {

        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let mut ws = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
        let rc = unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws)
        };
        if rc != 0 {
            return Err(format!("openpty: {}", std::io::Error::last_os_error()));
        }

        // Raw terminal on the slave: no echo, no canonical buffer — the
        // engine reads console commands line-wise from stdin.
        unsafe {
            let mut tios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(slave, &mut tios) != 0 {
                let e = std::io::Error::last_os_error();
                close_pair(master, slave);
                return Err(format!("tcgetattr: {e}"));
            }
            libc::cfmakeraw(&mut tios);
            if libc::tcsetattr(slave, libc::TCSANOW, &tios) != 0 {
                let e = std::io::Error::last_os_error();
                close_pair(master, slave);
                return Err(format!("tcsetattr: {e}"));
            }
        }

        let master_fd = master;
        let slave_fd = slave;
        unsafe {
            // Non-blocking reads from the master (pump loop).
            let flags = libc::fcntl(master, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        let child = {
            use std::os::unix::process::CommandExt;
            cmd.stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            unsafe {
                // After fork, before exec: detach into a new session, wire the
                // PTY slave to stdio, make it the controlling terminal.
                cmd.pre_exec(move || {
                    libc::setsid();
                    libc::dup2(slave_fd, 0);
                    libc::dup2(slave_fd, 1);
                    libc::dup2(slave_fd, 2);
                    if slave_fd > 2 {
                        libc::close(slave_fd);
                    }
                    libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, std::ptr::null::<libc::c_void>());
                    libc::close(master_fd);
                    Ok(())
                });
            }
            cmd.spawn().map_err(|e| format!("spawn: {e}"))?
        };

        // Parent does not need the slave end.
        unsafe { libc::close(slave) };

        let pid = child.id();
        Ok(PtySession { child, master, pid })
    }

    /// Read whatever is pending on the master; `Ok(0)` = child closed its
    /// side (EOF), `Err` with `WouldBlock` = nothing right now.
    pub fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = unsafe {
            libc::read(self.master, buf.as_mut_ptr().cast(), buf.len())
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Send a console command line to the child's stdin.
    pub fn send_line(&mut self, line: &str) -> Result<(), String> {
        let payload = format!("{line}\n").into_bytes();
        let mut off = 0;
        while off < payload.len() {
            let n = unsafe {
                libc::write(self.master, payload[off..].as_ptr().cast(), payload.len() - off)
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            off += n as usize;
        }
        Ok(())
    }

    /// SIGTERM to the whole process group; escalate to SIGKILL later if needed.
    pub fn terminate(&mut self) {
        unsafe {
            libc::killpg(self.pid as libc::pid_t, libc::SIGTERM);
        }
    }

    pub fn kill(&mut self) {
        unsafe {
            libc::killpg(self.pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        unsafe { libc::close(self.master) };
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn close_pair(master: RawFd, slave: RawFd) {
    unsafe {
        libc::close(master);
        libc::close(slave);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawns_echo_via_pty() {
        let mut cmd = Command::new("/bin/cat");
        let mut pty = PtySession::spawn(&mut cmd).expect("spawn cat");
        pty.send_line("ping-pty").unwrap();
        // cat echoes the line back through the PTY.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut buf = [0u8; 256];
        let mut got = String::new();
        while std::time::Instant::now() < deadline {
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            if got.contains("ping-pty") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(got.contains("ping-pty"), "no echo, got: {got:?}");
        // Echo must be disabled: exactly one occurrence, not two.
        assert_eq!(got.matches("ping-pty").count(), 1);
        pty.terminate();
        let _ = pty.child.wait();
    }
}
