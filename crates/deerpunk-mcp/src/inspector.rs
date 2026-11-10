//! WebSocket client for the deerpunk engine inspector
//! (`deerpunk-inspector-protocol`: JSON-RPC 2.0 over WS, default `127.0.0.1:9000`).
//!
//! The MCP server is a *client* of the inspector. One [`Inspector`] owns the
//! socket behind a mutex; each `call` lazily (re)connects with a short
//! backoff after a failure. All RPC shapes mirror `deerpunk-studio/src/protocol.rs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use tungstenite::client::IntoClientRequest;
use tungstenite::Message;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
pub const PROTO_VERSION: i64 = 1;

/// Live inspector session data from the `inspector.initialize` handshake.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub server: String,
    pub server_version: Option<String>,
    pub collections: Value,
}

type Ws = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

enum State {
    Down,
    Live { ws: Ws, session: SessionInfo },
}

pub struct Inspector {
    addr: String,
    state: Mutex<State>,
    next_id: AtomicU64,
    last_failure: Mutex<Option<std::time::Instant>>,
}

impl Inspector {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            addr: format!("{host}:{port}"),
            state: Mutex::new(State::Down),
            next_id: AtomicU64::new(1),
            last_failure: Mutex::new(None),
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Connected and handshaken?
    pub fn is_connected(&self) -> bool {
        matches!(&*self.lock(), State::Live { .. })
    }

    /// Handshake data from the last successful `inspector.initialize`.
    pub fn session(&self) -> Option<SessionInfo> {
        match &*self.lock() {
            State::Live { session, .. } => Some(session.clone()),
            State::Down => None,
        }
    }

    /// Send a JSON-RPC request and wait for the matching reply's `result`.
    /// On transport failure the socket is dropped and one reconnect attempt
    /// is made before surfacing the canonical "engine not running" error.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        match self.attempt(method, params.clone()) {
            Ok(v) => Ok(v),
            Err(e) => {
                self.drop_socket();
                // Respect a small backoff since the just-failed attempt.
                if let Some(t) = *self.last_failure.lock().unwrap() {
                    let since = t.elapsed();
                    if since < RECONNECT_BACKOFF {
                        std::thread::sleep(RECONNECT_BACKOFF - since);
                    }
                }
                self.attempt(method, params)
                    .map_err(|_| format!("engine not running ({e})"))
            }
        }
    }

    fn attempt(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut guard = self.lock();
        if matches!(&*guard, State::Down) {
            *guard = State::Live {
                ws: connect(&self.addr)?,
                session: SessionInfo {
                    server: String::new(),
                    server_version: None,
                    collections: Value::Null,
                },
            };
            // Handshake; failure drops back to Down.
            if let Err(e) = self.handshake(&mut guard) {
                *guard = State::Down;
                *self.last_failure.lock().unwrap() = Some(std::time::Instant::now());
                return Err(e);
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let State::Live { ws, .. } = &mut *guard else {
            unreachable!("just ensured Live")
        };
        ws.send(Message::text(req.to_string()))
            .map_err(|e| format!("send: {e}"))?;
        let reply = read_reply(ws, id)?;
        *self.last_failure.lock().unwrap() = None;
        if let Some(err) = reply.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("rpc error");
            return Err(format!("inspector rpc error: {msg}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// `inspector.initialize` + `inspector.initialized`, storing session info.
    fn handshake(&self, guard: &mut MutexGuard<'_, State>) -> Result<(), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let State::Live { ws, session } = &mut **guard else {
            unreachable!("called with Live state")
        };
        ws.send(Message::text(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "inspector.initialize",
                "params": {"proto": PROTO_VERSION},
            })
            .to_string(),
        ))
        .map_err(|e| format!("handshake send: {e}"))?;
        let reply = read_reply(ws, id)?;
        if let Some(err) = reply.get("error") {
            return Err(format!(
                "handshake rejected: {}",
                err.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
            ));
        }
        let result = reply.get("result").cloned().unwrap_or(Value::Null);
        session.server = result
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        session.server_version = result
            .get("serverVersion")
            .and_then(Value::as_str)
            .map(str::to_string);
        session.collections = result.get("collections").cloned().unwrap_or(Value::Null);
        let _ = ws.send(Message::text(
            json!({"jsonrpc": "2.0", "method": "inspector.initialized", "params": {}}).to_string(),
        ));
        Ok(())
    }

    fn drop_socket(&self) {
        *self.lock() = State::Down;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Read messages until the reply for `id` arrives (skips notifications).
fn read_reply(ws: &mut Ws, id: u64) -> Result<Value, String> {
    let deadline = std::time::Instant::now() + READ_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("timeout waiting for inspector reply".into());
        }
        let tungstenite::stream::MaybeTlsStream::Plain(sock) = ws.get_ref() else {
            return Err("tls inspector not supported".into());
        };
        sock.set_read_timeout(Some(remaining.min(READ_TIMEOUT)))
            .map_err(|e| format!("set timeout: {e}"))?;
        match ws.read() {
            Ok(Message::Text(t)) => {
                let msg: Value = serde_json::from_str(&t).map_err(|e| format!("json: {e}"))?;
                if msg.get("id").and_then(Value::as_u64) == Some(id) {
                    return Ok(msg);
                }
                // Notifications and other replies are ignored.
            }
            Ok(Message::Ping(p)) => {
                let _ = ws.send(Message::Pong(p));
            }
            Ok(Message::Close(_)) => return Err("inspector closed connection".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                continue
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
}

fn connect(addr: &str) -> Result<Ws, String> {
    use std::net::ToSocketAddrs;
    let sock: std::net::SocketAddr = addr
        .to_socket_addrs()
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {addr}"))?;
    let stream = std::net::TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT)
        .map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_nodelay(true)
        .map_err(|e| format!("nodelay: {e}"))?;
    let request = format!("ws://{addr}")
        .into_client_request()
        .map_err(|e| format!("ws request for {addr}: {e}"))?;
    let (ws, _resp) =
        tungstenite::client(request, tungstenite::stream::MaybeTlsStream::Plain(stream))
            .map_err(|e| format!("ws handshake {addr}: {e}"))?;
    Ok(ws)
}
