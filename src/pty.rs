//! Child process attached to a terminal: a real PTY on macOS/Linux, pipes
//! plus a Job Object on Windows. The child gets console I/O either way —
//! the engine reads console commands line-wise from stdin and its output
//! streams back through the pump loop.
//!
//! Unix: the child is its own session (`setsid`) with the PTY slave as
//! controlling terminal, so SIGTERM/SIGKILL reach the whole process group;
//! terminal echo is disabled so typed input is not duplicated.
//! Windows: stdout/stderr are piped into an internal buffer (read() never
//! blocks), and the child is assigned to a Job Object so terminate/kill
//! take down the whole tree (grandchildren included). Note the engine's
//! stdout is not a TTY on this path — ANSI coloring is up to it.

#[cfg(unix)]
pub use unix_impl::PtySession;
#[cfg(windows)]
pub use windows_impl::PtySession;

#[cfg(unix)]
mod unix_impl {
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
            let mut ws = libc::winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let rc = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut ws,
                )
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
                        libc::ioctl(
                            0,
                            libc::TIOCSCTTY as libc::c_ulong,
                            std::ptr::null::<libc::c_void>(),
                        );
                        libc::close(master_fd);
                        Ok(())
                    });
                }
                cmd.spawn().map_err(|e| format!("spawn: {e}"))?
            };

            // Parent does not need the slave end.
            unsafe { libc::close(slave) };

            let pid = child.id();
            super::debug_log(&format!("spawn pid={pid} master={master}"));
            Ok(PtySession { child, master, pid })
        }

        /// Read whatever is pending on the master; `Ok(0)` = child closed its
        /// side (EOF), `Err` with `WouldBlock` = nothing right now.
        pub fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = unsafe { libc::read(self.master, buf.as_mut_ptr().cast(), buf.len()) };
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
                    libc::write(
                        self.master,
                        payload[off..].as_ptr().cast(),
                        payload.len() - off,
                    )
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
            let status = self.child.try_wait().ok().flatten();
            super::debug_log(&format!(
                "drop pid={} master={} child_status={status:?}",
                self.pid, self.master
            ));
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
}

#[cfg(windows)]
mod windows_impl {
    use std::io::{ErrorKind, Read, Write as _};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::{Arc, Mutex};

    pub struct PtySession {
        pub child: Child,
        stdin: ChildStdin,
        /// stdout + stderr, merged by reader threads; `read()` drains it.
        out: Arc<Mutex<Vec<u8>>>,
        job: isize,
        pid: u32,
    }

    impl PtySession {
        /// Spawn `cmd` with piped stdio and park it in a Job Object. No PTY
        /// on this platform — the pump contract is preserved through the
        /// internal buffer (read() returns `WouldBlock` while the child is
        /// alive and quiet, `Ok(0)` after it exits and drained).
        pub fn spawn(cmd: &mut Command) -> Result<PtySession, String> {
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
            let pid = child.id();
            let job = unsafe { job_for(pid) }.map_err(|e| format!("job object: {e}"))?;
            super::debug_log(&format!("spawn pid={pid} job={job:#x} windows"));

            let stdin = child.stdin.take().expect("stdin piped");
            let out: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
            // stdout and stderr are distinct pipe types — one reader thread each.
            for stream in [
                child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
                child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
            ] {
                let Some(mut stream) = stream else { continue };
                let sink = out.clone();
                std::thread::spawn(move || {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
                            Err(_) => break,
                        }
                    }
                });
            }

            Ok(PtySession {
                child,
                stdin,
                out,
                job,
                pid,
            })
        }

        /// Drain pending output; `Ok(0)` = child gone, `WouldBlock` = idle.
        pub fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut out = self.out.lock().unwrap();
            if out.is_empty() {
                return match self.child.try_wait() {
                    Ok(Some(_)) => Ok(0),
                    _ => Err(std::io::Error::new(ErrorKind::WouldBlock, "no output yet")),
                };
            }
            let n = buf.len().min(out.len());
            buf[..n].copy_from_slice(&out[..n]);
            out.drain(..n);
            Ok(n)
        }

        /// Send a console command line to the child's stdin.
        pub fn send_line(&mut self, line: &str) -> Result<(), String> {
            self.stdin
                .write_all(format!("{line}\n").as_bytes())
                .and_then(|_| self.stdin.flush())
                .map_err(|e| e.to_string())
        }

        /// No SIGTERM on Windows: both stops terminate the whole job tree.
        pub fn terminate(&mut self) {
            unsafe {
                terminate_job(self.job);
            }
        }

        pub fn kill(&mut self) {
            unsafe {
                terminate_job(self.job);
            }
        }
    }

    impl Drop for PtySession {
        fn drop(&mut self) {
            let status = self.child.try_wait().ok().flatten();
            super::debug_log(&format!("drop pid={} child_status={status:?}", self.pid));
            unsafe {
                terminate_job(self.job);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            unsafe {
                CloseHandle(self.job);
            }
        }
    }

    /// New Job Object with `pid` as its first (and only manually assigned)
    /// member. Processes the child spawns later join the job automatically,
    /// so `TerminateJobObject` reaches the whole tree.
    unsafe fn job_for(pid: u32) -> Result<isize, std::io::Error> {
        // SAFETY: plain kernel32 calls, handles closed on every path.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if job == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process == 0 {
                let e = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(e);
            }
            let ok = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            if ok == 0 {
                let e = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(e);
            }
            Ok(job)
        }
    }

    unsafe fn terminate_job(job: isize) {
        // SAFETY: handle from job_for, closed by the owner on drop.
        unsafe {
            TerminateJobObject(job, 1);
        }
    }

    const PROCESS_SET_QUOTA: u32 = 0x0100;
    const PROCESS_TERMINATE: u32 = 0x0001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(lpJobAttributes: *mut core::ffi::c_void, lpName: *const u16) -> isize;
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> isize;
        fn AssignProcessToJobObject(hJob: isize, hProcess: isize) -> i32;
        fn TerminateJobObject(hJob: isize, uExitCode: u32) -> i32;
        fn CloseHandle(hObject: isize) -> i32;
    }
}

/// Low-level PTY lifecycle diagnostics, enabled by `DEERPUNK_STUDIO_PTY_DEBUG`
/// (path to an append-only log). Bypasses the UI so events survive a dead GUI.
pub(crate) fn debug_log(msg: &str) {
    let Ok(path) = std::env::var("DEERPUNK_STUDIO_PTY_DEBUG") else {
        return;
    };
    use std::io::Write as _;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}
