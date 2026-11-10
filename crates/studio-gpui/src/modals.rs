//! Modal dialogs — the gpui analog of the egui `gameinfo_modal`
//! (src/app.rs) and `mcp_modal` (src/panels.rs). Both open imperatively
//! from chrome clicks through [`WindowExt::open_dialog`]: the gameinfo
//! modal from the top bar identity block, the MCP modal from the statusbar
//! chip. The dialog surface ships the three close paths the egui modal
//! wired up by hand (× button, Escape, backdrop click) as defaults.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::{
    ActiveTheme as _, Disableable, Sizable, Size, WindowExt, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use deerpunk_studio::gameinfo::GameInfo;
use deerpunk_studio::mcp::local_ip;

use crate::state::StudioState;
use crate::{live_detail, theme};

// ── gameinfo ────────────────────────────────────────────────────────────

/// Open the gameinfo modal (egui `gameinfo_modal`): the shared live-detail
/// view renders the `gameinfo.txt` keys. The body view is built here — once
/// per open, not per dialog rebuild — because every [`live_detail::detail`]
/// call creates a fresh view entity whose expand state would reset.
pub fn open_gameinfo(state: &Entity<StudioState>, window: &mut Window, cx: &mut App) {
    let (title, body) = {
        let state = state.read(cx);
        let title = state
            .gameinfo
            .as_ref()
            .map(|info| info.window_title().to_string())
            .unwrap_or_else(|| "Game info".into());
        // Copy the inputs out of the `read` borrow first: `detail` needs
        // `&mut App`, which conflicts with the outstanding shared borrow —
        // so no closure here (a closure would capture `*cx` while `state`
        // is still borrowed).
        let body = match state.gameinfo.as_ref() {
            Some(info) => {
                let value = gameinfo_value(info);
                let prefabs = state.prefab_map.clone();
                Some(live_detail::detail(&value, &prefabs, window, cx))
            }
            None => None,
        };
        (title, body)
    };
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .w(px(420.))
            .title(SharedString::from(title.clone()))
            // No project / no gameinfo.txt: the egui modal said so in the
            // body (and still opened — the identity block is always
            // clickable).
            .when_some(body.clone(), |dialog, body| dialog.child(body))
            .when(body.is_none(), |dialog| {
                dialog.child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::MUTED)
                        .child("no gameinfo.txt"),
                )
            })
    });
}

/// The gameinfo keys as a JSON object for the shared detail renderer: the
/// eight known fields in the egui `gameinfo_modal` row order, then the
/// leftover `extras` sorted by key (same `kv_row` sequence as egui).
fn gameinfo_value(info: &GameInfo) -> Value {
    let mut extras: Vec<(&String, &String)> = info.extras.iter().collect();
    extras.sort();
    let mut rows = serde_json::Map::new();
    let mut insert = |key: &str, value: &str| {
        rows.insert(key.to_string(), Value::String(value.to_string()));
    };
    insert("gamefolder", &info.gamefolder);
    insert("title", &info.title);
    insert("basedir", &info.basedir);
    insert("icon", &info.icon);
    insert("version", &info.version);
    insert("startmap", &info.startmap);
    insert("trainmap", &info.trainmap);
    insert("gamedll", &info.gamedll);
    for (key, value) in extras {
        insert(key, value);
    }
    Value::Object(rows)
}

// ── MCP server ──────────────────────────────────────────────────────────

/// Open the MCP server modal (egui `mcp_modal` in src/panels.rs): server
/// status with version/uptime, Start/Stop/Restart, endpoints with copy
/// buttons, inspector link state, exposed tools and the agent hookup
/// snippets. The body entity is created before [`WindowExt::open_dialog`]
/// (the builder may re-run on layer rebuilds; keep entity creation out of
/// it).
pub fn open_mcp(state: &Entity<StudioState>, window: &mut Window, cx: &mut App) {
    let body = cx.new(|_| McpModal {
        state: state.clone(),
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.w(px(480.)).title("MCP server").child(body.clone())
    });
}

