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
    pub fn ensure_started(&mut self, inspector_addr: &str) {
        if self.is_running() {
            return;
        }
        match self.spawn(inspector_addr) {
            Ok(()) => self.status_err = None,
            Err(e) => self.status_err = Some(e),
        }
    }

    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_)) => {
                    self.child = None;
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
                let _ = tx.send(http_get_json("127.0.0.1", port, "/status"));
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
                }
                Err(e) => self.status_err = Some(format!("parse /status: {e}")),
            },
            Ok(Err(e)) => self.status_err = Some(e),
            Err(mpsc::TryRecvError::Empty) => self.status_rx = Some(rx), // still in flight
            Err(mpsc::TryRecvError::Disconnected) => {
                self.status_err = Some("status thread died".into())
            }
        }
    }
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
fn http_get_json(host: &str, port: u16, path: &str) -> Result<String, String> {
    let mut stream =
        TcpStream::connect((host, port)).map_err(|e| format!("connect {host}:{port}: {e}"))?;
    stream
        .set_read_timeout(Some(STATUS_TIMEOUT))
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
