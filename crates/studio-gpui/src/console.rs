//! Console panel (bottom dock): ANSI-colored runner output stream + command
//! input line. Ported from the egui console (src/panels.rs `console_panel` +
//! `render_console_line`, src/app.rs input handling). The stream supports
//! mouse selection (row/byte-col carets via text-layout hit-testing), an
//! accent-tinted highlight, cmd-a/cmd-c/escape hotkeys scoped to the console
//! focus context, and right-click Copy/Select All through `PopupMenu`.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::*;

use deerpunk_studio::ansi::{Span, Stream};
use deerpunk_studio::runner::{RunState, Step, StepState};

use crate::state::StudioState;

/// Console stream ring size (egui parity: `Stream::new(5000)`).
const CONSOLE_CAPACITY: usize = 5000;

/// Console stream text: Blender-console-level gray, readable without glaring
/// (egui `theme::CONSOLE_TEXT`).
const CONSOLE_TEXT: (u8, u8, u8) = (0xc8, 0xc8, 0xc8);
/// Brightness above which a near-neutral ANSI color softens to `CONSOLE_TEXT`
/// (white cargo lines should not glare).
const CONSOLE_BRIGHT_MAX: u8 = 230;
/// Max channel spread for an ANSI color to count as near-neutral.
const CONSOLE_NEUTRAL_SPREAD: u8 = 24;

const ROW_FONT_SIZE: f32 = 13.0; // egui monospace row height basis
const STRIP_FONT_SIZE: f32 = 11.0; // egui `theme::small`

actions!(
    console,
    [CopyConsole, SelectAllConsole, ClearConsoleSelection]
);

/// One-time keymap registration for the console hotkeys. Bindings are scoped
/// to the `Console` key context (the stream area below), so they apply only
/// while the stream is focused and never intercept `cmd-c`/`cmd-a` while the
/// command input is focused.
#[derive(Default)]
struct ConsoleBindingsInstalled;

impl Global for ConsoleBindingsInstalled {}

fn ensure_bindings(cx: &mut App) {
    if cx.has_global::<ConsoleBindingsInstalled>() {
        return;
    }
    cx.set_global(ConsoleBindingsInstalled);
    cx.bind_keys([
        KeyBinding::new("cmd-c", CopyConsole, Some("Console")),
        KeyBinding::new("cmd-a", SelectAllConsole, Some("Console")),
        KeyBinding::new("escape", ClearConsoleSelection, Some("Console")),
    ]);
}

/// A caret position inside the stream: row index + byte offset within the
/// row's plain text (spans concatenated, ANSI already parsed away).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelPoint {
    row: usize,
    col: usize,
}

/// Mouse (or cmd-a) selection; `anchor` is where the drag started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: SelPoint,
    head: SelPoint,
}

