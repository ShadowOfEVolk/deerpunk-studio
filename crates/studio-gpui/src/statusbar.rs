//! Bottom status strip (gpui analog of the egui `status_bar` in
//! src/app.rs): one compact inspector chip on the left — connection state
//! + refresh cadence, with a hover popover holding `host:port`, the
//! connect/disconnect actions and the refresh-cadence rows; run state, the
//! MCP server chip (click opens the MCP modal), studio error and the
//! asset-scan summary (hover popover with the gamedir detail) on the
//! right. Re-renders on `StudioState` notifications from the background
//! pump.

use std::time::Duration;

use gpui_kit::component::hover_card::{HoverCard, HoverCardState};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use deerpunk_studio::assets as studio_assets;
use deerpunk_studio::net::ConnState;
use deerpunk_studio::runner::RunState;

use crate::fa;
use crate::state::StudioState;
use crate::theme;

/// Icon size shared by every status-strip glyph (12–14px density band).
const ICON_SIZE: Pixels = px(12.);

/// Text action styled like the strip's label buttons: FA glyph + label.
fn text_action(
    id: &'static str,
    icon: char,
    label: &str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(SharedString::from(id)))
        .flex()
        .items_center()
        .gap_1()
        .h(px(20.))
        .px_1()
        .rounded(px(3.))
        .cursor_pointer()
        .text_size(px(11.))
        .text_color(theme::TEXT_DIM)
        .hover(|this| this.bg(theme::HOVER).text_color(theme::TEXT))
        .on_click(on_click)
        .child(fa::sized(icon, ICON_SIZE))
        .child(SharedString::from(label.to_string()))
}

/// Height of the strip (egui: `Panel::bottom("statusbar").exact_size(24.0)`).
const BAR_HEIGHT: f32 = 24.;
/// Refresh cadence options (egui `REFRESH_OPTIONS`, src/app.rs).
const REFRESH_OPTIONS: [u64; 5] = [0, 200, 250, 500, 1000];

pub struct StatusBar {
    state: Entity<StudioState>,
}

impl StatusBar {
    pub fn new(state: Entity<StudioState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self { state }
    }
}

pub(crate) fn conn_color(state: &ConnState) -> Hsla {
    match state {
        ConnState::Connected => theme::GREEN,
        ConnState::Connecting => theme::YELLOW,
        ConnState::Disconnected => theme::MUTED,
        ConnState::Error(_) => theme::RED,
    }
}

/// `color` at an explicit alpha (egui `tinted`, src/app.rs).
pub(crate) fn tinted(color: Hsla, alpha: f32) -> Hsla {
    hsla(color.h, color.s, color.l, alpha)
}

fn run_label(state: &RunState) -> String {
    match state {
        RunState::Idle => "idle".into(),
        RunState::Building => "building".into(),
        RunState::Running => "running".into(),
        RunState::Stopping => "stopping".into(),
        RunState::Failed(err) => format!("failed: {err}"),
    }
}

fn run_icon(state: &RunState) -> char {
    match state {
        RunState::Idle => fa::ACTIVITY,
        RunState::Building => fa::SPINNER,
        RunState::Running => fa::PLAY,
        RunState::Stopping => fa::STOP,
        RunState::Failed(_) => fa::CIRCLE_EXCLAMATION,
    }
}

fn run_color(state: &RunState) -> Hsla {
    match state {
        RunState::Idle => theme::MUTED,
        RunState::Building | RunState::Stopping => theme::YELLOW,
        RunState::Running => theme::GREEN,
        RunState::Failed(_) => theme::RED,
    }
}

fn refresh_label(refresh_ms: u64) -> String {
    if refresh_ms == 0 {
        "manual".into()
    } else {
        format!("{refresh_ms} ms")
    }
}