/// MCP modal body: a live view over the [`McpManager`]. The egui modal
/// re-read the manager every frame and kicked one throttled `/status`
/// request per frame; here the background pump notifies `StudioState`
/// every 50ms, which re-renders this view at the same cadence.
struct McpModal {
    state: Entity<StudioState>,
}

impl Render for McpModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.state.update(cx, |state, _| state.mcp.request_status());
        let running = self.state.update(cx, |state, _| state.mcp.is_running());

        let state_entity = self.state.clone();
        let state = state_entity.read(cx);
        let status = state.mcp.status.clone();
        let status_err = state.mcp.status_err.clone();
        let port = state.mcp.port();
        // The egui modal rebuilt this from host/port every frame; same here
        // (the render re-runs on every pump tick, so the buttons always
        // carry the current address).
        let inspector_addr = format!("{}:{}", state.host.trim(), state.port.trim());
        let mono = cx.theme().mono_font_family.clone();

        let (version_note, inspector, tools) = match &status {
            Some(st) => (
                Some(format!("v{} · up {:.0}s", st.version, st.uptime_secs)),
                st.inspector.clone(),
                Some(st.tools.clone()),
            ),
            None => (None, None, None),
        };

        // Status row: dot + running/stopped + version/uptime when known.
        let status_row = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .size(px(10.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(if running { theme::GREEN } else { theme::RED }),
            )
            .child(div().font_weight(FontWeight::BOLD).child(if running {
                "running"
            } else {
                "stopped"
            }))
            .when_some(version_note, |row, note| {
                row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::TEXT_DIM)
                        .child(note),
                )
            });

        // Start/Stop/Restart: egui disabled Start while running and Stop
        // while stopped; Restart always spawns a fresh child.
        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("mcp-start")
                    .ghost()
                    .label("Start")
                    .disabled(running)
                    .on_click({
                        let state = state_entity.clone();
                        let addr = inspector_addr.clone();
                        move |_, _, cx| state.update(cx, |state, _| state.mcp.restart(&addr))
                    }),
            )
            .child(
                Button::new("mcp-stop")
                    .ghost()
                    .label("Stop")
                    .disabled(!running)
                    .on_click({
                        let state = state_entity.clone();
                        move |_, _, cx| state.update(cx, |state, _| state.mcp.stop())
                    }),
            )
            .child(
                Button::new("mcp-restart")
                    .ghost()
                    .label("Restart")
                    .on_click({
                        let state = state_entity.clone();
                        let addr = inspector_addr.clone();
                        move |_, _, cx| state.update(cx, |state, _| state.mcp.restart(&addr))
                    }),
            );

        // Endpoints: loopback always, LAN when an interface address exists.
        let mut endpoints = v_flex().gap_1().child(heading("Endpoints")).child(copy_row(
            "local",
            &mono,
            format!("http://127.0.0.1:{port}/mcp"),
        ));
        if let Some(ip) = local_ip() {
            endpoints = endpoints.child(copy_row("LAN", &mono, format!("http://{ip}:{port}/mcp")));
        }

        // Inspector link: connected state when the server reports it, else
        // the last /status failure, else the pending note (egui order).
        let inspector_line = match &inspector {
            Some(ins) => div().text_size(px(13.)).child(format!(
                "{} — {}",
                if ins.connected {
                    "connected"
                } else {
                    "not connected"
                },
                ins.addr
            )),
            None => match &status_err {
                Some(err) => div()
                    .text_size(px(13.))
                    .text_color(theme::YELLOW)
                    .child(err.clone()),
                None => div()
                    .text_size(px(13.))
                    .text_color(theme::MUTED)
                    .child("no status yet…"),
            },
        };

        // Tools: name — description rows, or the egui empty/pending notes.
        let tools_section = v_flex().gap_1().child(heading("Tools")).child(match tools {
            Some(tools) if !tools.is_empty() => v_flex().gap_0p5().children(
                tools
                    .iter()
                    .map(|tool| {
                        div()
                            .text_size(px(11.))
                            .text_color(theme::TEXT_DIM)
                            .child(format!("{} — {}", tool.name, tool.description))
                    })
                    .collect::<Vec<_>>(),
            ),
            Some(_) => dim_line("no tools exposed"),
            None => dim_line("status pending…"),
        });

        // Agent hookup: the egui modal's read-only config + probe blocks,
        // each with its copy affordance.
        let ip = local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());
        let cfg = format!(
            "{{\n  \"mcpServers\": {{\n    \"deerpunk\": {{ \"url\": \"http://{ip}:{port}/mcp\" }}\n  }}\n}}"
        );
        let probe = format!(
            "curl -s -X POST http://127.0.0.1:{port}/mcp -H 'content-type: application/json' -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}}'"
        );
        let agent_section = v_flex()
            .gap_1()
            .child(heading("Connecting an agent"))
            .child(
                div()
                    .text_size(px(13.))
                    .child("Streamable HTTP MCP server. Client config:"),
            )
            .child(mono_block(&mono, cfg.clone()))
            .child(
                Clipboard::new("copy-config")
                    .value(cfg)
                    .with_size(Size::XSmall)
                    .tooltip("copy config"),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::TEXT_DIM)
                    .child("Generic probe:"),
            )
            .child(mono_block(&mono, probe.clone()))
            .child(
                Clipboard::new("copy-probe")
                    .value(probe)
                    .with_size(Size::XSmall)
                    .tooltip("copy probe"),
            );

        v_flex()
            .gap_3()
            .child(status_row)
            .child(actions)
            .child(separator())
            .child(endpoints)
            .child(separator())
            .child(
                v_flex()
                    .gap_1()
                    .child(heading("Inspector link"))
                    .child(inspector_line),
            )
            .child(separator())
            .child(tools_section)
            .child(separator())
            .child(agent_section)
    }
}

