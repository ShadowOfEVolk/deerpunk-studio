//! Application shell: IDE-style layout — top toolbar, far-left view rail,
//! resource sidebar, central content, bottom console panel, status bar.
//! Live data comes from the engine inspector WS server (JSON-RPC over WS).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use serde_json::Value;

use crate::ansi::Stream;
use crate::assets::{self, AssetKind, ScanResult};
use crate::config::StudioConfig;
use crate::gameinfo::GameInfo;
use crate::net::{ConnState, NetClient, NetCmd, NetEvent};
use crate::panels;
use crate::profiles::Profiles;
use crate::project::Project;
use crate::protocol::{self, CollectionDescriptor};
use crate::runner::{self, Event as RunEvent, Runner, RunState, Step, StepState};
use crate::theme;
use egui_phosphor::variants::regular as ph;

/// Top-level views, switched from the left rail (VS Code activity bar).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum View {
    /// deerpunk.toml + gameinfo summary/documents.
    Project,
    /// Files on disk (asset scan).
    DiskAssets,
    /// Texture gallery with previews.
    Textures,
    /// cvars_catalog.json.
    Cvars,
    /// Raw deerpunk.toml text.
    Raw,
    /// Live engine collection (entities, assets, players, …).
    Live(String),
}

/// Which document the Project view shows in the center.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectDoc {
    Summary,
    Toml,
    GameInfo,
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
    pub selection: Option<Selection>,
    pub detail: Option<Value>,
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

    // Layout state.
    pub active_view: View,
    pub project_doc: ProjectDoc,
    pub cvars_category: Option<String>,
    pub selected_texture: Option<String>,

    // Per-view UI state.
    pub assets_filter: String,
    pub assets_kind: Option<AssetKind>,
    pub textures_filter: String,
    pub cvars_filter: String,
    pub filters: HashMap<String, String>,
}

