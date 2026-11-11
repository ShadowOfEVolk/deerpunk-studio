//! `deerpunk-mcp` — MCP server proxying the running deerpunk engine inspector.
//!
//! Transports: streamable HTTP (`POST /mcp`, plus plain `GET /status` for the
//! studio modal) and optional `--stdio`. The inspector itself is reached over
//! WebSocket (JSON-RPC, `inspector.initialize` handshake) with reconnect.

mod dispatch;
mod inspector;
mod tools;

use std::net::SocketAddr;

use serde_json::json;
use serde_json::Value;

use dispatch::SharedInspector;
use inspector::Inspector;

const DEFAULT_PORT: u16 = 9100;
const DEFAULT_INSPECTOR: &str = "127.0.0.1:9000";
/// How often the anti-orphan watcher checks that the parent is still alive.
const PARENT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
static START: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

#[derive(Debug)]
struct Args {
    port: u16,
    inspector_addr: String,
    stdio: bool,
    /// Studio pid passed via `--parent-pid`; the process exits once it is gone.
    parent_pid: Option<u32>,
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Args, String> {
    let mut parsed = Args {
        port: DEFAULT_PORT,
        inspector_addr: DEFAULT_INSPECTOR.to_string(),
        stdio: false,
        parent_pid: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                parsed.port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_PORT);
            }
            "--inspector" => {
                if let Some(v) = args.next() {
                    parsed.inspector_addr = v;
                }
            }
            "--stdio" => parsed.stdio = true,
            "--parent-pid" => {
                parsed.parent_pid = args.next().and_then(|v| v.parse().ok());
            }
            other => {
                return Err(format!(
                    "unknown argument: {other}\nusage: deerpunk-mcp [--port N] [--inspector host:port] [--stdio] [--parent-pid pid]"
                ));
            }
        }
    }
    Ok(parsed)
}

/// True while `pid` is an existing process. `kill(pid, 0)` performs the
/// existence check without signalling; EPERM still means "exists" (someone
/// else's process).
#[allow(unsafe_code)] // libc::kill is a trivially safe-in-practice syscall probe
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill(2) with signal 0 only performs permission/existence checks.
    match unsafe { libc::kill(pid as libc::pid_t, 0) } {
        0 => true,
        _ => std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
    }
}

fn main() {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(usage) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    };
    let Args {
        port,
        inspector_addr,
        stdio,
        parent_pid,
    } = args;

    // Anti-orphan watch: the studio passes its own pid; when it goes away
    // (crash, kill -9 — anything a Drop in the parent cannot cover) we exit
    // instead of squatting on the port forever.
    if let Some(pid) = parent_pid {
        std::thread::Builder::new()
            .name("parent-watch".into())
            .spawn(move || loop {
                if !pid_alive(pid) {
                    std::process::exit(0);
                }
                std::thread::sleep(PARENT_POLL_INTERVAL);
            })
            .expect("spawn parent-watch thread");
    }

    let (host, port_i) = match inspector_addr.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(9000)),
        None => (inspector_addr.clone(), 9000),
    };
    let inspector = SharedInspector::new(Inspector::new(&host, port_i));

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async move {
        if stdio {
            run_stdio(inspector).await;
        } else {
            if let Err(e) = run_http(inspector, port).await {
                eprintln!("fatal: {e}");
                std::process::exit(1);
            }
        }
    });
}

async fn run_http(inspector: SharedInspector, port: u16) -> Result<(), String> {
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;

    let bind_addr = inspector.get().addr().to_string();
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::post(
                |State(mcp): State<SharedInspector>,
                 headers: HeaderMap,
                 axum::Json(body): axum::Json<Value>| async move {
                    // Best-effort session: echo the client's Mcp-Session-Id, mint one
                    // on initialize if absent. No server-side session state kept.
                    let session_header = headers
                        .get("mcp-session-id")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let mint = session_header.is_none()
                        && body.get("method").and_then(Value::as_str) == Some("initialize");
                    let session_id = session_header.unwrap_or_else(|| {
                        format!("{}-{}", START.elapsed().as_nanos(), std::process::id())
                    });

                    let resp = |r: axum::response::Response| {
                        Ok::<axum::response::Response, std::convert::Infallible>(r)
                    };
                    if let Some(resp_json) = dispatch::handle_request(&mcp, &body) {
                        let mut r = (StatusCode::OK, axum::Json(resp_json)).into_response();
                        if mint {
                            if let Ok(v) = axum::http::HeaderValue::from_str(&session_id) {
                                r.headers_mut().insert("Mcp-Session-Id", v);
                            }
                        }
                        resp(r)
                    } else {
                        // Accepted notification: no body per streamable-HTTP spec.
                        resp((StatusCode::ACCEPTED, "").into_response())
                    }
                },
            ),
        )
        .route(
            "/status",
            axum::routing::get(move |State(mcp): State<SharedInspector>| async move {
                axum::Json(status_json(mcp.get(), port))
            }),
        )
        .with_state(inspector);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    eprintln!("deerpunk-mcp listening on http://{addr}/mcp (inspector: {bind_addr})");
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

async fn run_stdio(inspector: SharedInspector) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stdin = BufReader::new(tokio::io::stdin());
    let mut out = tokio::io::stdout();
    let mut lines = stdin.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": "parse error"},
            });
            let _ = out.write_all(format!("{resp}\n").as_bytes()).await;
            continue;
        };
        if let Some(resp) = dispatch::handle_request(&inspector, &req) {
            let _ = out.write_all(format!("{resp}\n").as_bytes()).await;
        }
    }
}

fn status_json(mcp: &Inspector, port: u16) -> Value {
    let session = mcp.session();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "port": port,
        "uptime_secs": START.elapsed().as_secs(),
        "inspector": {
            "connected": mcp.is_connected(),
            "addr": mcp.addr(),
            "server": session.as_ref().map(|s| s.server.as_str()).unwrap_or(""),
            "server_version": session.as_ref().and_then(|s| s.server_version.clone()),
        },
        "tools": tools::tool_defs().iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(items: &[&str]) -> Result<Args, String> {
        parse_args(items.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_when_no_args() {
        let a = parse(&[]).unwrap();
        assert_eq!(a.port, DEFAULT_PORT);
        assert_eq!(a.inspector_addr, DEFAULT_INSPECTOR);
        assert!(!a.stdio);
        assert_eq!(a.parent_pid, None);
    }

    #[test]
    fn parses_all_flags() {
        let a = parse(&["--port", "1234", "--inspector", "10.0.0.5:7000", "--stdio", "--parent-pid", "4242"]).unwrap();
        assert_eq!(a.port, 1234);
        assert_eq!(a.inspector_addr, "10.0.0.5:7000");
        assert!(a.stdio);
        assert_eq!(a.parent_pid, Some(4242));
    }

    #[test]
    fn unknown_argument_is_usage_error() {
        let err = parse(&["--bogus"]).unwrap_err();
        assert!(err.contains("usage"));
    }

    #[test]
    fn self_pid_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn dead_pid_is_not_alive() {
        // No sane pid table hands out 999999999 (> 32-bit pid_t range on
        // darwin/linux), so this pid cannot exist.
        assert!(!pid_alive(999_999_999));
    }
}
