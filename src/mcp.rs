//! Child-process management for the `deerpunk-mcp` server binary plus a tiny
//! blocking HTTP client for its `GET /status` endpoint (no extra deps: the
//! studio already hand-rolls its WS client the same way).

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde::Deserialize;

const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
const STATUS_MIN_INTERVAL: Duration = Duration::from_millis(1500);
/// Adopt-probe timeout: localhost refusal is instant, 250 ms is generous.
const ADOPT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
/// Cooldown between auto-respawn attempts after a failed spawn or dead child.
const RESPAWN_COOLDOWN: Duration = Duration::from_secs(10);
/// Consecutive failed `/status` polls before we stop trusting an adopted
/// external server and allow spawning our own child again.
const EXTERNAL_MAX_FAILURES: u32 = 4;

/// Subset of the binary's `GET /status` document (see crates/deerpunk-mcp).
#[derive(Debug, Clone, Deserialize)]
pub struct McpStatus {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub uptime_secs: f64,
    #[serde(default)]
    pub inspector: Option<InspectorState>,
    #[serde(default)]
    pub tools: Vec<McpTool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InspectorState {
    pub connected: bool,
    #[serde(default)]
    pub addr: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Owns the spawned `deerpunk-mcp` child and the last known `/status` reply.
pub struct McpManager {
    port: u16,
    child: Option<Child>,
    /// True when a valid `deerpunk-mcp` was already listening on the port
    /// (started outside this process); we adopt it instead of spawning.
    external: bool,
    /// Consecutive failed `/status` polls while `external` is set.
    external_failures: u32,
    /// Earliest instant at which the next auto-respawn attempt is allowed.
    next_spawn_at: Option<Instant>,
    status_rx: Option<Receiver<Result<String, String>>>,
    status_req_at: Option<Instant>,
    /// Last successful `/status` document.
    pub status: Option<McpStatus>,
    /// Last `/status` failure (spawn, HTTP, parse) for the modal.
    pub status_err: Option<String>,
}

impl McpManager {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            child: None,
            external: false,
            external_failures: 0,
            next_spawn_at: None,
            status_rx: None,
            status_req_at: None,
            status: None,
            status_err: None,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Spawn the server if it is not running yet (auto-start on app launch).
    /// A valid external `deerpunk-mcp` already on the port is adopted instead.
    pub fn ensure_started(&mut self, inspector_addr: &str) {
        if self.is_running() {
            return;
        }
        // Back off auto-respawn after a dead child / failed spawn so a port
        // held by garbage cannot turn into an endless spawn loop.
        if let Some(at) = self.next_spawn_at {
            if Instant::now() < at {
                return;
            }
        }
        // Adopt an externally started server if it answers /status.
        match probe_status(self.port) {
            Ok(status) => {
                self.external = true;
                self.external_failures = 0;
                self.status = Some(status);
                self.status_err = None;
                self.next_spawn_at = None;
                return;
            }
            Err(_) => {} // nothing valid to adopt; try to spawn
        }
        match self.spawn(inspector_addr) {
            Ok(()) => {
                self.status_err = None;
                self.next_spawn_at = None;
            }
            Err(e) => {
                self.status_err = Some(e);
                self.next_spawn_at = Some(Instant::now() + RESPAWN_COOLDOWN);
            }
        }
    }

    pub fn is_running(&mut self) -> bool {
        if self.external {
            return true;
        }
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_)) => {
                    self.child = None;
                    // The child died on its own (e.g. bind failure): back off.
                    self.next_spawn_at = Some(Instant::now() + RESPAWN_COOLDOWN);
                    false
                }
                Ok(None) => true,
                Err(_) => {
                    self.child = None;
                    false
                }
            },
            None => false,
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.external = false;
        self.external_failures = 0;
        self.next_spawn_at = None;
        self.status = None;
        self.status_err = None;
    }

    pub fn restart(&mut self, inspector_addr: &str) {
        self.stop();
        self.status_req_at = None;
        match self.spawn(inspector_addr) {
            Ok(()) => self.status_err = None,
            Err(e) => self.status_err = Some(e),
        }
    }

    fn spawn(&mut self, inspector_addr: &str) -> Result<(), String> {
        let exe = binary_path()?;
        let mut cmd = Command::new(&exe);
        cmd.args([
            "--port",
            &self.port.to_string(),
            "--inspector",
            inspector_addr,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
        let child = cmd
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", exe.display()))?;
        self.child = Some(child);
        Ok(())
    }

    /// Kick off a `/status` request if the previous one is done and old enough.
    pub fn request_status(&mut self) {
        if self.status_rx.is_some() {
            return; // request in flight
        }
        if let Some(at) = self.status_req_at {
            if at.elapsed() < STATUS_MIN_INTERVAL {
                return;
            }
        }
        let (tx, rx) = mpsc::channel();
        let port = self.port;
        self.status_req_at = Some(Instant::now());
        self.status_rx = Some(rx);
        std::thread::Builder::new()
            .name("mcp-status".into())
            .spawn(move || {
                let _ = tx.send(http_get_json("127.0.0.1", port, "/status", STATUS_TIMEOUT));
            })
            .ok();
    }

    /// Drain the pending `/status` reply, if any.
    pub fn poll(&mut self) {
        let Some(rx) = self.status_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(body)) => match serde_json::from_str::<McpStatus>(&body) {
                Ok(status) => {
                    self.status = Some(status);
                    self.status_err = None;
                    if self.external {
                        self.external_failures = 0;
                    }
                }
                Err(e) => self.note_status_failure(format!("parse /status: {e}")),
            },
            Ok(Err(e)) => self.note_status_failure(e),
            Err(mpsc::TryRecvError::Empty) => self.status_rx = Some(rx), // still in flight
            Err(mpsc::TryRecvError::Disconnected) => {
                self.note_status_failure("status thread died".into())
            }
        }
    }

    /// Record a failed `/status` poll; too many consecutive failures while an
    /// external server is adopted means it went away, so allow respawning.
    fn note_status_failure(&mut self, err: String) {
        self.status_err = Some(err);
        if self.external {
            self.external_failures += 1;
            if self.external_failures >= EXTERNAL_MAX_FAILURES {
                self.external = false;
                self.external_failures = 0;
            }
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        // Never orphan the child / the port when the studio exits.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// One-shot blocking `/status` probe used to adopt an external server.
fn probe_status(port: u16) -> Result<McpStatus, String> {
    let body = http_get_json("127.0.0.1", port, "/status", ADOPT_PROBE_TIMEOUT)?;
    serde_json::from_str::<McpStatus>(&body).map_err(|e| format!("parse /status: {e}"))
}

/// Resolve the server binary: env override first, then next to our own exe.
fn binary_path() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("DEERPUNK_STUDIO_MCP_BIN") {
        return Ok(path.into());
    }
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "no exe parent dir".to_string())?;
    Ok(dir.join("deerpunk-mcp"))
}

/// Minimal HTTP/1.0 GET returning the body (JSON text). HTTP/1.0 avoids
/// chunked transfer-encoding, so `read_to_end` sees the whole document.
fn http_get_json(host: &str, port: u16, path: &str, timeout: Duration) -> Result<String, String> {
    let mut stream =
        TcpStream::connect((host, port)).map_err(|e| format!("connect {host}:{port}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("read timeout: {e}"))?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = String::new();
    stream
        .read_to_string(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let body = buf
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or("")
        .to_string();
    if body.is_empty() {
        return Err(format!("empty /status body from {host}:{port}"));
    }
    Ok(body)
}

/// Primary LAN address without shelling out (UDP "connect" only routes).
pub fn local_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("9.9.9.9:80").ok()?;
    Some(sock.local_addr().ok()?.ip())
}
