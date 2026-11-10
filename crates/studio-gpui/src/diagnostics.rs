//! Diagnostics panel (bottom dock): the studio's Problems view. The runner
//! builds with `--message-format=json` and pushes parsed rustc diagnostics
//! into the lib's [`deerpunk_studio::diagnostics::DIAG_SINK`]; this panel
//! drains the sink on a timer and lists them grouped by severity — errors
//! first, then warnings/notes — with Error/Warning filter chips. Expanding
//! a row shows rustc's rendered snippet in monospace; clicking a row opens
//! the file through the shared [`ViewerBus`] (when the integrator wires it
//! via [`DiagnosticsPanel::with_bus`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use deerpunk_studio::diagnostics::{Diag, DiagSeverity, drain_diags};

use crate::fa;
use crate::state::StudioState;
use crate::theme;
use crate::viewers::ViewerBus;

/// Drain cadence: diagnostics arrive in bursts during builds; a 250 ms poll
/// turns them into rows imperceptibly next to the build's own latency.
const DRAIN_INTERVAL: Duration = Duration::from_millis(250);

/// `triangle-exclamation` (f071, FA6 Free solid) — verified present in the
/// embedded font's cmap but not (yet) a `fa` module constant; kept local so
/// the shared icon table stays with its owner.
const TRIANGLE_EXCLAMATION: char = '\u{f071}';

pub struct DiagnosticsPanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    /// Shared file-open bus, when wired by the integrator (`with_bus`);
    /// a row click then opens the diagnostic's file. `new` leaves it off.
    bus: Option<WeakEntity<ViewerBus>>,
    /// Every diagnostic drained so far, append-only — indexes stay stable
    /// and key the expansion set.
    diags: Vec<Diag>,
    show_errors: bool,
    show_warnings: bool,
    expanded: HashSet<usize>,
}

impl DiagnosticsPanel {
    /// Integrator hook: pass the shared `Entity<ViewerBus>` (app.rs creates
    /// it for the Files/Viewer tabs) so clicking a diagnostic row opens the
    /// file; without it rows simply do not open anything.
    pub fn with_bus(
        state: Entity<StudioState>,
        bus: Option<Entity<ViewerBus>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = Self {
            focus_handle: cx.focus_handle(),
            state,
            bus: bus.map(|bus| bus.downgrade()),
            diags: Vec::new(),
            show_errors: true,
            show_warnings: true,
            expanded: HashSet::new(),
        };
        cx.spawn(async move |panel, cx| {
            loop {
                cx.background_executor().timer(DRAIN_INTERVAL).await;
                let Some(panel) = panel.upgrade() else {
                    break;
                };
                let fresh = drain_diags();
                if fresh.is_empty() {
                    continue;
                }
                let _ = panel.update(cx, |panel, cx| {
                    panel.diags.extend(fresh);
                    cx.notify();
                });
            }
        })
        .detach();
        panel
    }