impl StudioApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: &[String]) -> Self {
        theme::apply(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);

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
            selection: None,
            detail: None,
            inspector_built: false,
            profiles: Profiles::default(),
            active_profile: "dev".into(),
            runner: Runner::new(),
            run_state: RunState::Idle,
            steps: Vec::new(),
            console: Stream::new(5000),
            input: String::new(),
            console_open: true,
            active_view: View::Project,
            project_doc: ProjectDoc::Summary,
            cvars_category: None,
            selected_texture: None,
            assets_filter: String::new(),
            assets_kind: None,
            textures_filter: String::new(),
            cvars_filter: String::new(),
            filters: HashMap::new(),
        };
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
        app
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
                self.project = Some(project);
                self.profiles = Profiles::load(&root);
                self.active_profile = self
                    .profiles
                    .order
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "dev".into());
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

    fn rpc(&mut self, method: &str, params: Value) {
        let id = self.next_rpc_id;
        self.next_rpc_id += 1;
        self.pending.insert(id, method.to_string());
        self.net.cmd(NetCmd::Send(protocol::request(id, method, params)));
    }

    /// Refresh every list-mode collection and the active selection.
    pub fn request_refresh(&mut self) {
        if self.conn_state != ConnState::Connected {
            return;
        }
        self.last_refresh = Instant::now();
        let ids: Vec<String> = self
            .collections
            .iter()
            .filter(|c| c.modes.contains(&protocol::CollectionMode::List))
            .map(|c| c.id.clone())
            .collect();
        for id in ids {
            self.rpc(&format!("{id}.list"), serde_json::json!({}));
        }
        if let Some(sel) = &self.selection {
            let method = format!("{}.get", sel.col);
            let index: Option<u64> = sel.index_str.parse().ok();
            if let Some(index) = index {
                self.rpc(&method, serde_json::json!({ "index": index }));
            }
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
            .map(|n| Step { name: (*n).to_string(), state: StepState::Pending })
            .collect();
        self.runner.start(runner::InstancePlan {
            project_root: project.root.clone(),
            gamedir: project.gamedir.clone(),
            engine_dir: project.engine_dir.clone(),
            profile,
            gamelibs: project.gamelibs.clone(),
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
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
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
                    if state == ConnState::Disconnected && self.want_connected {
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
            && matches!(self.conn_state, ConnState::Disconnected | ConnState::Error(_))
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
                            self.request_refresh();
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
        ui.horizontal(|ui| {
            if let Some(project) = &self.project {
                ui.label(
                    egui::RichText::new(&project.name)
                        .strong()
                        .color(theme::TEXT),
                );
            } else {
                ui.label(theme::small("no project — Open…"));
            }
            ui.separator();

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Right-to-left order: Run/Stop, then profile, then file
                // actions — so the profile dropdown sits left of Run.
                if self.run_state == RunState::Idle {
                    if ui.button(format!("{}  Run", ph::PLAY)).clicked() {
                        self.run_instance();
                    }
                } else {
                    if ui.button(format!("{}  Stop", ph::STOP)).clicked() {
                        self.stop_instance();
                    }
                    ui.label(theme::small(run_state_label(&self.run_state)));
                }
                let profile_label = self
                    .profiles
                    .get(&self.active_profile)
                    .map(|p| p.label.clone())
                    .unwrap_or_else(|| self.active_profile.clone());
                egui::ComboBox::from_id_salt("run_profile")
                    .selected_text(profile_label)
                    .show_ui(ui, |ui| {
                        for id in self.profiles.order.clone() {
                            let Some(p) = self.profiles.get(&id) else { continue };
                            if ui
                                .selectable_label(id == self.active_profile, &p.label)
                                .clicked()
                            {
                                self.active_profile = id;
                            }
                        }
                    });
                if ui.button(format!("{}  Open…", ph::FOLDER_OPEN)).clicked() {
                    self.pick_folder();
                }
                if !self.cfg.recents.is_empty() {
                    egui::ComboBox::from_id_salt("recents")
                        .selected_text(theme::small("Recents"))
                        .show_ui(ui, |ui| {
                            for recent in self.cfg.recents.clone() {
                                if ui.selectable_label(true, &recent).clicked() {
                                    self.open_project(PathBuf::from(&recent));
                                }
                            }
                        });
                }
            });
        });
    }

    /// Far-left view rail (VS Code activity bar).
    fn rail(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.add_space(6.0);
            self.rail_button(ui, View::Project, ph::HOUSE, "Project");
            self.rail_button(ui, View::DiskAssets, ph::HARD_DRIVES, "Disk assets");
            self.rail_button(ui, View::Textures, ph::IMAGE, "Textures");
            self.rail_button(ui, View::Cvars, ph::SLIDERS, "Cvars");
            self.rail_button(ui, View::Raw, ph::FILE_TEXT, "deerpunk.toml raw");

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

            // Bottom: console toggle.
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                let toggle = ph::TERMINAL_WINDOW;
                let btn = egui::Button::new(
                    egui::RichText::new(toggle).color(theme::TEXT_DIM).size(14.0),
                )
                .fill(egui::Color32::TRANSPARENT)
                .min_size(egui::vec2(36.0, 24.0));
                if ui.add(btn).clicked() {
                    self.console_open = !self.console_open;
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
            stripe = stripe.translate(egui::vec2(-6.0, 0.0));
            ui.painter().rect_filled(stripe, 1.0, theme::ACCENT);
        }
        if resp.clicked() {
            self.active_view = view;
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
                        if project.gamedir_valid() { "ok" } else { "missing" }
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
        self.handle_dropped_files(ui);

        // Paint the whole window first so no platform clear color leaks.
        let full = ui.available_rect_before_wrap();
        ui.painter()
            .rect_filled(full, 0.0, theme::BG);

        egui::Panel::top("topbar")
            .resizable(false)
            .exact_size(30.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::HEADER)
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ui, |ui| self.top_bar(ui));

        egui::Panel::left("rail")
            .resizable(false)
            .exact_size(48.0)
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(4, 2)))
            .show(ui, |ui| self.rail(ui));

        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(250.0)
            .min_size(140.0)
            .frame(egui::Frame::new().fill(theme::PANEL))
            .show(ui, |ui| {
                let view = self.active_view.clone();
                panels::sidebar(ui, self, &view);
            });

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
                .frame(egui::Frame::new().fill(theme::BG))
                .show(ui, |ui| panels::console_panel(ui, self));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::same(10)))
            .show(ui, |ui| {
                let view = self.active_view.clone();
                panels::center(ui, self, &view);
            });

        if let Some(project) = &self.project {
            let title = self
                .gameinfo
                .as_ref()
                .map(|i| i.window_title().to_string())
                .unwrap_or_else(|| project.name.clone());
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(format!(
                "deerpunk studio — {title}"
            )));
        }
    }
}

fn run_state_label(state: &RunState) -> String {
    match state {
        RunState::Idle => "idle".into(),
        RunState::Building => "building…".into(),
        RunState::Running => "running".into(),
        RunState::Stopping => "stopping…".into(),
        RunState::Failed(e) => format!("failed: {e}"),
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

fn conn_badge(ui: &mut egui::Ui, state: &ConnState) {
    let (text, color) = match state {
        ConnState::Connected => ("connected", theme::GREEN),
        ConnState::Connecting => ("connecting", theme::YELLOW),
        ConnState::Disconnected => ("disconnected", theme::MUTED),
        ConnState::Error(_) => ("error", theme::RED),
    };
    let fill =
        egui::Color32::from_rgba_unmultiplied_const(color.r(), color.g(), color.b(), 0x33);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(2.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(theme::mono_colored(text, color));
        });
}

pub const KINDS: [AssetKind; 18] = [
    AssetKind::GameInfo,
    AssetKind::StartupScript,
    AssetKind::Texture,
    AssetKind::Material,
    AssetKind::Shader,
    AssetKind::Prefab,
    AssetKind::Entity,
    AssetKind::Config,
    AssetKind::Map,
    AssetKind::MapRegion,
    AssetKind::DataPack,
    AssetKind::Sound,
    AssetKind::Ui,
    AssetKind::Font,
    AssetKind::Binary,
    AssetKind::Log,
    AssetKind::Doc,
    AssetKind::Other,
];

fn absolute(p: &std::path::Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}
