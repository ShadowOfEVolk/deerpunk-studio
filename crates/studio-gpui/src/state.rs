//! Toolkit-agnostic studio state: an `Entity<StudioState>` carrying what the
//! former egui `StudioApp` held (minus its egui widgets), plus `spawn_pump` —
//! the background task that drains scan/net/runner channels and drives the
//! timers that egui ran from its per-frame `update()`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::{App, Entity};
use serde_json::Value;

use deerpunk_studio::ansi::Stream;
use deerpunk_studio::assets::{self, ScanResult};
use deerpunk_studio::config::StudioConfig;
use deerpunk_studio::gameinfo::GameInfo;
use deerpunk_studio::mcp::McpManager;
use deerpunk_studio::net::{ConnState, NetClient, NetCmd, NetEvent};
use deerpunk_studio::profiles::Profiles;
use deerpunk_studio::project::Project;
use deerpunk_studio::protocol;
use deerpunk_studio::runner::{self, RunState, Runner, Step, StepState};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Pump cadence: egui polled every frame (~60 Hz); a background pump at
/// ~20 Hz is plenty for console bytes, socket events and timers.
const PUMP_INTERVAL: Duration = Duration::from_millis(50);
const CONSOLE_CAPACITY: usize = 5000;

/// Test scenario: extra engine CLI args, lighter than a launch profile.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub id: String,
    pub label: String,
    /// Extra engine args, e.g. ["--inspector-port", "9000"].
    pub args: Vec<String>,
}

/// Selection in a collection: which row, plus the id string for matching.
pub struct Selection {
    pub col: String,
    pub index_str: String,
}

pub struct StudioState {
    pub cfg: StudioConfig,
    pub project: Option<Project>,
    pub gameinfo: Option<GameInfo>,
    pub scan: Option<ScanResult>,
    pub scan_rx: Option<std::sync::mpsc::Receiver<ScanResult>>,
    pub error: Option<String>,

    // Connection state.
    pub net: NetClient,
    pub conn_state: ConnState,
    pub want_connected: bool,
    pub reconnect_at: Option<Instant>,
    pub host: String,
    pub port: String,
    pub refresh_ms: u64,
    pub last_refresh: Instant,
    pub next_rpc_id: u64,
    pub pending: HashMap<u64, String>,
    pub collections: Vec<protocol::CollectionDescriptor>,
    pub lists: HashMap<String, Value>,
    pub last_fetch: HashMap<String, Instant>,
    pub selection: Option<Selection>,
    pub detail: Option<Value>,
    pub prefab_map: HashMap<u32, String>,
    pub inspector_built: bool,

    // Run instance state.
    pub profiles: Profiles,
    pub active_profile: String,
    /// Launch scenarios (engine arg sets) — `.deerpunk/scenarios.toml`. The
    /// egui app kept them on `StudioApp`; here the shared state owns them so
    /// [`StudioState::run_instance`] can pass the active scenario's args to
    /// the engine.
    pub scenarios: Vec<Scenario>,
    pub active_scenario: String,
    pub runner: Runner,
    pub run_state: RunState,
    pub steps: Vec<Step>,
    pub console: Stream,
    pub input: String,

    // MCP server (deerpunk-mcp child process).
    pub mcp: McpManager,

    /// Id of the collection whose list/detail is refreshed (gpui analog of
    /// the egui `View::Live(id)` active view).
    pub active_collection: Option<String>,
}

impl StudioState {
    pub fn new() -> Self {
        let cfg = StudioConfig::load();
        let mut state = Self {
            cfg,
            project: None,
            gameinfo: None,
            scan: None,
            scan_rx: None,
            error: None,
            net: NetClient::new(),
            conn_state: ConnState::Disconnected,
            want_connected: false,
            reconnect_at: None,
            host: "127.0.0.1".into(),
            port: "9000".into(),
            refresh_ms: 250,
            last_refresh: Instant::now(),
            next_rpc_id: 1,
            pending: HashMap::new(),
            collections: Vec::new(),
            lists: HashMap::new(),
            last_fetch: HashMap::new(),
            prefab_map: HashMap::new(),
            selection: None,
            detail: None,
            inspector_built: false,
            profiles: Profiles::default(),
            active_profile: "dev".into(),
            scenarios: vec![Scenario {
                id: "default".into(),
                label: "Default".into(),
                args: Vec::new(),
            }],
            active_scenario: "default".into(),
            runner: Runner::new(),
            run_state: RunState::Idle,
            steps: Vec::new(),
            console: Stream::new(CONSOLE_CAPACITY),
            input: String::new(),
            mcp: McpManager::new(9100),
            active_collection: None,
        };
        // Launched inside a game folder with no args: open it; otherwise
        // fall back to the last project.
        let cwd = std::env::current_dir().unwrap_or_default();
        if cwd.join("deerpunk.toml").is_file() {
            state.open_project(cwd);
        } else if let Some(last) = state.cfg.last_project.clone() {
            state.open_project(PathBuf::from(last));
        }
        // Inspector auto-connect even without a project.
        if !state.want_connected {
            state.connect();
        }
        state
    }