    /// One list row: chevron, severity icon, `file:line` and the message;
    /// row click opens the file, chevron click expands the rendered snippet.
    fn row(&self, idx: usize, panel: &WeakEntity<Self>, cx: &App) -> AnyElement {
        let diag = &self.diags[idx];
        let (icon, color) = match diag.severity {
            DiagSeverity::Error => (fa::CIRCLE_EXCLAMATION, theme::RED),
            DiagSeverity::Warning => (TRIANGLE_EXCLAMATION, theme::YELLOW),
            DiagSeverity::Note => (TRIANGLE_EXCLAMATION, theme::MUTED),
        };
        let expanded = self.expanded.contains(&idx);
        let location = match (&diag.file, diag.line) {
            (Some(file), Some(line)) => Some(format!("{file}:{line}")),
            (Some(file), None) => Some(file.clone()),
            _ => None,
        };

        let chevron = div()
            .id(("diag-toggle", idx))
            .flex_none()
            .cursor_pointer()
            .child(chevron(expanded))
            .on_click({
                let panel = panel.clone();
                move |_, _, cx| {
                    cx.stop_propagation(); // not a file-open click
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |panel, cx| {
                            if panel.expanded.contains(&idx) {
                                panel.expanded.remove(&idx);
                            } else {
                                panel.expanded.insert(idx);
                            }
                            cx.notify();
                        });
                    }
                }
            });

        let open = h_flex()
            .items_start()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|s| s.bg(theme::HOVER))
            .child(chevron)
            .child(div().flex_none().text_color(color).child(fa::icon(icon)))
            .when_some(location, |row, location| {
                row.child(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(theme::TEXT_DIM)
                        .font_family(cx.theme().mono_font_family.clone())
                        .whitespace_nowrap()
                        .child(location),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_sm()
                    .text_color(theme::TEXT)
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(diag.message.clone()),
            )
            .when_some(diag.code.clone(), |row, code| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme::FAINT)
                        .whitespace_nowrap()
                        .child(code),
                )
            });

        // Row click → open through the bus (a no-op when not wired).
        let row = div()
            .id(("diag-row", idx))
            .on_click({
                let bus = self.bus.clone();
                let state = self.state.clone();
                let file = diag.file.clone();
                move |_, _, cx| {
                    let Some(path) = file
                        .as_deref()
                        .and_then(|file| resolve_file(file, state.read(cx)))
                    else {
                        return;
                    };
                    if let Some(bus) = bus.as_ref().and_then(|bus| bus.upgrade()) {
                        bus.update(cx, |bus, cx| bus.open(path, cx));
                    }
                }
            })
            .child(open);

        // rustc's rendered snippet, one `whitespace_nowrap` row per line
        // (this gpui Div has no pre-wrap style; clipping keeps long lines
        // from blowing out the dock width).
        let rendered_lines: Vec<String> = if expanded {
            diag.rendered
                .as_deref()
                .map(|r| r.lines().map(str::to_string).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut col = v_flex().child(row);
        if !rendered_lines.is_empty() {
            col =
                col.child(
                    div()
                        .mx_2()
                        .mb_1()
                        .px_2()
                        .py_1p5()
                        .rounded_sm()
                        .bg(theme::CARD)
                        .border_l_2()
                        .border_color(color)
                        .text_size(px(12.))
                        .text_color(theme::TEXT_DIM)
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(v_flex().gap_0().children(
                            rendered_lines.into_iter().map(|line| {
                                div().overflow_hidden().whitespace_nowrap().child(line)
                            }),
                        )),
                );
        }
        col.into_any_element()
    }
}

/// `triangle-exclamation`-style disclosure arrow from the gpui-kit icon set
/// (the fa table has no right/down chevron pair).
fn chevron(expanded: bool) -> Icon {
    Icon::new(if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .text_color(theme::MUTED)
    .size(px(13.))
}

/// rustc `file_name`s are relative to the compiled crate's workspace: the
/// gamelib build runs in the project root, the engine build in the engine
/// repo. Absolute paths pass through; relative ones resolve against the
/// engine dir first (unambiguous when it hits), then the project root, and
/// only when the file actually exists there — otherwise `None`, so a click
/// never opens a bogus path.
fn resolve_file(file: &str, state: &StudioState) -> Option<PathBuf> {
    let raw = Path::new(file);
    if raw.is_absolute() {
        return Some(raw.to_path_buf());
    }
    let project = state.project.as_ref()?;
    let in_engine = project.engine_dir.join(raw);
    if in_engine.is_file() {
        return Some(in_engine);
    }
    let in_project = project.root.join(raw);
    if in_project.is_file() {
        return Some(in_project);
    }
    None
}

/// Small group heading above a severity section.
fn section_label(text: String, color: Hsla) -> AnyElement {
    div()
        .px_2()
        .pt_2()
        .pb_1()
        .text_xs()
        .text_color(color)
        .child(text)
        .into_any_element()
}

impl EventEmitter<PanelEvent> for DiagnosticsPanel {}

impl Focusable for DiagnosticsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for DiagnosticsPanel {
    fn panel_name(&self) -> &'static str {
        "DiagnosticsPanel"
    }
}

