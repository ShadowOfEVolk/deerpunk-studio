//! Application shell: IDE-style layout — top toolbar, far-left view rail,
//! resource sidebar, central content, bottom console panel, status bar.
//! Live data comes from the engine inspector WS server (JSON-RPC over WS).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use serde_json::Value;

use crate::ansi::Stream;
use crate::assets::{self, ScanResult};
use crate::config::StudioConfig;
use crate::gameinfo::GameInfo;
use crate::mcp::McpManager;
use crate::net::{ConnState, NetClient, NetCmd, NetEvent};
use crate::panels;
use crate::profiles::Profiles;
use crate::project::Project;
use crate::protocol::{self, CollectionDescriptor};
use crate::runner::{self, Event as RunEvent, RunState, Runner, Step, StepState};
use crate::theme;
use egui_phosphor::variants::regular as ph;

/// Top-level views, switched from the left rail (VS Code activity bar).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum View {
    /// Hierarchical file browser (project root + gamedir) + asset viewers.
    Files,
    /// cvars_catalog.json.
    Cvars,
    /// Launch profiles + scenarios editor (gear in the launch panel).
    LaunchSettings,
    /// Puffin profiler (flame graph from the running game, :8585).
    Puffin,
    /// Live engine collection (entities, assets, players, …).
    Live(String),
}

/// A file opened in the main-zone tab strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFile {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Display path relative to the project root.
    pub rel: String,
}

/// Test scenario: extra engine CLI args, lighter than a launch profile.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub id: String,
    pub label: String,
    /// Extra engine args, e.g. ["--inspector-port", "9000"].
    pub args: Vec<String>,
}

/// Live Puffin profiler session: the HTTP client self-pumps frames from the
/// game's puffin server; `ProfilerUi` renders the flame graph.
pub struct PuffinTab {
    pub client: puffin_http::Client,
    pub profiler: puffin_egui::ProfilerUi,
}

impl PuffinTab {
    /// Connect a self-pumping profiler client to `addr` (host:port).
    pub fn new(addr: &str) -> Self {
        Self {
            client: puffin_http::Client::new(addr.to_string()),
            profiler: puffin_egui::ProfilerUi::default(),
        }
    }
}

/// Game icon bytes, read and decode-checked once per project. Fed to egui
/// as a `bytes://` source because the `file://` loader is not compiled in
/// (`egui_extras` runs without its `file` feature).
#[derive(Clone)]
pub struct GameIcon {
    /// Stable per-icon-path uri, so egui's image cache never mixes icons.
    uri: String,
    /// Raw icon file bytes (known decodable).
    bytes: Arc<[u8]>,
}

pub const REFRESH_OPTIONS: [u64; 5] = [0, 200, 250, 500, 1000];
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Selection in a collection: which row, plus the id string for matching.
pub struct Selection {
    pub col: String,
    pub index_str: String,
}

pub struct StudioApp {
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
    pub collections: Vec<CollectionDescriptor>,
    pub lists: HashMap<String, Value>,
    /// Per-collection time of the last `.list` fetch (lazy: only the active
    /// Live view's collection is fetched).
    pub last_fetch: HashMap<String, Instant>,
    pub selection: Option<Selection>,
    pub detail: Option<Value>,
    /// Prefab → decoded icon texture (None = known missing/no icon), so each
    /// prefab file is parsed and uploaded once per session.
    pub prefab_icons: HashMap<String, Option<egui::TextureHandle>>,
    pub inspector_built: bool,

    // Run instance state.
    pub profiles: Profiles,
    pub active_profile: String,
    pub runner: Runner,
    pub run_state: RunState,
    pub steps: Vec<Step>,
    pub console: Stream,
    pub input: String,
    pub console_open: bool,

    // Puffin profiler connection (View::Puffin).
    pub puffin: Option<PuffinTab>,
    pub puffin_addr: String,
    pub puffin_auto: bool,
    pub puffin_retry_at: Instant,

    // MCP server (deerpunk-mcp child process) + its status modal.
    pub mcp: McpManager,
    pub mcp_modal_open: bool,

    // Layout state.
    pub active_view: View,
    pub cvars_category: Option<String>,

    // Main-zone file tabs (file browser / asset viewer).
    pub open_tabs: Vec<OpenFile>,
    pub active_tab: Option<usize>,

    // Launch scenarios (engine arg sets).
    pub scenarios: Vec<Scenario>,
    pub active_scenario: String,

    // Game info modal (top bar icon click).
    pub show_gameinfo_modal: bool,
    /// Recents snapshot pushed to the native menu last.
    pub last_recents: Vec<String>,
    /// The project's game icon, loaded eagerly (see [`GameIcon`]).
    pub game_icon: Option<GameIcon>,

    // Per-view UI state.
    pub cvars_filter: String,
    pub filters: HashMap<String, String>,
}