impl Selection {
    /// (start, end) in reading order regardless of drag direction.
    fn ordered(&self) -> (SelPoint, SelPoint) {
        if (self.anchor.row, self.anchor.col) <= (self.head.row, self.head.col) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

pub struct ConsolePanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    /// Single-line command input; created lazily in `render` (needs `Window`).
    input: Option<Entity<InputState>>,
    /// Hint currently installed on the input, so it is only re-set when the
    /// run state flips (a repeated `set_placeholder` would notify forever).
    input_hint: Option<&'static str>,
    scroll: UniformListScrollHandle,
    /// Total rows shown last frame. Growth + being at the bottom drives the
    /// auto-scroll (egui `ScrollArea::stick_to_bottom`).
    last_total: usize,
    /// Current text selection, in row/byte-col caret points.
    selection: Option<Selection>,
    /// Anchor of an in-progress press-drag; `None` outside a drag.
    drag_anchor: Option<SelPoint>,
    /// Viewport bounds of the stream area, captured at paint time by a canvas
    /// overlay — needed to turn window mouse positions into row/col carets.
    stream_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
}

impl ConsolePanel {
    pub fn new(state: Entity<StudioState>, cx: &mut Context<Self>) -> Self {
        ensure_bindings(cx);
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self {
            focus_handle: cx.focus_handle(),
            state,
            input: None,
            input_hint: None,
            scroll: UniformListScrollHandle::new(),
            last_total: 0,
            selection: None,
            drag_anchor: None,
            stream_bounds: Rc::new(RefCell::new(None)),
        }
    }

    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(""));
        cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Change => {
                        let value = input.read(cx).value().to_string();
                        this.state.update(cx, |s, _| s.input = value);
                    }
                    // Engine reads lines from stdin (egui: Enter on the TextEdit).
                    InputEvent::PressEnter { .. } => this.submit(input, window, cx),
                    _ => {}
                }
            },
        )
        .detach();
        self.input = Some(input.clone());
        input
    }

    /// egui parity: `enter && running && !input.trim().is_empty()` →
    /// `runner.input(trim)`, and only then clear the field.
    fn submit(&mut self, input: &Entity<InputState>, window: &mut Window, cx: &mut Context<Self>) {
        let text = input.read(cx).value().to_string();
        let trimmed = text.trim().to_string();
        let mut sent = false;
        if !trimmed.is_empty() {
            self.state.update(cx, |s, _| {
                if s.run_state == RunState::Running {
                    s.runner.input(trimmed);
                    s.input.clear();
                    sent = true;
                }
            });
        }
        if sent {
            input.update(cx, |st, cx| st.set_value("", window, cx));
        }
    }

    /// Row count shown by the stream list: finished lines + the pending one.
    fn total_rows(&self, cx: &App) -> usize {
        let s = self.state.read(cx);
        s.console.lines.len() + usize::from(!s.console.pending().is_empty())
    }

    /// Laid-out height of one stream row (from the last uniform_list frame).
    fn row_height(&self) -> Option<Pixels> {
        self.scroll
            .0
            .borrow()
            .last_item_size
            .map(|size| size.item.height)
            .filter(|h| *h > px(0.))
    }

    /// Plain (ANSI-stripped) text of one stream row; the pending tail counts
    /// as the row right after the last finished line.
    fn row_text(&self, row: usize, cx: &App) -> Option<String> {
        let s = self.state.read(cx);
        if row < s.console.lines.len() {
            Some(plain_text(&s.console.lines[row]))
        } else if row == s.console.lines.len() && !s.console.pending().is_empty() {
            Some(plain_text(s.console.pending()))
        } else {
            None
        }
    }

    fn mono_font(&self, cx: &App) -> Font {
        Font {
            family: cx.theme().mono_font_family.clone(),
            ..Font::default()
        }
    }

    /// Window position → caret point. Rows map through the scroll offset and
    /// row height; the column comes from shaping the row's text in the same
    /// mono font/size as the list and hit-testing the x position (byte index
    /// closest to the pointer, like a terminal caret).
    fn sel_point_at(&self, position: Point<Pixels>, window: &Window, cx: &App) -> Option<SelPoint> {
        let bounds = (*self.stream_bounds.borrow())?;
        let row_height = self.row_height()?;
        let total = self.total_rows(cx);
        if total == 0 {
            return None;
        }
        let offset = self.scroll.0.borrow().base_handle.offset();
        let local = position - bounds.origin;
        // offset.y <= 0 when scrolled down; content coord = viewport + offset.
        let content_y = local.y - offset.y;
        // Negative floats saturate to 0 in the `as usize` cast; clamp the tail.
        let row = ((content_y / row_height).floor() as usize).min(total - 1);
        let line = self.row_text(row, cx)?;
        let font = self.mono_font(cx);
        let layout = window.text_system().layout_line(
            &line,
            px(ROW_FONT_SIZE),
            &[TextRun {
                len: line.len(),
                font,
                color: hsl(CONSOLE_TEXT),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        Some(SelPoint {
            row,
            col: layout.closest_index_for_x(local.x),
        })
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Clicking the stream focuses the console so its key bindings apply.
        self.focus_handle.focus(window, cx);
        let Some(anchor) = self.sel_point_at(event.position, window, cx) else {
            return;
        };
        self.drag_anchor = Some(anchor);
        self.selection = None; // a plain click (no drag) clears the selection
        cx.notify();
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging() {
            return;
        }
        let Some(anchor) = self.drag_anchor else {
            return;
        };
        let Some(head) = self.sel_point_at(event.position, window, cx) else {
            return;
        };
        let next = Selection { anchor, head };
        if self.selection != Some(next) {
            self.selection = Some(next);
            cx.notify();
        }
    }

    fn handle_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        // A drag that never left its anchor was a click: nothing stays
        // selected.
        if self.drag_anchor.take().is_some()
            && self.selection.is_some_and(|sel| sel.anchor == sel.head)
        {
            self.selection = None;
            cx.notify();
        }
    }

    /// cmd-c: copy the selection as plain text. With no selection — a no-op
    /// (egui never dumped the whole buffer on a bare cmd-c either).
    fn copy_selection(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(sel) = self.selection else {
            return;
        };
        let (start, end) = sel.ordered();
        let lines: Vec<_> = (start.row..=end.row)
            .map(|row| self.row_text(row, cx))
            .collect();
        let text = selection_plain_text(&lines, start.col, end.col);
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// cmd-a: select everything currently visible in the viewport.
    fn select_all_visible(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(bounds) = *self.stream_bounds.borrow() else {
            return;
        };
        let Some(row_height) = self.row_height() else {
            return;
        };
        let total = self.total_rows(cx);
        if total == 0 {
            return;
        }
        let offset_y = self.scroll.0.borrow().base_handle.offset().y;
        let first = (((px(0.) - offset_y) / row_height).floor() as usize).min(total - 1);
        let visible = ((bounds.size.height / row_height).ceil() as usize).max(1);
        let last = (first + visible - 1).min(total - 1);
        let end_col = self.row_text(last, cx).map(|text| text.len()).unwrap_or(0);
        self.selection = Some(Selection {
            anchor: SelPoint { row: first, col: 0 },
            head: SelPoint {
                row: last,
                col: end_col,
            },
        });
        cx.notify();
    }
}

impl EventEmitter<PanelEvent> for ConsolePanel {}

impl Focusable for ConsolePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for ConsolePanel {
    fn panel_name(&self) -> &'static str {
        "ConsolePanel"
    }
}

impl Panel for ConsolePanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Console"
    }
}