impl Panel for DiagnosticsPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Diagnostics"
    }
}

impl Render for DiagnosticsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let error_idx: Vec<usize> = self
            .diags
            .iter()
            .enumerate()
            .filter(|(_, d)| d.severity == DiagSeverity::Error)
            .map(|(i, _)| i)
            .collect();
        let warning_idx: Vec<usize> = self
            .diags
            .iter()
            .enumerate()
            .filter(|(_, d)| d.severity != DiagSeverity::Error)
            .map(|(i, _)| i)
            .collect();
        let (error_count, warning_count) = (error_idx.len(), warning_idx.len());

        // Filter chips: active in the severity color, off in faint gray.
        let panel = cx.entity().downgrade();
        let errors_chip = Button::new("diag-filter-errors")
            .label(format!(
                "{} Ошибки {error_count}",
                fa::icon(fa::CIRCLE_EXCLAMATION)
            ))
            .custom(
                ButtonCustomVariant::new(cx).foreground(if self.show_errors {
                    theme::RED
                } else {
                    theme::FAINT
                }),
            )
            .on_click({
                let panel = panel.clone();
                move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |panel, cx| {
                            panel.show_errors = !panel.show_errors;
                            cx.notify();
                        });
                    }
                }
            });
        let warnings_chip = Button::new("diag-filter-warnings")
            .label(format!(
                "{} Предупреждения {warning_count}",
                fa::icon(TRIANGLE_EXCLAMATION)
            ))
            .custom(
                ButtonCustomVariant::new(cx).foreground(if self.show_warnings {
                    theme::YELLOW
                } else {
                    theme::FAINT
                }),
            )
            .on_click(move |_, _, cx| {
                if let Some(panel) = panel.upgrade() {
                    panel.update(cx, |panel, cx| {
                        panel.show_warnings = !panel.show_warnings;
                        cx.notify();
                    });
                }
            });

        let panel_handle = cx.entity().downgrade();
        let mut list = v_flex().gap_0p5();
        let mut shown = 0usize;
        if self.show_errors && error_count > 0 {
            list = list.child(section_label(format!("Ошибки — {error_count}"), theme::RED));
            for &i in &error_idx {
                list = list.child(self.row(i, &panel_handle, cx));
                shown += 1;
            }
        }
        if self.show_warnings && warning_count > 0 {
            list = list.child(section_label(
                format!("Предупреждения и заметки — {warning_count}"),
                theme::YELLOW,
            ));
            for &i in &warning_idx {
                list = list.child(self.row(i, &panel_handle, cx));
                shown += 1;
            }
        }

        let body = if shown == 0 {
            let (glyph_color, headline, hint) = if self.diags.is_empty() {
                (
                    theme::GREEN,
                    "Проблем нет",
                    "диагностика появится после сборки движка или геймлибов",
                )
            } else {
                (
                    theme::MUTED,
                    "Ничего не показано",
                    "включите нужные категории чипами фильтра",
                )
            };
            v_flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_color(glyph_color)
                        .child(fa::sized(fa::CHECK, px(24.))),
                )
                .child(div().text_sm().text_color(theme::TEXT_DIM).child(headline))
                .child(div().text_xs().text_color(theme::FAINT).child(hint))
                .into_any_element()
        } else {
            div()
                .id("diagnostics-scroll")
                .flex_1()
                .min_h_0()
                .overflow_scroll()
                .child(div().p_2().child(list))
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(theme::PANEL)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::BORDER)
                    .child(errors_chip)
                    .child(warnings_chip),
            )
            .child(body)
    }
}