impl StudioApp {
    /// Texture for a prefab's icon, decoded and uploaded once per prefab.
    /// Missing prefab file / icon key / image yields None (cached) — never
    /// an error surfaced to the UI.
    pub fn prefab_icon(
        &mut self,
        ctx: &egui::Context,
        prefab: &str,
    ) -> Option<egui::TextureHandle> {
        if let Some(tex) = self.prefab_icons.get(prefab) {
            return tex.clone();
        }
        let tex = self
            .project
            .as_ref()
            .and_then(|p| load_prefab_icon(&p.gamedir, ctx, prefab));
        self.prefab_icons.insert(prefab.to_string(), tex.clone());
        tex
    }

    pub fn new(cc: &eframe::CreationContext<'_>, args: &[String]) -> Self {
        theme::apply(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        // Native system File menu (macOS menubar; no-op elsewhere).
        crate::native_menu::install();

        let cfg = StudioConfig::load();
        let mut app = Self {
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
            selection: None,
            detail: None,
            prefab_icons: HashMap::new(),
            inspector_built: false,
            profiles: Profiles::default(),
            active_profile: "dev".into(),
            runner: Runner::new(),
            run_state: RunState::Idle,
            steps: Vec::new(),
            console: Stream::new(5000),
            input: String::new(),
            console_open: true,
            puffin: None,
            puffin_addr: "127.0.0.1:8585".into(),
            puffin_auto: true,
            puffin_retry_at: Instant::now(),
            active_view: View::Files,
            cvars_category: None,
            open_tabs: Vec::new(),
            active_tab: None,
            scenarios: vec![Scenario {
                id: "default".into(),
                label: "Default".into(),
                args: Vec::new(),
            }],
            active_scenario: "default".into(),
            show_gameinfo_modal: false,
            last_recents: Vec::new(),
            game_icon: None,
            mcp: McpManager::new(9100),
            mcp_modal_open: false,
            cvars_filter: String::new(),
            filters: HashMap::new(),
        };
        // `--port <n>`: inspector port override (must land before any
        // connect, including the one inside open_project).
        let mut args: Vec<String> = args.to_vec();
        if let Some(pos) = args.iter().position(|a| a == "--port") {
            if let Some(p) = args.get(pos + 1) {
                app.port = p.clone();
            }
            let end = (pos + 1).min(args.len() - 1);
            args.drain(pos..=end);
        }
        let path_arg = args.iter().find(|a| a.as_str() != "--run");
        if let Some(path) = path_arg {
            app.open_project(PathBuf::from(path));
        } else {
            // Launched from a game folder with no args: use the current dir.
            let cwd = std::env::current_dir().unwrap_or_default();
            if cwd.join("deerpunk.toml").is_file() {
                app.open_project(cwd);
            } else if let Some(last) = app.cfg.last_project.clone() {
                app.open_project(PathBuf::from(last));
            }
        }
        // `--run`: start the instance immediately (build + engine launch).
        if args.iter().any(|a| a == "--run") {
            app.run_instance();
        }
        // Puffin auto-connect: the client retries below once the game is up.
        app.puffin = Some(PuffinTab::new(&app.puffin_addr));
        // Inspector auto-connect even without a project (engine may already
        // be running); open_project() connects on its own.
        if !app.want_connected {
            app.connect();
        }
        app
    }

    /// Open a game project folder (must contain `deerpunk.toml`) and
    /// auto-connect to its inspector server.
    pub fn open_project(&mut self, root: PathBuf) {
        let root = absolute(&root);
        match Project::load(&root) {
            Ok(project) => {
                self.error = None;
                let gameinfo = GameInfo::from_gamedir(&project.gamedir);
                self.game_icon = load_game_icon(&project.gamedir, &gameinfo);
                self.gameinfo = Some(gameinfo);
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
                self.project = Some(project);
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
                self.console = Stream::new(5000);
                self.steps.clear();
                self.connect();
            }
            Err(e) => {
                self.error = Some(format!("{}: {e}", root.display()));
            }
        }
    }

    pub fn pick_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Open game project folder (contains deerpunk.toml)")
            .pick_folder()
        {
            self.open_project(dir);
        }
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
        self.inspector_built = false;
        self.net.cmd(NetCmd::Connect { host, port });
    }

    pub fn disconnect(&mut self) {
        self.want_connected = false;
        self.reconnect_at = None;
        self.net.cmd(NetCmd::Disconnect);
    }

    pub fn reconnect(&mut self) {
        self.want_connected = true;
        self.reconnect_at = None;
        self.net.cmd(NetCmd::Disconnect);
        let host = self.host.trim().to_string();
        if let Ok(port) = self.port.trim().parse::<u16>() {
            self.net.cmd(NetCmd::Connect { host, port });
        }
    }