    /// Open a game project folder (must contain `deerpunk.toml`) and
    /// auto-connect to its inspector server.
    pub fn open_project(&mut self, root: PathBuf) {
        let root = absolute(&root);
        match Project::load(&root) {
            Ok(project) => {
                self.error = None;
                self.gameinfo = Some(GameInfo::from_gamedir(&project.gamedir));
                self.scan = None;
                let gamedir = project.gamedir.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                self.scan_rx = Some(rx);
                std::thread::Builder::new()
                    .name("asset-scan".into())
                    .spawn(move || {
                        let _ = tx.send(assets::scan_gamedir(&gamedir));
                    })
                    .ok();
                self.cfg.record_open(&root.to_string_lossy());
                self.cfg.save();
                self.profiles = Profiles::load(&root);
                self.active_profile = self
                    .profiles
                    .order
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "dev".into());
                self.scenarios = load_scenarios(&root);
                self.active_scenario = self
                    .scenarios
                    .first()
                    .map(|s| s.id.clone())
                    .unwrap_or_else(|| "default".into());
                self.console = Stream::new(CONSOLE_CAPACITY);
                self.steps.clear();
                self.project = Some(project);
                self.connect();
            }
            Err(e) => {
                self.error = Some(format!("{}: {e}", root.display()));
            }
        }
    }

    /// Launch the pre_run pipeline + engine instance with the active
    /// profile and the active scenario's extra engine args — the gpui port
    /// of the egui `StudioApp::run_instance` (src/app.rs).
    ///
    /// TopBar currently runs its own private copy (crates/studio-gpui/src/
    /// topbar.rs `run_instance`) which passes `extra_args: Vec::new()`; the
    /// integration pass should switch it to this method so the active
    /// scenario's args reach the engine.
    pub fn run_instance(&mut self) {
        if self.run_state != RunState::Idle {
            return;
        }
        let Some(project) = self.project.as_ref() else {
            return;
        };
        // Copy the plan fields out of the borrow: the console/steps resets
        // below mutate `self`.
        let project_root = project.root.clone();
        let gamedir = project.gamedir.clone();
        let engine_dir = project.engine_dir.clone();
        let gamelibs = project.gamelibs.clone();
        let Some(profile) = self.profiles.get(&self.active_profile).cloned() else {
            self.error = Some(format!("no run profile `{}`", self.active_profile));
            return;
        };
        self.console = Stream::new(CONSOLE_CAPACITY);
        self.steps = runner::STEP_NAMES
            .iter()
            .map(|n| Step {
                name: (*n).to_string(),
                state: StepState::Pending,
            })
            .collect();
        let scenario_args = self
            .scenarios
            .iter()
            .find(|s| s.id == self.active_scenario)
            .map(|s| s.args.clone())
            .unwrap_or_default();
        self.runner.start(runner::InstancePlan {
            project_root,
            gamedir,
            engine_dir,
            profile,
            gamelibs,
            extra_args: scenario_args,
        });
    }

    pub fn connect(&mut self) {
        let host = self.host.trim().to_string();
        let Ok(port) = self.port.trim().parse::<u16>() else {
            self.error = Some(format!("bad port: {:?}", self.port));
            return;
        };
        self.error = None;
        self.want_connected = true;
        self.reconnect_at = None;
        self.pending.clear();
        self.collections.clear();
        self.lists.clear();
        self.last_fetch.clear();
        self.prefab_map.clear();
        self.inspector_built = false;
        self.net.cmd(NetCmd::Connect { host, port });
    }

    pub fn disconnect(&mut self) {
        self.want_connected = false;
        self.reconnect_at = None;
        self.net.cmd(NetCmd::Disconnect);
    }

    fn reconnect(&mut self) {
        self.want_connected = true;
        self.reconnect_at = None;
        self.net.cmd(NetCmd::Disconnect);
        let host = self.host.trim().to_string();
        if let Ok(port) = self.port.trim().parse::<u16>() {
            self.net.cmd(NetCmd::Connect { host, port });
        }
    }

    fn inspector_addr(&self) -> String {
        format!("{}:{}", self.host.trim(), self.port.trim())
    }

    fn rpc(&mut self, method: &str, params: Value) {
        let id = self.next_rpc_id;
        self.next_rpc_id += 1;
        self.pending.insert(id, method.to_string());
        self.net
            .cmd(NetCmd::Send(protocol::request(id, method, params)));
    }

    fn ensure_prefab_map(&mut self) {
        if self.conn_state != ConnState::Connected || !self.has_entity_collections() {
            return;
        }
        let stale = match self.last_fetch.get("prefabs") {
            None => true,
            Some(t) => self.refresh_ms > 0 && t.elapsed() >= Duration::from_millis(self.refresh_ms),
        };
        if stale {
            self.rpc("prefabs.list", serde_json::json!({}));
            self.last_fetch
                .insert("prefabs".to_string(), Instant::now());
        }
    }

    fn has_entity_collections(&self) -> bool {
        self.collections
            .iter()
            .any(|c| c.id == "serverentities" || c.id == "cliententities")
    }

    /// Fetch the active collection list (if list-mode) and selection detail.
    pub fn request_refresh(&mut self) {
        if self.conn_state != ConnState::Connected {
            return;
        }
        self.last_refresh = Instant::now();
        self.ensure_prefab_map();
        let Some(id) = self.active_collection.clone() else {
            return;
        };
        if self
            .collections
            .iter()
            .any(|c| c.id == id && c.modes.contains(&protocol::CollectionMode::List))
        {
            self.rpc(&format!("{id}.list"), serde_json::json!({}));
            self.last_fetch.insert(id.clone(), Instant::now());
        }
        if let Some(sel) = &self.selection {
            if sel.col == id {
                let method = format!("{}.get", sel.col);
                let index: Option<u64> = sel.index_str.parse().ok();
                if let Some(index) = index {
                    self.rpc(&method, serde_json::json!({ "index": index }));
                }
            }
        }
    }

    fn ensure_active_list(&mut self) {
        if self.conn_state != ConnState::Connected {
            return;
        }
        let Some(id) = self.active_collection.clone() else {
            return;
        };
        let stale = match self.last_fetch.get(&id) {
            None => true,
            Some(t) => self.refresh_ms > 0 && t.elapsed() >= Duration::from_millis(self.refresh_ms),
        };
        if stale {
            self.request_refresh();
        }
    }

    /// Select a row: request the detail snapshot.
    pub fn select(&mut self, col: String, index_str: String) {
        self.selection = Some(Selection { col, index_str });
        self.request_refresh();
    }

    fn console_lines_push(&mut self, text: String) {
        self.console.feed(format!("{text}\n").as_bytes());
        self.dump_console(text.as_bytes());
    }

    /// `DEERPUNK_STUDIO_CONSOLE=<path>`: append raw console bytes to a file.
    fn dump_console(&self, bytes: &[u8]) {
        static CONSOLE_DUMP: std::sync::LazyLock<Option<String>> =
            std::sync::LazyLock::new(|| std::env::var("DEERPUNK_STUDIO_CONSOLE").ok());
        if let Some(path) = CONSOLE_DUMP.as_ref() {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write;
                let _ = f.write_all(bytes);
            }
        }
    }

    fn poll_scan(&mut self) {
        if let Some(rx) = &self.scan_rx {
            match rx.try_recv() {
                Ok(res) => {
                    self.scan = Some(res);
                    self.scan_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.scan_rx = None;
                }
            }
        }
    }

    fn poll_runner(&mut self) {
        for ev in self.runner.poll() {
            self.apply_run_event(ev);
        }
    }

    /// Apply one runner event — the gpui analog of the egui `poll_runner`
    /// match (same order, same per-event handling), with one fix: the
    /// crash paths in src/runner.rs (build failure, engine exit != 0) park
    /// the runner at `Failed` **without** a trailing `Idle` — unlike the
    /// Stop/exit-0 paths, which send one. The idle runner thread ignores
    /// `Cmd::Stop`, so a plain assignment would leave `run_state == Failed`
    /// forever: the launch button then sits on the stop branch (a no-op)
    /// and can never relaunch. egui ground truth treats `Failed` as
    /// transient: surface the reason in the console (the step strip keeps
    /// the per-step ✗ detail) and land in `Idle` so the button returns to
    /// Play.
    fn apply_run_event(&mut self, ev: runner::Event) {
        match ev {
            runner::Event::Console(bytes) => {
                self.console.feed(&bytes);
                self.dump_console(&bytes);
            }
            runner::Event::State(RunState::Failed(err)) => {
                self.console_lines_push(format!("[studio] run failed: {err}"));
                self.run_state = RunState::Idle;
            }
            runner::Event::State(state) => self.run_state = state,
            runner::Event::Step(idx, state) => {
                if let Some(step) = self.steps.get_mut(idx) {
                    step.state = state;
                }
            }
            runner::Event::Note(text) => self.console_lines_push(text),
            runner::Event::Exited(_) => {}
        }
    }

    fn poll_net(&mut self) {
        let events = self.net.poll();
        let mut lines: Vec<String> = Vec::new();
        for ev in events {
            match ev {
                NetEvent::State(state) => {
                    if matches!(state, ConnState::Disconnected | ConnState::Error(_))
                        && self.want_connected
                    {
                        self.reconnect_at = Some(Instant::now() + RECONNECT_DELAY);
                    }
                    if state == ConnState::Connected {
                        self.rpc(
                            protocol::RPC_INITIALIZE,
                            serde_json::json!({
                                "proto": protocol::PROTO_VERSION,
                                "client": "deerpunk-studio",
                            }),
                        );
                    }
                    self.conn_state = state;
                }
                NetEvent::Line(text) => {
                    lines.push(text);
                }
            }
        }
        for line in lines {
            self.handle_line(&line);
        }

        // Timers.
        if self.want_connected
            && matches!(
                self.conn_state,
                ConnState::Disconnected | ConnState::Error(_)
            )
            && self.reconnect_at.is_some_and(|at| Instant::now() >= at)
        {
            self.reconnect();
        }
        if self.conn_state == ConnState::Connected
            && self.refresh_ms > 0
            && self.last_refresh.elapsed() >= Duration::from_millis(self.refresh_ms)
        {
            self.request_refresh();
        }
    }

    fn handle_line(&mut self, line: &str) {
        let Some(incoming) = protocol::parse_incoming(line) else {
            return;
        };
        match incoming {
            protocol::Incoming::RpcError { id, code, message } => {
                self.console_lines_push(format!("[rpc error {code}] {message}"));
                if let Some(id) = id {
                    self.pending.remove(&id);
                }
            }
            protocol::Incoming::Reply { id, result } => {
                let Some(method) = self.pending.remove(&id) else {
                    return;
                };
                if method == protocol::RPC_INITIALIZE {
                    match serde_json::from_value::<protocol::InitializeResult>(result.clone()) {
                        Ok(init) => {
                            self.net.cmd(NetCmd::Send(protocol::notification(
                                protocol::RPC_INITIALIZED,
                                serde_json::json!({}),
                            )));
                            self.collections = init.collections.clone();
                            self.console_lines_push(format!(
                                "[ws] {} v{} — {} collections",
                                init.server,
                                init.server_version.as_deref().unwrap_or("?"),
                                init.collections.len()
                            ));
                            self.inspector_built = true;
                            self.ensure_active_list();
                            self.ensure_prefab_map();
                        }
                        Err(e) => self.console_lines_push(format!("[ws] initialize parse: {e}")),
                    }
                    return;
                }
                if let Some((col_id, action)) = method.rsplit_once('.') {
                    match action {
                        "list" => {
                            self.lists.insert(col_id.to_string(), result.clone());
                            if col_id == "prefabs" {
                                self.prefab_map = protocol::list_rows(&result)
                                    .iter()
                                    .filter_map(protocol::prefab_row)
                                    .collect();
                            }
                        }
                        "get" => {
                            self.detail = Some(result);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// One pump tick: drains scan/net/runner, drives reconnect/refresh
    /// timers and keeps the MCP child process alive — the gpui analog of
    /// the head of the egui `update()` loop.
    fn poll(&mut self) {
        self.poll_scan();
        self.poll_net();
        self.poll_runner();
        let inspector = self.inspector_addr();
        self.mcp.ensure_started(&inspector);
        self.mcp.poll();
    }
}

/// Background task pumping `StudioState` every [`PUMP_INTERVAL`] until the
/// entity is released. Call once after creating the state entity.
pub fn spawn_pump(state: Entity<StudioState>, cx: &mut App) {
    let weak = state.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(PUMP_INTERVAL).await;
            // A released entity means the window/app is gone; stop pumping.
            let Some(state) = weak.upgrade() else {
                break;
            };
            state.update(cx, |state, cx| {
                state.poll();
                cx.notify();
            });
        }
    })
    .detach();
}

fn absolute(p: &std::path::Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(p)
            .canonicalize()
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Load `.deerpunk/scenarios.toml` — `[scenario.<id>] label = "…"; args = […]`.
/// Missing file yields the built-in empty `default` scenario. 1:1 port of
/// the egui `load_scenarios` (src/app.rs).
fn load_scenarios(project_root: &std::path::Path) -> Vec<Scenario> {
    let fallback = || {
        vec![Scenario {
            id: "default".into(),
            label: "Default".into(),
            args: Vec::new(),
        }]
    };
    let Ok(text) = std::fs::read_to_string(project_root.join(".deerpunk/scenarios.toml")) else {
        return fallback();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return fallback();
    };
    let Some(table) = value.get("scenario").and_then(|v| v.as_table()) else {
        return fallback();
    };
    let mut out = Vec::new();
    for (id, value) in table {
        let label = value
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .to_string();
        let args = value
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        out.push(Scenario {
            id: id.clone(),
            label,
            args,
        });
    }
    if out.is_empty() { fallback() } else { out }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Instant;

    use deerpunk_studio::ansi::Stream;
    use super::Scenario;
    use deerpunk_studio::config::StudioConfig;
    use deerpunk_studio::mcp::McpManager;
    use deerpunk_studio::net::{ConnState, NetClient};
    use deerpunk_studio::profiles::Profiles;
    use deerpunk_studio::runner::{self, RunState, Runner, Step, StepState};

    use super::{CONSOLE_CAPACITY, StudioState, load_scenarios};

    /// A bare `StudioState` without the `new()` side effects (config load,
    /// project open, inspector auto-connect): the pump's run-instance
    /// handling is what's under test.
    fn test_state() -> StudioState {
        StudioState {
            cfg: StudioConfig::default(),
            project: None,
            gameinfo: None,
            scan: None,
            scan_rx: None,
            error: None,
            net: NetClient::new(),
            conn_state: ConnState::Disconnected,
            want_connected: false,
            reconnect_at: None,
            host: "127.0.0.1".into(),
            port: "9000".into(),
            refresh_ms: 250,
            last_refresh: Instant::now(),
            next_rpc_id: 1,
            pending: HashMap::new(),
            collections: Vec::new(),
            lists: HashMap::new(),
            last_fetch: HashMap::new(),
            prefab_map: HashMap::new(),
            selection: None,
            detail: None,
            inspector_built: false,
            profiles: Profiles::default(),
            active_profile: "dev".into(),
            scenarios: vec![Scenario {
                id: "default".into(),
                label: "Default".into(),
                args: Vec::new(),
            }],
            active_scenario: "default".into(),
            runner: Runner::new(),
            run_state: RunState::Idle,
            steps: Vec::new(),
            console: Stream::new(CONSOLE_CAPACITY),
            input: String::new(),
            mcp: McpManager::new(9100),
            active_collection: None,
        }
    }

    fn console_text(state: &StudioState) -> String {
        state
            .console
            .lines
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A throwaway project root, optionally holding a
    /// `.deerpunk/scenarios.toml`.
    fn temp_root(content: Option<&str>) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "deerpunk-studio-scenarios-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".deerpunk")).unwrap();
        if let Some(content) = content {
            std::fs::write(root.join(".deerpunk/scenarios.toml"), content).unwrap();
        }
        root
    }

    /// Missing file → the built-in empty `default` scenario (egui parity).
    #[test]
    fn missing_scenarios_file_yields_default() {
        let root = temp_root(None);
        let scenarios = load_scenarios(&root);
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0].id, "default");
        assert_eq!(scenarios[0].label, "Default");
        assert!(scenarios[0].args.is_empty());
    }

    /// The real-world file format (comments, string-array args) parses like
    /// the egui loader; label falls back to the id when absent.
    #[test]
    fn scenarios_toml_parses() {
        let root = temp_root(Some(
            "# engine arg scenarios\n\
             [scenario.default]\n\
             label = \"Default\"\n\
             \n\
             [scenario.no-inspector]\n\
             label = \"No inspector\"\n\
             args = [\"--inspector-port\", \"0\"]\n\
             \n\
             [scenario.inspector-lan]\n\
             args = [\"--inspector-host\", \"0.0.0.0\", \"--inspector-port\", \"9000\"]\n",
        ));
        let scenarios = load_scenarios(&root);
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(scenarios.len(), 3);
        let default = scenarios.iter().find(|s| s.id == "default").unwrap();
        assert_eq!(default.label, "Default");
        assert!(default.args.is_empty());
        let no_inspector = scenarios.iter().find(|s| s.id == "no-inspector").unwrap();
        assert_eq!(no_inspector.label, "No inspector");
        assert_eq!(no_inspector.args, vec!["--inspector-port", "0"]);
        let lan = scenarios.iter().find(|s| s.id == "inspector-lan").unwrap();
        // Label falls back to the id.
        assert_eq!(lan.label, "inspector-lan");
        assert_eq!(
            lan.args,
            vec!["--inspector-host", "0.0.0.0", "--inspector-port", "9000"]
        );
    }

    /// Unparsable TOML → fallback (egui parity).
    #[test]
    fn broken_scenarios_toml_yields_default() {
        let root = temp_root(Some("this line has no toml structure"));
        let scenarios = load_scenarios(&root);
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0].id, "default");
    }

    /// A scenarios.toml without a `[scenario.*]` table → fallback.
    #[test]
    fn scenarios_without_table_yields_default() {
        let root = temp_root(Some("[profile.dev]\nlabel = \"Dev\"\n"));
        let scenarios = load_scenarios(&root);
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0].id, "default");
    }

    /// A crash parks the runner at `Failed` without a trailing `Idle`
    /// (src/runner.rs only sends one after Stop/exit-0): the pump must land
    /// in `Idle` anyway, with the reason visible in the console.
    #[test]
    fn crash_without_trailing_idle_lands_in_idle() {
        let mut state = test_state();
        state.apply_run_event(runner::Event::State(RunState::Building));
        state.apply_run_event(runner::Event::State(RunState::Failed(
            "engine exited with 101".into(),
        )));
        assert_eq!(state.run_state, RunState::Idle);
        assert!(console_text(&state).contains("run failed: engine exited with 101"));
    }

    /// The full Building → Failed → Idle drain applies in order (no event
    /// loss/reorder): the trailing `Idle` wins, step detail survives and
    /// the launch button ends clickable (`run_state == Idle`).
    #[test]
    fn building_failed_idle_drains_in_order() {
        let mut state = test_state();
        state.steps = runner::STEP_NAMES
            .iter()
            .map(|n| Step {
                name: (*n).to_string(),
                state: StepState::Pending,
            })
            .collect();
        state.apply_run_event(runner::Event::State(RunState::Building));
        state.apply_run_event(runner::Event::Console(b"error: could not compile".to_vec()));
        state.apply_run_event(runner::Event::Step(
            2,
            StepState::Failed("exit code 101".into()),
        ));
        state.apply_run_event(runner::Event::State(RunState::Failed(
            "engine build failed".into(),
        )));
        state.apply_run_event(runner::Event::State(RunState::Idle));
        assert_eq!(state.run_state, RunState::Idle);
        assert_eq!(
            state.steps[2].state,
            StepState::Failed("exit code 101".into())
        );
        assert!(console_text(&state).contains("run failed: engine build failed"));
        assert!(console_text(&state).contains("error: could not compile"));
    }
}
