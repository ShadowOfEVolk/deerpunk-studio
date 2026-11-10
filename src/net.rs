//! WebSocket connection to the engine inspector server
//! (`deerpunk-inspector-protocol`, JSON-RPC over WS, default `127.0.0.1:9000`).
//!
//! One background thread owns the socket; the UI polls via channels.
//! Commands sent while connecting are queued and flushed on connect.

use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use tungstenite::{Message, WebSocket};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const POLL_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected,
    /// Connect or transport failure with a human reason.
    Error(String),
}

pub enum NetCmd {
    Connect { host: String, port: u16 },
    Disconnect,
    Send(String),
}

pub enum NetEvent {
    State(ConnState),
    Line(String),
}

pub struct NetClient {
    tx: Sender<NetCmd>,
    rx: Receiver<NetEvent>,
}

impl NetClient {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("inspector-ws".into())
            .spawn(move || run(cmd_rx, event_tx))
            .expect("spawn inspector-ws thread");
        Self { tx: cmd_tx, rx: event_rx }
    }

    pub fn cmd(&self, cmd: NetCmd) {
        let _ = self.tx.send(cmd);
    }

    /// Drain all pending events; non-blocking.
    pub fn poll(&self) -> Vec<NetEvent> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(ev) => out.push(ev),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}

enum Loop {
    Idle,
    Live(WebSocket<TcpStream>),
}

fn run(cmd_rx: Receiver<NetCmd>, event_tx: Sender<NetEvent>) {
    let mut state = Loop::Idle;
    let mut queued: Vec<String> = Vec::new();

    loop {
        // Command handling (idle: block on cmd, live: drain non-blocking).
        let cmds: Vec<NetCmd> = match &state {
            Loop::Idle => match cmd_rx.recv() {
                Ok(cmd) => vec![cmd],
                Err(_) => return,
            },
            Loop::Live(_) => {
                let mut out = Vec::new();
                while let Ok(cmd) = cmd_rx.try_recv() {
                    out.push(cmd);
                }
                out
            }
        };

        let mut went_down = false;
        for cmd in cmds {
            match cmd {
                NetCmd::Connect { host, port } => {
                    // Restart connection cycle if already live.
                    if let Loop::Live(mut ws) = state {
                        let _ = ws.close(None);
                        state = Loop::Idle;
                    }
                    let _ = event_tx.send(NetEvent::State(ConnState::Connecting));
                    match connect(&host, port) {
                        Ok(ws) => {
                            state = Loop::Live(ws);
                            let _ = event_tx.send(NetEvent::State(ConnState::Connected));
                            for line in std::mem::take(&mut queued) {
                                if let Loop::Live(ws) = &mut state {
                                    if push_text(ws, &line).is_err() {
                                        state = Loop::Idle;
                                        let _ = event_tx.send(NetEvent::State(ConnState::Disconnected));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = event_tx.send(NetEvent::State(ConnState::Error(e)));
                        }
                    }
                }
                NetCmd::Disconnect => {
                    if let Loop::Live(mut ws) = state {
                        let _ = ws.close(None);
                        state = Loop::Idle;
                    }
                    let _ = event_tx.send(NetEvent::State(ConnState::Disconnected));
                }
                NetCmd::Send(line) => match &mut state {
                    Loop::Live(ws) => {
                        if push_text(ws, &line).is_err() {
                            went_down = true;
                        }
                    }
                    Loop::Idle => queued.push(line),
                },
            }
        }
        if went_down {
            if let Loop::Live(mut ws) = state {
                let _ = ws.close(None);
                state = Loop::Idle;
            }
            let _ = event_tx.send(NetEvent::State(ConnState::Disconnected));
        }

        // Read pass.
        match &mut state {
            Loop::Live(ws) => loop {
                match ws.read() {
                    Ok(Message::Text(t)) => {
                        let _ = event_tx.send(NetEvent::Line(t.as_str().to_string()));
                    }
                    Ok(Message::Ping(p)) => {
                        let _ = ws.send(Message::Pong(p));
                    }
                    Ok(Message::Close(_)) | Ok(_) => {
                        state = Loop::Idle;
                        let _ = event_tx.send(NetEvent::State(ConnState::Disconnected));
                        break;
                    }
                    Err(tungstenite::Error::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        break;
                    }
                    Err(_) => {
                        state = Loop::Idle;
                        let _ = event_tx.send(NetEvent::State(ConnState::Disconnected));
                        break;
                    }
                }
            },
            Loop::Idle => {}
        }
    }
}

fn connect(host: &str, port: u16) -> Result<WebSocket<TcpStream>, String> {
    use std::net::ToSocketAddrs;
    let addr = format!("{host}:{port}");
    let sock: std::net::SocketAddr = addr
        .to_socket_addrs()
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {addr}"))?;
    let stream = TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT)
        .map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_nodelay(true)
        .map_err(|e| format!("nodelay: {e}"))?;
    stream
        .set_read_timeout(Some(POLL_TIMEOUT))
        .map_err(|e| format!("read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(POLL_TIMEOUT))
        .map_err(|e| format!("write timeout: {e}"))?;
    let url = format!("ws://{addr}");
    let (ws, _resp) =
        tungstenite::client::client(url.as_str(), stream).map_err(|e| format!("handshake: {e}"))?;
    Ok(ws)
}

fn push_text(ws: &mut WebSocket<TcpStream>, text: &str) -> Result<(), String> {
    ws.send(Message::text(text)).map_err(|e| e.to_string())?;
    ws.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_polls_without_server() {
        let client = NetClient::new();
        assert!(client.poll().is_empty());
        // Connecting to a dead port yields an error state event.
        client.cmd(NetCmd::Connect { host: "127.0.0.1".into(), port: 1 });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let events = client.poll();
            if events
                .iter()
                .any(|ev| matches!(ev, NetEvent::State(ConnState::Error(_))))
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no error state in time");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