impl Render for ConsolePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = self.ensure_input(window, cx);
        let mono_font = cx.theme().mono_font_family.clone();

        let running = {
            let s = self.state.read(cx);
            s.run_state == RunState::Running
        };

        // Keep the input hint in sync with the engine state (egui swapped it
        // live while typing).
        let hint = if running {
            "console command…"
        } else {
            "engine not running"
        };
        if self.input_hint != Some(hint) {
            if let Some(input) = self.input.clone() {
                input.update(cx, |st, cx| st.set_placeholder(hint, window, cx));
            }
            self.input_hint = Some(hint);
        }

        let steps: Vec<Step> = self.state.read(cx).steps.clone();
        let total = {
            let s = self.state.read(cx);
            s.console.lines.len() + usize::from(!s.console.pending().is_empty())
        };

        // egui `stick_to_bottom(true)`: follow the tail while the user is at
        // the bottom; once they scroll up, stop forcing until they return.
        if self.scroll.is_scrolled_to_end() != Some(false) && total > self.last_total {
            self.scroll.scroll_to_bottom();
        }
        self.last_total = total;

        let item_count = total.max(1);
        let selection = self.selection;
        let state = self.state.clone();
        let stream = uniform_list("console-stream", item_count, move |range, _window, cx| {
            let s = state.read(cx);
            let lines_len = s.console.lines.len();
            let pending = !s.console.pending().is_empty();
            range
                .map(|i| {
                    let line: &[Span] = if i < lines_len {
                        &s.console.lines[i]
                    } else if pending {
                        s.console.pending()
                    } else {
                        &[]
                    };
                    // Byte range of the selection within this row (unclamped;
                    // `console_line` clamps against the rendered text).
                    let sel_range = selection.and_then(|sel| {
                        let (start, end) = sel.ordered();
                        (i >= start.row && i <= end.row).then(|| {
                            let from = if i == start.row { start.col } else { 0 };
                            let to = if i == end.row { end.col } else { usize::MAX };
                            from..to
                        })
                    });
                    console_line(line, sel_range)
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        .h_full()
        .w_full()
        .font_family(mono_font.clone())
        .text_size(px(ROW_FONT_SIZE))
        .text_color(hsl(CONSOLE_TEXT));

        // Focus + key context live on the stream area only: cmd-c/cmd-a/escape
        // fire when the stream is clicked (and focused), never while the
        // command input below has focus. The invisible canvas just records the
        // viewport bounds at paint time for the mouse hit-testing above.
        let has_selection = selection.is_some();
        let menu_focus = self.focus_handle.clone();
        let stream_area = div()
            .id("console-stream-area")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .track_focus(&self.focus_handle)
            .key_context("Console")
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .on_action(cx.listener(|this, _: &CopyConsole, window, cx| {
                this.copy_selection(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllConsole, window, cx| {
                this.select_all_visible(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ClearConsoleSelection, _, cx| {
                if this.selection.take().is_some() {
                    cx.notify();
                }
            }))
            .child(stream)
            .child(
                canvas(
                    {
                        let bounds_cell = Rc::clone(&self.stream_bounds);
                        move |bounds, _, _| *bounds_cell.borrow_mut() = Some(bounds)
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .context_menu(move |menu, _, _| {
                menu.action_context(menu_focus.clone())
                    .menu_with_disabled("Copy", Box::new(CopyConsole), !has_selection)
                    .menu("Select All", Box::new(SelectAllConsole))
            });

        let mut panel = div().flex().flex_col().size_full().p_2().gap_1();

        // Pre_run step strip (only while steps exist, like egui).
        if !steps.is_empty() {
            let mut strip = div().flex().flex_wrap().items_center().gap_x_2().flex_1();
            for step in &steps {
                let (glyph, color) = match &step.state {
                    StepState::Pending => ("…", crate::theme::FAINT),
                    StepState::Running => ("▶", crate::theme::ACCENT),
                    StepState::Ok => ("✓", crate::theme::GREEN),
                    StepState::Failed(_) => ("✗", crate::theme::RED),
                };
                strip = strip
                    .child(label(glyph, STRIP_FONT_SIZE, color))
                    .child(label(
                        step.name.as_str(),
                        STRIP_FONT_SIZE,
                        crate::theme::TEXT_DIM,
                    ));
                if let StepState::Failed(e) = &step.state {
                    strip = strip.child(label(e.as_str(), STRIP_FONT_SIZE, crate::theme::RED));
                }
                strip = strip.child(label("·", STRIP_FONT_SIZE, crate::theme::TEXT_DIM));
            }
            let clear = div()
                .id("console-clear")
                .cursor_pointer()
                .hover(|s| s.text_color(crate::theme::TEXT))
                .child(label("clear", STRIP_FONT_SIZE, crate::theme::MUTED))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.state
                        .update(cx, |s, _| s.console = Stream::new(CONSOLE_CAPACITY));
                    this.last_total = 0;
                }));
            panel = panel
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(strip)
                        .child(clear),
                )
                .child(Separator::horizontal());
        }

        panel = panel
            .child(stream_area)
            .child(Separator::horizontal())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .font_family(mono_font)
                            .text_size(px(ROW_FONT_SIZE))
                            .text_color(crate::theme::MUTED)
                            .child(">"),
                    )
                    .child(Input::new(&input).flex_1().min_w_0()),
            );

        panel
    }
}

/// One stream row: spans → colored `StyledText`. Terminal behavior — nowrap,
/// clipped at the row slot (egui used fixed-height `show_rows` too). When
/// `selection` is set (byte range, unclamped), the selected slice gets an
/// accent-tinted background.
fn console_line(line: &[Span], selection: Option<Range<usize>>) -> AnyElement {
    let mut text = String::new();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for span in line {
        let start = text.len();
        text.push_str(&span.text);
        let end = text.len();
        let mut rgb = span
            .style
            .color
            .map(|(r, g, b)| console_soften(r, g, b))
            .unwrap_or(CONSOLE_TEXT);
        if span.style.dim {
            // Widen before multiplying: u8 * 6 overflows for components > 42.
            let dim = |v: u8| ((v as u16) * 6 / 10) as u8;
            rgb = (dim(rgb.0), dim(rgb.1), dim(rgb.2));
        }
        highlights.push((
            start..end,
            HighlightStyle {
                color: Some(hsl(rgb)),
                ..Default::default()
            },
        ));
    }
    if text.is_empty() {
        // egui rendered " " for empty lines to keep the row height.
        text.push(' ');
    }
    if let Some(range) = selection {
        let start = range.start.min(text.len());
        let end = range.end.min(text.len());
        if start < end {
            highlights.push((
                start..end,
                HighlightStyle {
                    background_color: Some(crate::theme::ACCENT.alpha(0.25)),
                    ..Default::default()
                },
            ));
        }
    }
    div()
        .whitespace_nowrap()
        .overflow_hidden()
        .child(StyledText::new(text).with_highlights(highlights))
        .into_any_element()
}

fn label(text: impl Into<SharedString>, size: f32, color: Hsla) -> Div {
    div()
        .text_size(px(size))
        .text_color(color)
        .child(text.into())
}

/// Near-neutral bright ANSI colors soften to `CONSOLE_TEXT`; genuinely colored
/// spans (green/cyan/yellow/red) pass through. Ported from egui `theme.rs`.
fn console_soften(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let max = r.max(g).max(b);
    let near_neutral = max.saturating_sub(r.min(g).min(b)) <= CONSOLE_NEUTRAL_SPREAD;
    if near_neutral && max > CONSOLE_BRIGHT_MAX {
        CONSOLE_TEXT
    } else {
        (r, g, b)
    }
}

fn hsl(c: (u8, u8, u8)) -> Hsla {
    rgb(((c.0 as u32) << 16) | ((c.1 as u32) << 8) | c.2 as u32).into()
}

/// ANSI-stripped text of a row (spans are already parsed; concatenation only).
fn plain_text(line: &[Span]) -> String {
    line.iter().map(|span| span.text.as_str()).collect()
}

/// Plain-text extraction for a selection: `lines` holds rows
/// `start_row..=end_row` in order (`None` = row pruned from the ring); the
/// first row is cut from `start_col`, the last through `end_col`, rows are
/// joined with `'\n'`. Pure so the copy path is unit-testable.
fn selection_plain_text(lines: &[Option<String>], start_col: usize, end_col: usize) -> String {
    let last = lines.len().saturating_sub(1);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(line) = line else {
            if i != last {
                out.push('\n');
            }
            continue;
        };
        let from = if i == 0 { start_col.min(line.len()) } else { 0 };
        let to = if i == last {
            end_col.min(line.len())
        } else {
            line.len()
        };
        if from < to {
            out.push_str(&line[from..to]);
        }
        if i != last {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{CONSOLE_TEXT, console_soften, plain_text, selection_plain_text};
    use crate::theme::ACCENT;
    use deerpunk_studio::ansi::{Span, TextStyle};

    #[test]
    fn console_soften_matches_egui() {
        // White cargo lines soften to the console gray.
        assert_eq!(console_soften(255, 255, 255), CONSOLE_TEXT);
        assert_eq!(console_soften(235, 235, 235), CONSOLE_TEXT);
        // Near-neutral boundary: spread 24 softens, 25 passes through.
        assert_eq!(console_soften(246, 222, 222), CONSOLE_TEXT);
        assert_eq!(console_soften(247, 222, 222), (247, 222, 222));
        // Not bright enough: untouched.
        assert_eq!(console_soften(230, 230, 220), (230, 230, 220));
        assert_eq!(console_soften(170, 170, 170), (170, 170, 170));
        // Genuinely colored spans pass through.
        assert_eq!(console_soften(67, 161, 67), (67, 161, 67));
        assert_eq!(console_soften(231, 106, 106), (231, 106, 106));
    }

    fn styled(text: &str) -> Span {
        Span {
            text: text.to_string(),
            style: TextStyle {
                color: Some((67, 161, 67)),
                bold: true,
                dim: false,
            },
        }
    }

    /// Selection extraction is plain text: spans concatenate without ANSI
    /// escapes, partial rows are sliced by byte columns, rows join with '\n'.
    #[test]
    fn selection_plain_text_extracts_clean_text() {
        // Two rows, each built from two styled spans; the extraction path is
        // `plain_text` (as in `row_text`) then `selection_plain_text`.
        let span_rows = [
            Some(vec![styled("warn"), styled(": engine up")]),
            None, // pruned ring row in the middle
            Some(vec![styled("done"), styled(" 100%")]),
        ];
        let rows: Vec<_> = span_rows
            .iter()
            .map(|row| row.as_deref().map(plain_text))
            .collect();

        // Full-row selection spanning everything.
        assert_eq!(
            selection_plain_text(&rows, 0, usize::MAX),
            "warn: engine up\n\ndone 100%"
        );
        // Partial first row + partial last row.
        assert_eq!(selection_plain_text(&rows, 6, 4), "engine up\n\ndone");
        // Single-row substring.
        let one = [rows[0].clone()];
        assert_eq!(selection_plain_text(&one, 6, 15), "engine up");
        // Columns clamp past the row end.
        assert_eq!(selection_plain_text(&one, 4, 999), ": engine up");
        // Selection of only pruned rows → empty (cmd-c no-op).
        let pruned = [None];
        assert_eq!(selection_plain_text(&pruned, 0, 5), "");
        // Selection background stays a translucent accent.
        assert_eq!(ACCENT.alpha(0.25).a, 0.25);
    }
}
