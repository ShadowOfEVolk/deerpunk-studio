//! Application state and layout: top bar, dock (collection lists + detail +
//! file panels), status bar with connection controls. Live data comes from the
//! engine inspector WS server (JSON-RPC, collections discovered at handshake).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_dock::{DockArea, DockState, NodeIndex};
use serde_json::Value;

use crate::assets::{self, AssetKind, ScanResult};
use crate::config::StudioConfig;
use crate::gameinfo::GameInfo;
use crate::net::{ConnState, NetClient, NetCmd, NetEvent};
use crate::panels;
use crate::project::Project;
use crate::protocol::{self, CollectionDescriptor};
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
    pub dock_live_built: bool,

    // Dock + per-panel UI state.
    pub dock: DockState<Tab>,
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
            dock_live_built: false,
            dock: Self::build_dock(&[]),
            assets_filter: String::new(),
            assets_kind: None,
            textures_filter: String::new(),
            cvars_filter: String::new(),
            filters: HashMap::new(),
        };
        if let Some(path) = args.first() {
            app.open_project(PathBuf::from(path));
        } else if let Some(last) = app.cfg.last_project.clone() {
            app.open_project(PathBuf::from(last));
        }
        app
    }

    /// Left leaf: collection lists + file panels; right leaf: detail (42/58
    /// split like the inspector's Allotment default).
    fn build_dock(collections: &[CollectionDescriptor]) -> DockState<Tab> {
        let mut left: Vec<Tab> = collections
            .iter()
            .filter(|c| c.group == protocol::CollectionGroup::Runtime)
            .map(|c| Tab::Live(c.id.clone()))
            .collect();
        left.extend([
            Tab::Project,
            Tab::Assets,
            Tab::Textures,
            Tab::Cvars,
            Tab::Raw,
            Tab::Console,
        ]);
        let mut dock = DockState::new(left);
        dock.main_surface_mut()
            .split_right(NodeIndex::root(), 0.58, vec![Tab::Detail]);
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
                self.console_lines.clear();
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
        self.dock_live_built = false;
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
                            self.dock = Self::build_dock(&self.collections);
                            self.dock_live_built = true;
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
        egui::Frame::new()
            .fill(theme::HEADER)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("deerpunk studio")
                            .strong()
                            .color(theme::ACCENT_TEXT),
                    );
                    if let Some(project) = &self.project {
                        ui.separator();
                        ui.weak(&project.name);
                    }
                    if ui.button("Open…").clicked() {
                        self.pick_folder();
                    }
                    if !self.cfg.recents.is_empty() {
                        let current = self
                            .cfg
                            .recents
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "Recents".into());
                        egui::ComboBox::from_id_salt("recents")
                            .selected_text(current)
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
        ui.separator();
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(theme::HEADER)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    conn_badge(ui, &self.conn_state);
                    if matches!(self.conn_state, ConnState::Connected | ConnState::Error(_)) {
                        ui.weak(format!("{}:{}", self.host, self.port));
                    }
                    if ui.button("connect").clicked() {
                        self.connect();
                    }
                    if ui.button("disconnect").clicked() {
                        self.disconnect();
                    }
                    egui::ComboBox::from_id_salt("refresh_ms")
                        .selected_text(if self.refresh_ms == 0 {
                            "manual".to_string()
                        } else {
                            format!("{} ms", self.refresh_ms)
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
                    if ui.button("refresh").clicked() {
                        self.request_refresh();
                    }
                    if ui.button("reconnect").clicked() {
                        self.reconnect();
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(err) = &self.error {
                            ui.colored_label(theme::YELLOW, format!("⚠ {err}"));
                        }
                        if let (Some(scan), Some(project)) = (&self.scan, &self.project) {
                            let bytes: u64 = scan.entries.iter().map(|e| e.size).sum();
                            if !scan.errors.is_empty() {
                                ui.colored_label(
                                    theme::YELLOW,
                                    format!("{} walk errors", scan.errors.len()),
                                );
                            }
                            ui.weak(format!(
                                "{} files · {} · gamedir {}",
                                scan.entries.len(),
                                assets::fmt_size(bytes),
                                if project.gamedir_valid() { "ok" } else { "missing" }
                            ));
                        } else if self.scan_rx.is_some() {
                            ui.weak("scanning…");
                        }
                    });
                });
            });
    }

    fn dock_area(&mut self, ui: &mut egui::Ui) {
        let mut dock = std::mem::replace(&mut self.dock, DockState::new(vec![Tab::Detail]));
        let mut viewer = AppTabViewer { app: self };
        DockArea::new(&mut dock)
            .show_close_buttons(true)
            .show_inside(ui, &mut viewer);
        self.dock = dock;
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
        let dropped = ui
            .ctx()
            .input(|i| i.raw.dropped_files.clone());
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
        self.handle_dropped_files(ui);

        self.top_bar(ui);

        let full = ui.available_rect_before_wrap();
        let bottom_h = 28.0;
        let central = egui::Rect::from_min_max(
            full.min,
            egui::pos2(full.max.x, full.max.y - bottom_h),
        );
        let bottom = egui::Rect::from_min_max(
            egui::pos2(full.min.x, full.max.y - bottom_h),
            full.max,
        );

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

        ui.allocate_rect(egui::Rect::from_min_max(ui.min_rect().min, full.max), egui::Sense::hover());
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

struct AppTabViewer<'a> {
    app: &'a mut StudioApp,
}

impl<'a> egui_dock::TabViewer for AppTabViewer<'a> {
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
        if let Tab::Detail = tab {
            panels::detail_panel(ui, self.app);
            return;
        }
        panels::render(ui, tab, self.app);
    }
}

fn conn_badge(ui: &mut egui::Ui, state: &ConnState) {
    let (text, color) = match state {
        ConnState::Connected => ("connected", theme::GREEN),
        ConnState::Connecting => ("connecting", theme::YELLOW),
        ConnState::Disconnected => ("disconnected", theme::MUTED),
        ConnState::Error(_) => ("error", theme::RED),
    };
    let fill = egui::Color32::from_rgba_unmultiplied_const(color.r(), color.g(), color.b(), 0x33);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(4.0)
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
