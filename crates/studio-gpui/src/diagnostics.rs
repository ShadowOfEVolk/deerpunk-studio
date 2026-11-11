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
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use deerpunk_studio::diagnostics::{Diag, DiagSeverity, drain_diags};
use deerpunk_studio::runner::RunState;

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

/// Uniform slot height for every list row — `uniform_list` lays all rows
/// out with one measured height. 24px fits the headline line and centers a
/// 12px snippet line in its slot.
const DIAG_ROW_H: f32 = 24.;

/// One visible row of the flattened diagnostics list.
#[derive(Clone)]
struct DiagRow {
    kind: DiagRowKind,
}

#[derive(Clone)]
enum DiagRowKind {
    /// Severity section heading above its group (the old `section_label`).
    Section { text: String, color: Hsla },
    /// The diagnostic headline: chevron, severity icon, `file:line`,
    /// message, code — click opens the file, chevron toggles the snippet.
    Item {
        idx: usize,
        icon: char,
        color: Hsla,
        location: Option<String>,
        message: String,
        code: Option<String>,
        file: Option<String>,
        expanded: bool,
    },
    /// One `whitespace_nowrap` line of the rustc-rendered snippet, boxed
    /// with the severity-colored left border (the old per-diagnostic box,
    /// line-per-row).
    Snippet { color: Hsla, line: String },
}

pub struct DiagnosticsPanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    /// Shared file-open bus, when wired by the integrator (`with_bus`);
    /// a row click then opens the diagnostic's file. `new` leaves it off.
    bus: Option<WeakEntity<ViewerBus>>,
    /// Every diagnostic drained so far, append-only — indexes stay stable
    /// and key the expansion set. Cleared when a new build starts (the
    /// observer below catches the run-state transition).
    diags: Vec<Diag>,
    show_errors: bool,
    show_warnings: bool,
    expanded: HashSet<usize>,
    /// Last seen "the runner is building" marker: only the Idle/other →
    /// Building transition resets the buffer, repeated Building frames
    /// (the state pump notifies on every tick) are no-ops.
    building_seen: bool,
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
            state: state.clone(),
            bus: bus.map(|bus| bus.downgrade()),
            diags: Vec::new(),
            show_errors: true,
            show_warnings: true,
            expanded: HashSet::new(),
            building_seen: false,
        };
        // A new build wipes the previous run's diagnostics: the first
        // transition into `RunState::Building` (the state pump notifies on
        // every tick, so observers see each run-state change) clears the
        // buffer before the new build's output reaches the drain loop.
        cx.observe(&state, |panel, state, cx| {
            let run = state.read(cx).run_state.clone();
            if reset_on_build_start(
                &mut panel.diags,
                &mut panel.expanded,
                &mut panel.building_seen,
                &run,
            ) {
                cx.notify();
            }
        })
        .detach();
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

    /// Test seam (headless perf/behavior tests in `tests/ui.rs` mirror the
    /// module tree, so the panel's private buffer needs a cfg(test) way in):
    /// append already-parsed diagnostics as if drained from the sink.
    #[cfg(test)]
    pub fn push_for_test(&mut self, fresh: Vec<Diag>) {
        self.diags.extend(fresh);
    }

    /// Test seam: expand/collapse one diagnostic (the chevron-click body —
    /// click dispatch needs the kit's test-support, which this crate
    /// deliberately does not enable).
    #[cfg(test)]
    pub fn set_expanded_for_test(&mut self, idx: usize, on: bool) {
        if on {
            self.expanded.insert(idx);
        } else {
            self.expanded.remove(&idx);
        }
    }

    /// The visible rows: filter chips pick the severities (each rendered
    /// section only when non-empty), expanded diagnostics contribute one row
    /// per rendered snippet line. `uniform_list` lays out only the on-screen
    /// range, so a 500-row build costs no more per frame than an empty one.
    fn flatten(&self) -> Vec<DiagRow> {
        let mut rows = Vec::new();
        if self.show_errors {
            let idxs: Vec<usize> = self
                .diags
                .iter()
                .enumerate()
                .filter(|(_, d)| d.severity == DiagSeverity::Error)
                .map(|(i, _)| i)
                .collect();
            if !idxs.is_empty() {
                rows.push(DiagRow {
                    kind: DiagRowKind::Section {
                        text: format!("Ошибки — {}", idxs.len()),
                        color: theme::RED,
                    },
                });
                for i in idxs {
                    self.push_diag_rows(i, &mut rows);
                }
            }
        }
        if self.show_warnings {
            let idxs: Vec<usize> = self
                .diags
                .iter()
                .enumerate()
                .filter(|(_, d)| d.severity != DiagSeverity::Error)
                .map(|(i, _)| i)
                .collect();
            if !idxs.is_empty() {
                rows.push(DiagRow {
                    kind: DiagRowKind::Section {
                        text: format!("Предупреждения и заметки — {}", idxs.len()),
                        color: theme::YELLOW,
                    },
                });
                for i in idxs {
                    self.push_diag_rows(i, &mut rows);
                }
            }
        }
        rows
    }

    /// One diagnostic's rows: the headline, plus — when expanded — one row
    /// per line of rustc's rendered snippet.
    fn push_diag_rows(&self, idx: usize, rows: &mut Vec<DiagRow>) {
        let diag = &self.diags[idx];
        let (icon, color) = severity_icon(diag.severity);
        let location = match (&diag.file, diag.line) {
            (Some(file), Some(line)) => Some(format!("{file}:{line}")),
            (Some(file), None) => Some(file.clone()),
            _ => None,
        };
        let expanded = self.expanded.contains(&idx);
        rows.push(DiagRow {
            kind: DiagRowKind::Item {
                idx,
                icon,
                color,
                location,
                message: diag.message.clone(),
                code: diag.code.clone(),
                file: diag.file.clone(),
                expanded,
            },
        });
        if expanded {
            if let Some(rendered) = diag.rendered.as_deref() {
                for line in rendered.lines() {
                    rows.push(DiagRow {
                        kind: DiagRowKind::Snippet {
                            color,
                            line: line.to_string(),
                        },
                    });
                }
            }
        }
    }
}