    /// Keep the puffin client alive in auto mode: recreate it while the
    /// game's profiler server is not reachable yet (e.g. at app launch).
    fn tick_puffin(&mut self) {
        if self.puffin.is_none() && self.puffin_auto {
            self.puffin = Some(PuffinTab::new(&self.puffin_addr));
            self.puffin_retry_at = Instant::now() + Duration::from_secs(3);
            return;
        }
        let Some(tab) = self.puffin.as_mut() else {
            return;
        };
        if self.puffin_auto && !tab.client.connected() && Instant::now() >= self.puffin_retry_at {
            tab.client = puffin_http::Client::new(self.puffin_addr.clone());
            self.puffin_retry_at = Instant::now() + Duration::from_secs(3);
        }
    }

    /// Inspector WS address as `host:port` (for the MCP server args).
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

    /// The collection whose Live view is active, if any.
    fn active_collection(&self) -> Option<&str> {
        match &self.active_view {
            View::Live(id) => Some(id),
            _ => None,
        }
    }

    /// Fetch the active view's collection list (if it is a list-mode
    /// collection) and the active selection's detail.
    pub fn request_refresh(&mut self) {
        if self.conn_state != ConnState::Connected {
            return;
        }
        self.last_refresh = Instant::now();
        let Some(id) = self.active_collection().map(str::to_string) else {
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

    /// Lazy fetch for the active view: issue `.list` only when the data is
    /// missing or (with a refresh interval) older than the interval. Called
    /// on view switch and after initialize.
    pub fn ensure_active_list(&mut self) {
        if self.conn_state != ConnState::Connected {
            return;
        }
        let Some(id) = self.active_collection().map(str::to_string) else {
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

    /// Select a row: request the detail snapshot for get-mode collections.
    pub fn select(&mut self, col: String, index_str: String) {
        self.selection = Some(Selection { col, index_str });
        self.request_refresh();
    }

    /// Launch the pre_run pipeline + engine instance with the active profile.
    pub fn run_instance(&mut self) {
        if self.run_state != RunState::Idle {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };
        let Some(profile) = self.profiles.get(&self.active_profile).cloned() else {
            self.error = Some(format!("no run profile `{}`", self.active_profile));
            return;
        };
        self.console = Stream::new(5000);
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
            project_root: project.root.clone(),
            gamedir: project.gamedir.clone(),
            engine_dir: project.engine_dir.clone(),
            profile,
            gamelibs: project.gamelibs.clone(),
            extra_args: scenario_args,
        });
    }

    pub fn stop_instance(&mut self) {
        self.runner.stop();
    }

    fn poll_runner(&mut self) {
        for ev in self.runner.poll() {
            match ev {
                RunEvent::Console(bytes) => {
                    self.console.feed(&bytes);
                    self.dump_console(&bytes);
                }
                RunEvent::State(state) => self.run_state = state,
                RunEvent::Step(idx, state) => {
                    if let Some(step) = self.steps.get_mut(idx) {
                        step.state = state;
                    }
                }
                RunEvent::Note(text) => {
                    self.console.feed(format!("{text}\n").as_bytes());
                    self.dump_console(text.as_bytes());
                }
                RunEvent::Exited(_) => {}
            }
        }
    }

    /// `DEERPUNK_STUDIO_CONSOLE=<path>`: append raw console bytes to a file
    /// (headless debugging of instance output).
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
                        }
                        Err(e) => self.console_lines_push(format!("[ws] initialize parse: {e}")),
                    }
                    return;
                }
                if let Some((col_id, action)) = method.rsplit_once('.') {
                    match action {
                        "list" => {
                            self.lists.insert(col_id.to_string(), result);
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

    /// Console lines that originate from the studio itself.
    fn console_lines_push(&mut self, text: String) {
        self.console.feed(format!("{text}\n").as_bytes());
        self.dump_console(text.as_bytes());
    }

    // ------------------------------------------------------------- layout

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        // Three fixed zones: identity + File menu (left), launch cluster
        // (centered), connect status (right). The cluster holds its place
        // while the side zones resize.
        const CENTER_WIDTH: f32 = 640.0;
        let strip = ui.available_rect_before_wrap();
        let center_w = CENTER_WIDTH.min(strip.width() * 0.5);
        let center_left = strip.left() + (strip.width() - center_w) * 0.5;
        let center_rect = egui::Rect::from_min_max(
            egui::pos2(center_left, strip.top()),
            egui::pos2(center_left + center_w, strip.bottom()),
        );
        let left_rect = egui::Rect::from_min_max(strip.left_top(), center_rect.left_bottom());
        let right_rect = egui::Rect::from_min_max(center_rect.right_top(), strip.right_bottom());

        let mut left = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(left_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        self.top_bar_left(&mut left);

        let mut center = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(center_rect)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        // `top_down` centers the row horizontally; nudge it down so it is
        // centered vertically too.
        center.add_space((center.available_height() - 32.0).max(0.0) * 0.5);
        center.horizontal(|center| self.top_bar_center(center));

        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(right_rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        self.top_bar_right(&mut right);
    }

    /// Left zone: clickable game identity (icon + project name); click opens
    /// the gameinfo modal. Open/Recent live in the native system File menu.
    fn top_bar_left(&mut self, ui: &mut egui::Ui) {
        self.identity_block(ui);
    }

    /// Game icon (32px, ~4px rounded corners, no box or border) with the
    /// project name beside it in logo-sized type. Both are clickable and
    /// open the gameinfo modal.
    fn identity_block(&mut self, ui: &mut egui::Ui) {
        const ICON: f32 = 32.0;
        let size = egui::vec2(ICON, ICON);
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::CLICK | egui::Sense::HOVER);

        // The game's own icon, or a controller glyph when none loads.
        // Bytes were decode-checked at load time, so the egui-side load
        // only ever succeeds (blank for the first frame while it uploads).
        if let Some(icon) = self.game_icon.clone() {
            ui.put(
                rect,
                egui::Image::new(egui::ImageSource::Bytes {
                    uri: icon.uri.into(),
                    bytes: egui::load::Bytes::Shared(icon.bytes),
                })
                .fit_to_exact_size(size)
                .corner_radius(4.0),
            );
        } else {
            glyph_box(ui, rect, 17.0);
        }

        let has_project = self.project.is_some();
        let name = self
            .project
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "no project".into());
        // Plain label look: no button chrome (fill AND stroke off), font
        // larger than every other control in the bar — it's the game logo.
        let name_resp = ui.add(
            egui::Button::new(egui::RichText::new(name).strong().size(21.0).color(
                if has_project {
                    theme::TEXT
                } else {
                    theme::MUTED
                },
            ))
            .fill(egui::Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .truncate(),
        );
        if resp.clicked() || name_resp.clicked() {
            self.show_gameinfo_modal = true;
        }
        resp.on_hover_text("Game info");
        name_resp.on_hover_text("Game info");
    }

    /// Native macOS File menu: drain clicks, keep "Open Recent" in sync.
    fn poll_native_menu(&mut self) {
        for action in crate::native_menu::poll_actions() {
            match action {
                crate::native_menu::NativeMenuAction::OpenProject => self.pick_folder(),
                crate::native_menu::NativeMenuAction::OpenRecent(path) => {
                    self.open_project(PathBuf::from(path))
                }
            }
        }
        let recents = self.cfg.recents.clone();
        if recents != self.last_recents {
            crate::native_menu::set_recents(&recents);
            self.last_recents = recents;
        }
    }

    /// Center zone: ONE mega launch button — play icon, `profile / scenario`
    /// label, and a dropdown chevron sharing a single frame. While a run is
    /// active it glows softly and shows the state icon + dim label.
    fn top_bar_center(&mut self, ui: &mut egui::Ui) {
        const BTN_H: f32 = 32.0;
        const PLAY_W: f32 = 40.0;
        const CHEV_W: f32 = 28.0;

        let running = self.run_state != RunState::Idle;
        let enabled = self.project.is_some();
        // The play slot doubles as the run-state indicator: spinner while
        // building/stopping, pulse while running, warning on failure. Click
        // always stops while a run is active.
        let (glyph, tint) = if running {
            match &self.run_state {
                RunState::Building | RunState::Stopping => (ph::CIRCLE_NOTCH, theme::YELLOW),
                RunState::Running => (ph::PULSE, theme::GREEN),
                RunState::Failed(_) => (ph::WARNING, theme::RED),
                RunState::Idle => (ph::STOP, theme::RED),
            }
        } else {
            (ph::PLAY, theme::ACCENT)
        };

        let profile_label = self
            .profiles
            .get(&self.active_profile)
            .map(|p| p.label.clone())
            .unwrap_or_else(|| self.active_profile.clone());
        let scenario_label = self
            .scenarios
            .iter()
            .find(|s| s.id == self.active_scenario)
            .map(|s| s.label.clone())
            .unwrap_or_else(|| self.active_scenario.clone());
        let label = format!("{profile_label} / {scenario_label}");

        // Truncate the label to the room we can afford (leave space for the
        // run status and the connection dot on the right).
        let font = egui::FontId::proportional(15.0);
        let max_text_w = (ui.available_width() - PLAY_W - CHEV_W - 28.0 - 110.0).max(60.0);
        let galley = ui
            .painter()
            .layout_no_wrap(label, font.clone(), theme::TEXT);
        let text_w = galley.size().x.min(max_text_w);

        let total_w = PLAY_W + text_w + CHEV_W + 20.0;
        let (rect, mega) = ui.allocate_exact_size(egui::vec2(total_w, BTN_H), egui::Sense::HOVER);

        // Soft glow while running: layered rounded strokes of decreasing
        // alpha (renders reliably under wgpu, unlike frame shadows).
        let p = ui.painter();
        if running {
            for (w, a) in [(6.0, 0x18u8), (3.0, 0x30), (1.0, 0x50)] {
                p.rect_stroke(
                    rect.expand(w * 0.5),
                    2.0 + w * 0.5,
                    egui::Stroke::new(w, tinted(tint, a)),
                    egui::StrokeKind::Outside,
                );
            }
        }

        let fill = if !enabled {
            theme::FIELD
        } else if mega.hovered() {
            tinted(tint, 0x4d)
        } else {
            tinted(tint, 0x2e)
        };
        p.rect_filled(rect, 2.0, fill);

        // Play zone: launch or stop.
        let play_rect = egui::Rect::from_min_size(rect.left_top(), egui::vec2(PLAY_W, BTN_H));
        let text_rect = egui::Rect::from_min_max(
            play_rect.right_top(),
            egui::pos2(rect.right() - CHEV_W, rect.bottom()),
        );
        let chev_rect = egui::Rect::from_min_max(text_rect.right_top(), rect.right_bottom());

        let play = ui.interact(play_rect, ui.id().with("mega_play"), egui::Sense::CLICK);
        p.text(
            play_rect.center(),
            egui::Align2::CENTER_CENTER,
            glyph,
            egui::FontId::proportional(26.0),
            if enabled { tint } else { theme::TEXT_DIM },
        );
        if enabled && play.clicked() {
            if running {
                self.stop_instance();
            } else {
                self.run_instance();
            }
        }
        play.on_hover_text(if running {
            "Stop instance"
        } else {
            "Run instance"
        });

        // Label, clipped to its zone (single line, truncate to fit).
        p.with_clip_rect(text_rect.intersect(rect)).text(
            egui::pos2(text_rect.left() + 2.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            galley.text(),
            font,
            if enabled { theme::TEXT } else { theme::MUTED },
        );

        // Subtle vertical divider between label and chevron.
        let dx = chev_rect.left() - 3.0;
        p.line_segment(
            [
                egui::pos2(dx, rect.top() + 6.0),
                egui::pos2(dx, rect.bottom() - 6.0),
            ],
            egui::Stroke::new(1.0, tinted(theme::TEXT, 0x26)),
        );

        // Dropdown zone: one combined popup with profiles, scenarios and
        // the launch-settings entry.
        let chev = ui.interact(chev_rect, ui.id().with("mega_menu"), egui::Sense::CLICK);
        p.text(
            chev_rect.center(),
            egui::Align2::CENTER_CENTER,
            ph::CARET_DOWN,
            egui::FontId::proportional(15.0),
            theme::TEXT,
        );
        egui::Popup::menu(&chev).show(|ui| {
            for id in self.profiles.order.clone() {
                let Some(pr) = self.profiles.get(&id) else {
                    continue;
                };
                let active = id == self.active_profile;
                let text = if active {
                    format!("{}  {}", ph::CHECK, pr.label)
                } else {
                    format!("     {}", pr.label)
                };
                if ui.selectable_label(active, text).clicked() {
                    self.active_profile = id;
                    ui.close();
                }
            }
            ui.separator();
            for s in self.scenarios.clone() {
                let active = s.id == self.active_scenario;
                let text = if active {
                    format!("{}  {}", ph::CHECK, s.label)
                } else {
                    format!("     {}", s.label)
                };
                if ui.selectable_label(active, text).clicked() {
                    self.active_scenario = s.id;
                    ui.close();
                }
            }
            ui.separator();
            if ui
                .button(format!("{}  Launch settings", ph::GEAR))
                .clicked()
            {
                self.active_view = View::LaunchSettings;
                ui.close();
            }
        });

        ui.separator();
        connect_dot(ui, &self.conn_state);
    }

    /// Right zone: connection badge + inspector host:port. The text drops
    /// out first when the zone runs out of room (badge only).
    fn top_bar_right(&mut self, ui: &mut egui::Ui) {
        conn_badge(ui, &self.conn_state);
        let text = format!("{}:{}", self.host, self.port);
        if ui.available_width() > text.len() as f32 * 8.0 + 16.0 {
            ui.label(theme::small(text));
        }
    }

    /// Gameinfo modal: opens from the top bar identity block. Backdrop
    /// click, the × button or Escape all close it.
    fn gameinfo_modal(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("gameinfo_modal"))
            .backdrop_color(egui::Color32::from_black_alpha(120))
            .show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.horizontal(|ui| {
                    let title = self
                        .gameinfo
                        .as_ref()
                        .map(|i| i.window_title().to_string())
                        .unwrap_or_else(|| "Game info".into());
                    ui.strong(title);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(ph::X).frame(false)).clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                match &self.gameinfo {
                    Some(info) => {
                        kv_row(ui, "gamefolder", &info.gamefolder);
                        kv_row(ui, "title", &info.title);
                        kv_row(ui, "basedir", &info.basedir);
                        kv_row(ui, "icon", &info.icon);
                        kv_row(ui, "version", &info.version);
                        kv_row(ui, "startmap", &info.startmap);
                        kv_row(ui, "trainmap", &info.trainmap);
                        kv_row(ui, "gamedll", &info.gamedll);
                        if !info.extras.is_empty() {
                            ui.add_space(4.0);
                            let mut extras: Vec<(&String, &String)> = info.extras.iter().collect();
                            extras.sort();
                            for (k, v) in extras {
                                kv_row(ui, k, v);
                            }
                        }
                    }
                    None => {
                        ui.label(theme::small("no gameinfo.txt"));
                    }
                }
            });
        if close || modal.backdrop_response.clicked() {
            self.show_gameinfo_modal = false;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_gameinfo_modal = false;
        }
    }

    /// Far-left view rail (VS Code activity bar).
    fn rail(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.add_space(6.0);
            self.rail_button(ui, View::Files, ph::FOLDER_SIMPLE, "File browser");
            self.rail_button(ui, View::Cvars, ph::SLIDERS, "Cvars");
            self.rail_button(ui, View::Puffin, ph::FIRE, "Puffin profiler");

            // Live collections (from the engine handshake).
            if !self.collections.is_empty() {
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(2.0);
                let ids: Vec<String> = self.collections.iter().map(|c| c.id.clone()).collect();
                for id in ids {
                    let label = self
                        .collections
                        .iter()
                        .find(|c| c.id == id)
                        .map(|c| c.label.clone())
                        .unwrap_or_else(|| id.clone());
                    let glyph = live_glyph(&id);
                    self.rail_button(ui, View::Live(id.clone()), glyph, &label);
                }
            }

            // Bottom: MCP launcher above the console toggle.
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                let toggle = ph::TERMINAL_WINDOW;
                let btn = egui::Button::new(
                    egui::RichText::new(toggle)
                        .color(theme::TEXT_DIM)
                        .size(14.0),
                )
                .fill(egui::Color32::TRANSPARENT)
                .min_size(egui::vec2(36.0, 24.0));
                if ui.add(btn).clicked() {
                    self.console_open = !self.console_open;
                }
                ui.add_space(4.0);
                let mcp_up = self.mcp.is_running();
                let plug = egui::Button::new(
                    egui::RichText::new(ph::PLUG)
                        .color(if mcp_up { theme::GREEN } else { theme::MUTED })
                        .size(14.0),
                )
                .fill(egui::Color32::TRANSPARENT)
                .min_size(egui::vec2(36.0, 24.0));
                if ui
                    .add(plug)
                    .on_hover_text("MCP server for AI agents")
                    .clicked()
                {
                    self.mcp_modal_open = true;
                }
            });
        });
    }

    fn rail_button(&mut self, ui: &mut egui::Ui, view: View, glyph: &str, tooltip: &str) {
        let active = self.active_view == view;
        let (fill, color) = if active {
            (theme::FIELD, theme::TEXT)
        } else {
            (egui::Color32::TRANSPARENT, theme::TEXT_DIM)
        };
        let badge = match &view {
            View::Live(id) => self
                .lists
                .get(id)
                .map(|r| protocol::list_rows(r).len())
                .unwrap_or(0),
            _ => 0,
        };
        let text = if badge > 0 {
            egui::RichText::new(format!("{glyph}\n{badge}"))
                .color(color)
                .size(11.0)
        } else {
            egui::RichText::new(glyph).color(color).size(17.0)
        };
        let btn = egui::Button::new(text)
            .fill(fill)
            .corner_radius(4.0)
            .min_size(egui::vec2(38.0, 30.0));
        let resp = ui.add(btn);
        // VS Code-style active marker: accent stripe at the left edge.
        if active {
            let mut stripe = resp.rect;
            stripe.set_width(3.0);
            stripe = stripe.translate(egui::vec2(-4.0, 0.0));
            ui.painter().rect_filled(stripe, 1.0, theme::ACCENT);
        }
        if resp.clicked() {
            self.active_view = view;
            self.ensure_active_list();
        }
        resp.on_hover_text(tooltip);
    }

    /// Bottom status strip.
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            conn_badge(ui, &self.conn_state);
            if matches!(self.conn_state, ConnState::Connected | ConnState::Error(_)) {
                ui.label(theme::small(format!("{}:{}", self.host, self.port)));
            }
            if ui
                .add(egui::Button::new(theme::small("connect")).frame(false))
                .clicked()
            {
                self.connect();
            }
            if ui
                .add(egui::Button::new(theme::small("disconnect")).frame(false))
                .clicked()
            {
                self.disconnect();
            }
            egui::ComboBox::from_id_salt("refresh_ms")
                .selected_text(if self.refresh_ms == 0 {
                    theme::small("manual")
                } else {
                    theme::small(format!("{} ms", self.refresh_ms))
                })
                .show_ui(ui, |ui| {
                    for opt in REFRESH_OPTIONS {
                        let label = if opt == 0 {
                            "manual".to_string()
                        } else {
                            format!("{opt} ms")
                        };
                        if ui.selectable_label(self.refresh_ms == opt, label).clicked() {
                            self.refresh_ms = opt;
                        }
                    }
                });
            if ui
                .add(egui::Button::new(theme::small("refresh")).frame(false))
                .clicked()
            {
                self.request_refresh();
            }
            if ui
                .add(egui::Button::new(theme::small("reconnect")).frame(false))
                .clicked()
            {
                self.reconnect();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(err) = &self.error {
                    ui.label(
                        egui::RichText::new(format!("⚠ {err}"))
                            .small()
                            .color(theme::YELLOW),
                    );
                }
                if let (Some(scan), Some(project)) = (&self.scan, &self.project) {
                    let bytes: u64 = scan.entries.iter().map(|e| e.size).sum();
                    if !scan.errors.is_empty() {
                        ui.label(
                            egui::RichText::new(format!("{} walk errors", scan.errors.len()))
                                .small()
                                .color(theme::YELLOW),
                        );
                    }
                    ui.label(theme::small(format!(
                        "{} files · {} · gamedir {}",
                        scan.entries.len(),
                        assets::fmt_size(bytes),
                        if project.gamedir_valid() {
                            "ok"
                        } else {
                            "missing"
                        }
                    )));
                } else if self.scan_rx.is_some() {
                    ui.label(theme::small("scanning…"));
                }
            });
        });
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

    fn handle_dropped_files(&mut self, ui: &mut egui::Ui) {
        let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            let path = file.path().to_path_buf();
            if path.is_dir() {
                self.open_project(path);
            }
        }
    }
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_scan();
        self.poll_net();
        self.poll_runner();
        self.poll_native_menu();
        self.handle_dropped_files(ui);
        self.tick_puffin();
        {
            let inspector = self.inspector_addr();
            self.mcp.ensure_started(&inspector);
        }
        self.mcp.poll();
        if self.mcp_modal_open {
            self.mcp.request_status();
        }

        // Paint the whole window first so no platform clear color leaks.
        let full = ui.available_rect_before_wrap();
        ui.painter().rect_filled(full, 0.0, theme::BG);

        egui::Panel::top("topbar")
            .resizable(false)
            .exact_size(44.0)
            .frame(
                egui::Frame::new()
                    .fill(if self.run_state == RunState::Idle {
                        theme::HEADER
                    } else {
                        theme::BAR_ACTIVE
                    })
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ui, |ui| self.top_bar(ui));

        egui::Panel::left("rail")
            .resizable(false)
            .exact_size(48.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(4, 2)),
            )
            .show(ui, |ui| self.rail(ui));

        if !matches!(self.active_view, View::Puffin) {
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(250.0)
                .min_size(140.0)
                .frame(
                    egui::Frame::new()
                        .fill(theme::PANEL)
                        .inner_margin(egui::Margin::symmetric(8, 6)),
                )
                .show(ui, |ui| {
                    let view = self.active_view.clone();
                    panels::sidebar(ui, self, &view);
                });
        }

        // Bottom panels: the first one shown lands at the very bottom edge —
        // status bar first, console stacked above it.
        egui::Panel::bottom("statusbar")
            .resizable(false)
            .exact_size(24.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::HEADER)
                    .inner_margin(egui::Margin::symmetric(8, 1)),
            )
            .show(ui, |ui| self.status_bar(ui));

        if self.console_open {
            egui::Panel::bottom("console")
                .resizable(true)
                .default_size(220.0)
                .min_size(80.0)
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG)
                        .inner_margin(egui::Margin::symmetric(8, 4)),
                )
                .show(ui, |ui| panels::console_panel(ui, self));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::same(10)),
            )
            .show(ui, |ui| {
                let view = self.active_view.clone();
                panels::center(ui, self, &view);
            });

        if self.show_gameinfo_modal {
            self.gameinfo_modal(ui.ctx());
        }

        if self.mcp_modal_open {
            panels::mcp_modal(ui.ctx(), self);
        }

        if let Some(project) = &self.project {
            let title = self
                .gameinfo
                .as_ref()
                .map(|i| i.window_title().to_string())
                .unwrap_or_else(|| project.name.clone());
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Title(format!(
                    "deerpunk studio — {title}"
                )));
        }
    }
}