impl Render for StatusBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let conn = state.conn_state.clone();
        let run = state.run_state.clone();
        let refresh_ms = state.refresh_ms;
        let error = state.error.clone();

        // Right-zone facts, cloned out of the read before mutation below.
        let scan_files = state.scan.as_ref().map(|s| s.entries.len());
        let scan_bytes = state
            .scan
            .as_ref()
            .map(|s| s.entries.iter().map(|e| e.size).sum::<u64>());
        let scanning = state.scan_rx.is_some();
        let mcp_port = state.mcp.port();

        // `McpManager::is_running` reaps a dead child process, so it needs
        // `&mut` — the same per-frame check the egui rail did.
        let mcp_up = self.state.update(cx, |state, _| state.mcp.is_running());

        // ── left cluster: ONE inspector chip (hover popover) ────────────
        // The top bar badge + address + the inline connect/disconnect and
        // cadence controls used to be three strip residents; they all live
        // under this chip now.
        let inspector = HoverCard::new("inspector-chip")
            .anchor(Anchor::BottomLeft)
            .open_delay(Duration::from_millis(350))
            .close_delay(Duration::from_millis(250))
            .trigger(inspector_trigger(&conn, refresh_ms))
            .content({
                let state = self.state.clone();
                move |_, _, cx| inspector_card(&state, cx)
            });

        let left = h_flex().gap_2().items_center().child(inspector);

        // ── right cluster ───────────────────────────────────────────────
        // The chip opens the MCP server modal (egui's rail plug did the
        // same); no tooltip — gpui-component's tooltip-on-div API is
        // sealed, and the pointer/hover fill carry the affordance.
        let mcp = h_flex()
            .id("mcp-chip")
            .gap_1()
            .items_center()
            .px_1()
            .h(px(20.))
            .rounded(px(3.))
            .cursor_pointer()
            .text_color(if mcp_up {
                theme::TEXT_DIM
            } else {
                theme::FAINT
            })
            .hover(|this| this.bg(theme::HOVER).text_color(theme::TEXT))
            .on_click({
                let state = self.state.clone();
                move |_, window, cx| crate::modals::open_mcp(&state, window, cx)
            })
            .child(fa::sized(fa::PLUG, ICON_SIZE))
            .child(div().size(px(6.)).rounded_full().bg(if mcp_up {
                theme::GREEN
            } else {
                theme::MUTED
            }))
            .child(div().text_size(px(11.)).child(format!("mcp :{mcp_port}")));

        // Scan chip: the line carries only "N files · size" — gamedir path,
        // validity and walk errors live in the hover popover where they
        // have room to breathe.
        let scan_trigger = |files: usize, bytes: Option<u64>| -> Div {
            h_flex()
                .px_1()
                .h(px(20.))
                .items_center()
                .rounded(px(3.))
                .text_size(px(11.))
                .text_color(theme::TEXT_DIM)
                .child(match bytes {
                    Some(bytes) => format!("{files} files · {}", studio_assets::fmt_size(bytes)),
                    None => format!("{files} files"),
                })
        };

        let right = h_flex()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_color(run_color(&run))
                    .child(if matches!(run, RunState::Building | RunState::Stopping) {
                        fa::spinner(ICON_SIZE, run_color(&run), "run-spinner", true)
                    } else {
                        fa::sized(run_icon(&run), ICON_SIZE)
                    })
                    .child(
                        div()
                            .text_size(px(11.))
                            .whitespace_nowrap()
                            .child(run_label(&run)),
                    ),
            )
            .child(mcp)
            .when_some(error, |this, error| {
                this.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .text_color(theme::YELLOW)
                        .child(fa::sized(fa::CIRCLE_EXCLAMATION, ICON_SIZE))
                        .child(
                            div()
                                .max_w(px(320.))
                                .text_size(px(11.))
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(error),
                        ),
                )
            })
            .when_some(scan_files, |this, files| {
                this.child(
                    HoverCard::new("scan-chip")
                        .anchor(Anchor::BottomLeft)
                        .open_delay(Duration::from_millis(350))
                        .close_delay(Duration::from_millis(250))
                        .trigger(scan_trigger(files, scan_bytes))
                        .content({
                            let state = self.state.clone();
                            move |_, _, cx| scan_card(&state, cx)
                        }),
                )
            })
            .when(scanning, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .text_color(theme::TEXT_DIM)
                        .child(fa::spinner(
                            ICON_SIZE,
                            theme::TEXT_DIM,
                            "scan-spinner",
                            true,
                        ))
                        .child(div().text_size(px(11.)).child("scanning…")),
                )
            });

        h_flex()
            .h(px(BAR_HEIGHT))
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .px_2()
            .border_t_1()
            .border_color(theme::BORDER)
            .bg(theme::HEADER)
            .child(left)
            .child(right)
    }
}

/// The inspector chip: icon (link while connected, plug otherwise) plus a
/// "Inspector connected · cadence" label, tinted with the connection color.
/// Hovering it opens [`inspector_card`].
fn inspector_trigger(conn: &ConnState, refresh_ms: u64) -> Div {
    let color = conn_color(conn);
    let icon = if matches!(conn, ConnState::Connected) {
        fa::LINK
    } else {
        fa::PLUG
    };
    let label = match conn {
        ConnState::Connected => format!("Inspector connected · {}", refresh_label(refresh_ms)),
        ConnState::Connecting => "Inspector connecting".into(),
        ConnState::Disconnected => "Inspector disconnected".into(),
        ConnState::Error(_) => "Inspector error".into(),
    };
    h_flex()
        .gap_1()
        .items_center()
        .px_1()
        .h(px(20.))
        .rounded(px(3.))
        .text_size(px(11.))
        .text_color(color)
        .bg(tinted(color, 0.15))
        .hover(|this| this.bg(tinted(color, 0.3)))
        .cursor_pointer()
        .child(fa::sized(icon, ICON_SIZE))
        .child(label)
}