/// Severity icon + color (the old `row()` palette).
fn severity_icon(severity: DiagSeverity) -> (char, Hsla) {
    match severity {
        DiagSeverity::Error => (fa::CIRCLE_EXCLAMATION, theme::RED),
        DiagSeverity::Warning => (TRIANGLE_EXCLAMATION, theme::YELLOW),
        DiagSeverity::Note => (TRIANGLE_EXCLAMATION, theme::MUTED),
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
        let error_count = self
            .diags
            .iter()
            .filter(|d| d.severity == DiagSeverity::Error)
            .count();
        let warning_count = self.diags.len() - error_count;

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

        // The virtualized row list: sections + headlines + expanded snippet
        // lines, flattened once per render, laid out on-screen only.
        let rows = Arc::new(self.flatten());
        let empty = rows.is_empty();
        let panel_handle = cx.entity().downgrade();
        let bus = self.bus.clone();
        let state = self.state.clone();
        let list = uniform_list("diag-rows", rows.len().max(1), move |range, _, cx| {
            range
                .map(|ix| match rows.get(ix) {
                    Some(row) => render_diag_row(row, &panel_handle, &bus, &state, cx),
                    // Keeps the measured slot when the list is empty.
                    None => div().h(px(DIAG_ROW_H)).into_any_element(),
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0();

        let body = if empty {
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
            list.into_any_element()
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

/// Element for one visible row (only the on-screen range is built). Headline
/// rows keep the old interaction shape — row click opens the file through
/// the bus, chevron click toggles the snippet (propagation stopped so a
/// chevron click never opens).
fn render_diag_row(
    row: &DiagRow,
    panel: &WeakEntity<DiagnosticsPanel>,
    bus: &Option<WeakEntity<ViewerBus>>,
    state: &Entity<StudioState>,
    cx: &App,
) -> AnyElement {
    match &row.kind {
        DiagRowKind::Section { text, color } => div()
            .h(px(DIAG_ROW_H))
            .w_full()
            .flex()
            .items_center()
            .px_2()
            .text_xs()
            .text_color(*color)
            .child(text.clone())
            .into_any_element(),
        DiagRowKind::Item {
            idx,
            icon,
            color,
            location,
            message,
            code,
            file,
            expanded,
        } => {
            let idx = *idx;
            let chevron = div()
                .id(("diag-toggle", idx))
                .flex_none()
                .cursor_pointer()
                .child(chevron(*expanded))
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

            let line = h_flex()
                .h(px(DIAG_ROW_H))
                .w_full()
                .items_center()
                .gap_2()
                .px_2()
                .rounded_sm()
                .cursor_pointer()
                .hover(|s| s.bg(theme::HOVER))
                .child(chevron)
                .child(div().flex_none().text_color(*color).child(fa::icon(*icon)))
                .when_some(location.clone(), |row, location| {
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
                        .child(message.clone()),
                )
                .when_some(code.clone(), |row, code| {
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
            line.id(("diag-row", idx))
                .on_click({
                    let bus = bus.clone();
                    let state = state.clone();
                    let file = file.clone();
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
                .into_any_element()
        }
        // rustc's rendered snippet line: severity-colored left border over
        // the CARD fill, clipped to one `whitespace_nowrap` line (this gpui
        // Div has no pre-wrap style; clipping keeps long lines from blowing
        // out the dock width).
        DiagRowKind::Snippet { color, line } => div()
            .h(px(DIAG_ROW_H))
            .w_full()
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .mx_2()
                    .pl_2()
                    .h_full()
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .bg(theme::CARD)
                    .border_l_2()
                    .border_color(*color)
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(px(12.))
                            .text_color(theme::TEXT_DIM)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(line.clone()),
                    ),
            )
            .into_any_element(),
    }
}

/// The first transition into `RunState::Building` starts a new build: the
/// accumulated diagnostics belong to the previous run and are wiped along
/// with the expansions (indices restart with the new build). Repeat
/// Building frames are no-ops; any other state (Running/Idle/Failed/
/// Stopping) only clears the marker. Free function so the buffer rule is
/// unit-testable without a gpui context; returns whether it cleared.
fn reset_on_build_start(
    diags: &mut Vec<Diag>,
    expanded: &mut HashSet<usize>,
    building_seen: &mut bool,
    run_state: &RunState,
) -> bool {
    if matches!(run_state, RunState::Building) {
        if *building_seen {
            return false;
        }
        *building_seen = true;
        diags.clear();
        expanded.clear();
        return true;
    }
    *building_seen = false;
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(severity: DiagSeverity, message: &str) -> Diag {
        Diag {
            severity,
            message: message.to_string(),
            rendered: None,
            file: None,
            line: None,
            col: None,
            code: None,
        }
    }

    /// Two builds in a row: after the second start none of the first run's
    /// diagnostics remain, and the new run's output survives repeat
    /// Building frames (the state pump notifies on every tick).
    // `gpui_kit::*` (test-support on) re-exports gpui's `test` proc macro,
    // which shadows the builtin — spell the builtin out (same reason
    // tests/ui.rs imports Kit types explicitly).
    #[::core::prelude::v1::test]
    fn reset_on_build_start_clears_previous_build() {
        let mut diags = vec![
            diag(DiagSeverity::Error, "old error"),
            diag(DiagSeverity::Warning, "old warning"),
        ];
        let mut expanded = HashSet::from([0]);
        let mut seen = false;

        // First build start: reset + marker.
        assert!(reset_on_build_start(
            &mut diags,
            &mut expanded,
            &mut seen,
            &RunState::Building
        ));
        assert!(diags.is_empty());
        assert!(expanded.is_empty());
        assert!(seen);

        // The new build's diagnostics arrive and must survive further
        // Building frames.
        diags.push(diag(DiagSeverity::Error, "new error"));
        expanded.insert(0);
        assert!(!reset_on_build_start(
            &mut diags,
            &mut expanded,
            &mut seen,
            &RunState::Building
        ));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "new error");

        // The run finishes (marker clears), then a new build resets again.
        assert!(!reset_on_build_start(
            &mut diags,
            &mut expanded,
            &mut seen,
            &RunState::Running
        ));
        assert!(!seen);
        assert!(reset_on_build_start(
            &mut diags,
            &mut expanded,
            &mut seen,
            &RunState::Building
        ));
        assert!(diags.is_empty());
        assert!(expanded.is_empty());
    }
}