fn live_glyph(id: &str) -> &'static str {
    match id {
        "entities" => ph::CUBE,
        "assets" => ph::STACK,
        "players" => ph::USERS_THREE,
        "systems" => ph::GAUGE,
        "fmod_events" => ph::MUSIC_NOTES_SIMPLE,
        "methoddb" => ph::LIST_DASHES,
        _ => ph::DATABASE,
    }
}

fn conn_label(state: &ConnState) -> &'static str {
    match state {
        ConnState::Connected => "connected",
        ConnState::Connecting => "connecting",
        ConnState::Disconnected => "disconnected",
        ConnState::Error(_) => "error",
    }
}

fn conn_color(state: &ConnState) -> egui::Color32 {
    match state {
        ConnState::Connected => theme::GREEN,
        ConnState::Connecting => theme::YELLOW,
        ConnState::Disconnected => theme::MUTED,
        ConnState::Error(_) => theme::RED,
    }
}

/// RGB of `color` at an explicit alpha (unmultiplied).
fn tinted(color: egui::Color32, alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied_const(color.r(), color.g(), color.b(), alpha)
}

fn conn_badge(ui: &mut egui::Ui, state: &ConnState) {
    let color = conn_color(state);
    egui::Frame::new()
        .fill(tinted(color, 0x33))
        .corner_radius(2.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(theme::mono_colored(conn_label(state), color));
        });
}