/// Section heading (egui `ui.strong(...)` in the MCP modal).
fn heading(text: &'static str) -> Div {
    div()
        .text_size(px(13.))
        .font_weight(FontWeight::BOLD)
        .child(text)
}

/// Dim placeholder line (egui `ui.weak(...)`).
fn dim_line(text: &'static str) -> Div {
    div()
        .text_size(px(13.))
        .text_color(theme::MUTED)
        .child(text)
}

/// Full-width hairline between modal sections (egui `ui.separator()`).
fn separator() -> Div {
    div().h(px(1.)).w_full().bg(theme::BORDER)
}

/// `label  value  [copy]` row (egui `copy_row` in src/panels.rs).
fn copy_row(label: &str, mono: &SharedString, value: String) -> Div {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .w(px(40.))
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(theme::TEXT_DIM)
                .child(label.to_string()),
        )
        .child(
            div()
                .font_family(mono.clone())
                .text_size(px(12.))
                .min_w_0()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(value.clone()),
        )
        .child(
            Clipboard::new(format!("copy-{label}"))
                .value(value)
                .with_size(Size::XSmall)
                .tooltip("copy"),
        )
}

/// Read-only preformatted block (egui non-interactive `TextEdit::multiline`).
/// This gpui fork has no `pre` whitespace mode, so each line becomes its own
/// div and JSON indentation is carried by non-breaking spaces — `whitespace:
/// normal` would otherwise collapse it. The copy button always copies the
/// raw text, so the substitution is display-only.
fn mono_block(mono: &SharedString, text: String) -> Div {
    let lines: Vec<_> = text
        .lines()
        .map(|line| {
            let indent = line.len() - line.trim_start().len();
            div()
                .font_family(mono.clone())
                .text_size(px(12.))
                .text_color(theme::TEXT)
                .child(format!("{}{}", "\u{a0}".repeat(indent), &line[indent..]))
        })
        .collect();
    div()
        .w_full()
        .min_w_0()
        .rounded(px(4.))
        .border_1()
        .border_color(theme::BORDER)
        .bg(theme::FIELD)
        .px_2()
        .py_1()
        .children(lines)
}