/// Inspector hover popover: `host:port` with the connection state, the
/// connect/disconnect actions and the refresh-cadence rows — the controls
/// that used to sit inline in the strip (egui `status_bar` left cluster).
fn inspector_card(state: &Entity<StudioState>, cx: &Context<HoverCardState>) -> Div {
    let (conn, host, port, refresh_ms) = {
        let s = state.read(cx);
        (
            s.conn_state.clone(),
            s.host.clone(),
            s.port.clone(),
            s.refresh_ms,
        )
    };
    let connect = {
        let state = state.clone();
        text_action("inspector-connect", fa::PLUG, "connect", move |_, _, cx| {
            state.update(cx, |state, cx| {
                state.connect();
                cx.notify();
            });
        })
    };
    let disconnect = {
        let state = state.clone();
        text_action(
            "inspector-disconnect",
            fa::PLUG_CIRCLE_XMARK,
            "disconnect",
            move |_, _, cx| {
                state.update(cx, |state, cx| {
                    state.disconnect();
                    cx.notify();
                });
            },
        )
    };
    v_flex()
        .gap_2()
        .min_w(px(190.))
        .child(
            h_flex()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::BOLD)
                        .child(format!("{host}:{port}")),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(conn_color(&conn))
                        .child(match &conn {
                            ConnState::Connected => "connected",
                            ConnState::Connecting => "connecting…",
                            ConnState::Disconnected => "disconnected",
                            ConnState::Error(_) => "error",
                        }),
                ),
        )
        .child(h_flex().gap_1().child(connect).child(disconnect))
        .child(div().h(px(1.)).w_full().bg(theme::BORDER))
        .child(refresh_rows(state, refresh_ms))
}

/// Refresh-cadence rows; the current option is checked. Picking a row
/// stores it in `StudioState::refresh_ms` — the hover card stays open, so
/// the check mark moves on the next strip re-render (the pump ticks every
/// 50ms).
fn refresh_rows(state: &Entity<StudioState>, current: u64) -> Div {
    let rows: Vec<_> = REFRESH_OPTIONS
        .iter()
        .map(|opt| {
            let checked = *opt == current;
            let state = state.clone();
            let opt = *opt;
            h_flex()
                .id(ElementId::Name(SharedString::from(format!(
                    "refresh-{opt}"
                ))))
                .h(px(24.))
                .px_2()
                .gap_2()
                .items_center()
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(if checked {
                    theme::TEXT
                } else {
                    theme::TEXT_DIM
                })
                .hover(|this| this.bg(theme::HOVER))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    state.update(cx, |state, cx| {
                        state.refresh_ms = opt;
                        cx.notify();
                    });
                })
                .when(checked, |this| {
                    this.child(
                        div()
                            .text_color(theme::ACCENT)
                            .child(fa::sized(fa::CHECK, px(12.))),
                    )
                })
                .child(refresh_label(opt))
        })
        .collect();

    v_flex()
        .gap_0p5()
        .child(
            div()
                .px_2()
                .text_size(px(11.))
                .text_color(theme::TEXT_DIM)
                .child("refresh"),
        )
        .children(rows)
}

/// Scan hover popover: gamedir path, validity and walk errors — the facts
/// dropped from the strip line.
fn scan_card(state: &Entity<StudioState>, cx: &Context<HoverCardState>) -> Div {
    let mono = cx.theme().mono_font_family.clone();
    let (gamedir, gamedir_ok, walk_errors) = {
        let s = state.read(cx);
        (
            s.project.as_ref().map(|p| p.gamedir.display().to_string()),
            s.project.as_ref().map(|p| p.gamedir_valid()),
            s.scan.as_ref().map(|scan| scan.errors.len()),
        )
    };
    v_flex()
        .gap_1()
        .min_w(px(240.))
        .child(match gamedir {
            Some(dir) => div().font_family(mono).text_size(px(11.)).child(dir),
            None => div()
                .text_size(px(11.))
                .text_color(theme::MUTED)
                .child("no project"),
        })
        .when_some(gamedir_ok, |this, ok| {
            this.child(
                div()
                    .text_size(px(11.))
                    .text_color(if ok { theme::GREEN } else { theme::RED })
                    .child(if ok { "gamedir ok" } else { "gamedir missing" }),
            )
        })
        .when(walk_errors.is_some_and(|errors| errors > 0), |this| {
            this.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::YELLOW)
                    .child(format!("{} walk errors", walk_errors.unwrap_or(0))),
            )
        })
}