/// Compact connection indicator at the end of the launch cluster.
fn connect_dot(ui: &mut egui::Ui, state: &ConnState) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::HOVER);
    ui.painter()
        .circle_filled(rect.center(), 4.0, conn_color(state));
    resp.on_hover_text(conn_label(state));
}

/// Fallback content of the identity icon: a controller glyph.
fn glyph_box(ui: &mut egui::Ui, rect: egui::Rect, size: f32) {
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        ph::GAME_CONTROLLER,
        egui::FontId::proportional(size),
        theme::TEXT_DIM,
    );
}

/// Read the project's game icon and decode-check it up front, so the
/// egui-side load can only ever succeed (async, cached, no per-frame IO).
/// Missing file or undecodable bytes yield `None` and the identity block
/// keeps its controller glyph.
/// Decode a prefab's icon into a texture: `gamedir/prefabs/{prefab}.prefab`
/// is a JSON `EntityDef` whose top level may carry `icon` (a gamedir-relative
/// image path) and an optional `icon_color` tint (`[r, g, b]`, 0–255 or
/// 0.0–1.0). Any failure (missing file, bad JSON, undecodable image) yields
/// `None` — the row simply renders without an icon.
fn load_prefab_icon(
    gamedir: &std::path::Path,
    ctx: &egui::Context,
    prefab: &str,
) -> Option<egui::TextureHandle> {
    let path = gamedir.join("prefabs").join(format!("{prefab}.prefab"));
    let text = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let icon = value.get("icon").and_then(serde_json::Value::as_str)?;
    let bytes = std::fs::read(gamedir.join(icon)).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let mut pixels: Vec<egui::Color32> = img
        .pixels()
        .map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
        .collect();
    if let Some(tint) = icon_color(&value) {
        for c in &mut pixels {
            *c = egui::Color32::from_rgba_unmultiplied(
                (c.r() as u32 * tint[0] as u32 / 255) as u8,
                (c.g() as u32 * tint[1] as u32 / 255) as u8,
                (c.b() as u32 * tint[2] as u32 / 255) as u8,
                c.a(),
            );
        }
    }
    let size = [img.width() as usize, img.height() as usize];
    let source_size = egui::vec2(img.width() as f32, img.height() as f32);
    let color_image = egui::ColorImage {
        size,
        pixels: std::mem::take(&mut pixels),
        source_size,
    };
    Some(ctx.load_texture(
        format!("prefab_icon_{prefab}"),
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}

/// Parse an optional `icon_color` JSON array into an 8-bit RGB tint.
/// Values ≤ 1.0 are treated as floats (scaled ×255); anything larger as 0–255.
fn icon_color(value: &serde_json::Value) -> Option<[u8; 3]> {
    let arr = value
        .get("icon_color")
        .and_then(serde_json::Value::as_array)?;
    if arr.len() != 3 {
        return None;
    }
    let mut out = [0u8; 3];
    for (i, v) in arr.iter().enumerate() {
        let f = v.as_f64()?;
        out[i] = if f <= 1.0 {
            (f * 255.0).round().clamp(0.0, 255.0) as u8
        } else {
            f.round().clamp(0.0, 255.0) as u8
        };
    }
    Some(out)
}

fn load_game_icon(gamedir: &std::path::Path, info: &GameInfo) -> Option<GameIcon> {
    let path = info.icon_path(gamedir)?;
    let bytes = std::fs::read(&path).ok()?;
    image::load_from_memory(&bytes).ok()?;
    Some(GameIcon {
        uri: format!("bytes://{}", path.display()),
        bytes: bytes.into(),
    })
}

/// `key value` row (modal variant; panels.rs owns the sidebar original).
fn kv_row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.strong(key);
        ui.label(theme::mono_text(value));
    });
}

fn absolute(p: &std::path::Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}
/// Load `.deerpunk/scenarios.toml` — `[scenario.<id>] label = "…"; args = […]`.
/// Missing file yields the built-in empty `default` scenario.
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
