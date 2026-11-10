//! Application state and layout: Blender-style workspace tab strip (Files /
//! Inspector), docked central area, status bar with connection controls.
//! Live data comes from the engine inspector WS server (JSON-RPC over WS).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_dock::{DockArea, DockState, NodeIndex, TabViewer, TabStyle, TabBarStyle, Style as DockStyle};
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Tab {
    Project,
    Assets,
    Textures,
    Cvars,
    Raw,
    Console,
    Detail,
    Live(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workspace {
    Files,
    Inspector,
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
    pub console_lines: Vec<String>,
    pub inspector_built: bool,

    // Run instance state.
    pub profiles: Profiles,
    pub active_profile: String,
    pub runner: Runner,
    pub run_state: RunState,
    pub steps: Vec<Step>,
    pub console: Stream,
    pub input: String,

    // Workspaces + per-panel UI state.
    pub workspace: Workspace,
    pub files_dock: DockState<Tab>,
    pub inspector_dock: DockState<Tab>,
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
            console_lines: Vec::new(),
            inspector_built: false,
            profiles: Profiles::default(),
            active_profile: "dev".into(),
            runner: Runner::new(),
            run_state: RunState::Idle,
            steps: Vec::new(),
            console: Stream::new(5000),
            input: String::new(),
            workspace: Workspace::Files,
            files_dock: Self::build_files_dock(),
            inspector_dock: Self::build_inspector_dock(&[]),
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

    /// Files workspace: file-based project panels; right leaf holds Console.
    fn build_files_dock() -> DockState<Tab> {
        let mut dock = DockState::new(vec![
            Tab::Project,
            Tab::Assets,
            Tab::Textures,
            Tab::Cvars,
            Tab::Raw,
        ]);
        dock.main_surface_mut()
            .split_right(NodeIndex::root(), 0.42, vec![Tab::Console]);
        dock
    }

    /// Inspector workspace: collection lists left (42%), Detail + Console right.
    fn build_inspector_dock(collections: &[CollectionDescriptor]) -> DockState<Tab> {
        let left: Vec<Tab> = collections
            .iter()
            .map(|c| Tab::Live(c.id.clone()))
            .collect();
        let left = if left.is_empty() {
            vec![Tab::Live("entities".into())]
        } else {
            left
        };
        let mut dock = DockState::new(left);
        dock.main_surface_mut()
            .split_right(NodeIndex::root(), 0.58, vec![Tab::Detail, Tab::Console]);
        dock
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
                RunEvent::Console(bytes) => self.console.feed(&bytes),
                RunEvent::State(state) => self.run_state = state,
                RunEvent::Step(idx, state) => {
                    if let Some(step) = self.steps.get_mut(idx) {
                        step.state = state;
                    }
                }
                RunEvent::Note(text) => self.console.feed(format!("{text}\n").as_bytes()),
                RunEvent::Exited(_) => {}
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
                self.console_lines
                    .push(format!("[rpc error {code}] {message}"));
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
                            self.console_lines.push(format!(
                                "[ws] {} v{} — {} collections",
                                init.server,
                                init.server_version.as_deref().unwrap_or("?"),
                                init.collections.len()
                            ));
                            if !self.inspector_built {
                                self.inspector_dock = Self::build_inspector_dock(&self.collections);
                                self.inspector_built = true;
                            }
                            self.request_refresh();
                        }
                        Err(e) => self.console_lines.push(format!("[ws] initialize parse: {e}")),
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

    // ------------------------------------------------------------- layout

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let active = self.run_state != RunState::Idle;
        let fill = if active { theme::BAR_ACTIVE } else { theme::HEADER };
        egui::Frame::new()
            .fill(fill)
            .inner_margin(egui::Margin::symmetric(6, 3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    self.workspace_tab(ui, Workspace::Files, "Files");
                    self.workspace_tab(ui, Workspace::Inspector, "Inspector");
                    ui.separator();
                    if let Some(project) = &self.project {
                        ui.weak(&project.name);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Right-to-left order: Run/Stop, then profile, then file
                        // actions — so the profile dropdown sits left of Run.
                        if self.run_state == RunState::Idle {
                            if ui.button("▶ Run").clicked() {
                                self.run_instance();
                            }
                        } else {
                            if ui.button("■ Stop").clicked() {
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
                                    if ui.selectable_label(id == self.active_profile, &p.label).clicked() {
                                        self.active_profile = id;
                                    }
                                }
                            });
                        if ui.button("Open…").clicked() {
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
            });
        ui.separator();
    }

    /// Blender workspace tab: flat, active = lighter surface.
    fn workspace_tab(&mut self, ui: &mut egui::Ui, ws: Workspace, label: &str) {
        let active = self.workspace == ws;
        let (bg, color) = if active {
            (theme::FIELD, theme::TEXT)
        } else {
            (egui::Color32::TRANSPARENT, theme::TAB_INACTIVE)
        };
        let btn = egui::Button::new(egui::RichText::new(label).color(color).size(13.0))
            .fill(bg)
            .corner_radius(2.0)
            .min_size(egui::vec2(0.0, 22.0));
        if ui.add(btn).clicked() {
            self.workspace = ws;
        }
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(theme::HEADER)
            .inner_margin(egui::Margin::symmetric(8, 3))
            .show(ui, |ui| {
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
            });
    }

    fn dock_area(&mut self, ui: &mut egui::Ui) {
        match self.workspace {
            Workspace::Files => {
                let mut dock =
                    std::mem::replace(&mut self.files_dock, Self::build_files_dock());
                let mut viewer = AppTabViewer { app: self };
                DockArea::new(&mut dock)
                    .style(dock_style())
                    .show_inside(ui, &mut viewer);
                self.files_dock = dock;
            }
            Workspace::Inspector => {
                let mut dock = std::mem::replace(
                    &mut self.inspector_dock,
                    Self::build_inspector_dock(&[]),
                );
                let mut viewer = AppTabViewer { app: self };
                DockArea::new(&mut dock)
                    .style(dock_style())
                    .show_inside(ui, &mut viewer);
                self.inspector_dock = dock;
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

        self.top_bar(ui);

        let full = ui.available_rect_before_wrap();
        let bottom_h = 24.0;
        let central = egui::Rect::from_min_max(full.min, egui::pos2(full.max.x, full.max.y - bottom_h));
        let bottom =
            egui::Rect::from_min_max(egui::pos2(full.min.x, full.max.y - bottom_h), full.max);

        {
            let mut dock_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(central)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            self.dock_area(&mut dock_ui);
        }
        {
            let mut bottom_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(bottom)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            self.bottom_bar(&mut bottom_ui);
        }

        ui.allocate_rect(
            egui::Rect::from_min_max(ui.min_rect().min, full.max),
            egui::Sense::hover(),
        );
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

/// egui_dock chrome: flat Blender tab strip — inactive tabs sit on the header
/// color, the active one merges into the editor surface below.
fn dock_style() -> DockStyle {
    let mut style = DockStyle::from_egui(&egui::Style::default());
    style.dock_area_padding = Some(egui::Margin::ZERO);
    style.main_surface_border_stroke = egui::Stroke::NONE;

    style.separator = egui_dock::SeparatorStyle {
        width: 1.0,
        color_idle: theme::BORDER_DARK,
        color_hovered: theme::ACCENT,
        color_dragged: theme::ACCENT,
        ..Default::default()
    };

    style.tab_bar = TabBarStyle {
        bg_fill: theme::HEADER,
        height: 24.0,
        inner_margin: egui::Margin::ZERO,
        corner_radius: 0.0.into(),
        hline_color: theme::BORDER_DARK,
        ..Default::default()
    };

    style.tab = TabStyle {
        active: tab_interaction(theme::PANEL, theme::TEXT),
        inactive: tab_interaction(theme::HEADER, theme::TAB_INACTIVE),
        focused: tab_interaction(theme::PANEL, theme::TEXT),
        hovered: tab_interaction(theme::HOVER, theme::TEXT),
        ..Default::default()
    };
    style
}

fn tab_interaction(bg: egui::Color32, text: egui::Color32) -> egui_dock::TabInteractionStyle {
    egui_dock::TabInteractionStyle {
        outline_color: egui::Color32::TRANSPARENT,
        corner_radius: 0.0.into(),
        bg_fill: bg,
        text_color: text,
    }
}

struct AppTabViewer<'a> {
    app: &'a mut StudioApp,
}

impl<'a> TabViewer for AppTabViewer<'a> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        egui::Id::new(match tab {
            Tab::Project => "tab_project",
            Tab::Assets => "tab_assets",
            Tab::Textures => "tab_textures",
            Tab::Cvars => "tab_cvars",
            Tab::Raw => "tab_raw",
            Tab::Console => "tab_console",
            Tab::Detail => "tab_detail",
            Tab::Live(id) => return egui::Id::new(format!("tab_live_{id}")),
        })
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            Tab::Project => "Project".into(),
            Tab::Assets => "Assets".into(),
            Tab::Textures => "Textures".into(),
            Tab::Cvars => "Cvars".into(),
            Tab::Raw => "deerpunk.toml".into(),
            Tab::Console => "Console".into(),
            Tab::Detail => "Detail".into(),
            Tab::Live(id) => {
                let label = self
                    .app
                    .collections
                    .iter()
                    .find(|c| &c.id == id)
                    .map(|c| c.label.clone())
                    .unwrap_or_else(|| id.clone());
                label.into()
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        panels::render(ui, tab, self.app);
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
