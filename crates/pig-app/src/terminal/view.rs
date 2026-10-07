//! TerminalView: the gpui Entity for one terminal session.
//!
//! Skeleton based on tty7 `src/terminal/view.rs` (~700 lines taken out of
//! 18109): holds the Term wrapper plus a FocusHandle; a cx.spawn event pump
//! drains alacritty events in batches then cx.notify; the root div does
//! track_focus plus on_key_down plus on_scroll_wheel; the scroll wheel drives
//! display_offset; mouse drag selection; Cmd chord interception; 530ms cursor
//! blink. Dropped: search/history/completion/composer/agent detection/SSH/mouse
//! reporting (mouse reporting left as a TODO).

use alacritty_terminal::event::{Event as AlacEvent, WindowSize};
use alacritty_terminal::grid::{Dimensions as _, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use gpui_kit::{
    App, ClipboardItem, Context, EventEmitter, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Render, ScrollDelta,
    ScrollWheelEvent, Styled as _, Subscription, Window, div, px,
};

use super::colors::{self, TermColors};
use super::element::{GridSnapshot, RenderCell, TerminalElement};
use super::input::{self, KeyFlags};
use super::term::{TermSize, Terminal};

/// Padding around the grid (same magnitude as tty7's GRID_PAD_X/Y)
const GRID_PAD_X: f32 = 6.;
const GRID_PAD_Y: f32 = 4.;

/// Events the panel subscribes to: child process exit (the tab label gets "(exited)" appended)
pub(crate) enum TerminalViewEvent {
    Exited,
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

pub(crate) struct TerminalView {
    pub(crate) terminal: Terminal,
    pub(crate) focus_handle: FocusHandle,
    /// Cell size (measured every frame in prepaint; input.rs needs cell_width for the IME anchor)
    pub(crate) cell_width: Pixels,
    pub(crate) line_height: Pixels,
    /// Line height = font size × multiplier (1.3, the monospace terminal convention; no config item yet)
    pub(crate) line_height_mul: f32,
    /// Previous frame's grid and snapshot: redrawn as-is while the reader thread holds the lock (see element.rs build_grid)
    pub(crate) grid_buf: Vec<RenderCell>,
    pub(crate) grid_snap: Option<GridSnapshot>,
    /// IME preedit text (marked text), drawn underlined at the cursor cell
    pub(crate) marked_text: String,
    pub(crate) cursor_visible: bool,
    selecting: bool,
    /// Sub-line travel accumulated by smooth trackpad scrolling
    scroll_debt: f32,
    _subscriptions: Vec<Subscription>,
}

impl TerminalView {
    /// Wrap an already-started shell (the PTY is spawned by the panel first; no
    /// tab is created on failure). Wires up the event pump / blink timer / focus
    /// subscriptions.
    pub(crate) fn new(terminal: Terminal, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();

        // Event pump: drain in batches, process together (consecutive Wakeup
        // events collapse into one), see tty7 view.rs:1722-1750
        let events = terminal.events.clone();
        cx.spawn(async move |this, cx| {
            let mut batch = Vec::new();
            while let Ok(ev) = events.recv().await {
                batch.push(ev);
                while let Ok(ev) = events.try_recv() {
                    batch.push(ev);
                }
                let res = this.update(cx, |view, cx| {
                    let mut woke = false;
                    for ev in batch.drain(..) {
                        if matches!(ev, AlacEvent::Wakeup) && std::mem::replace(&mut woke, true) {
                            continue;
                        }
                        view.handle_event(ev, cx);
                    }
                });
                if res.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Cursor blink: toggles every 530ms, blinking only while focused (unfocused draws a steady hollow box, see paint_cursor)
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(530))
                    .await;
                if this
                    .update_in(cx, |view, window, cx| {
                        if view.focus_handle.is_focused(window) {
                            view.cursor_visible = !view.cursor_visible;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let _subscriptions = vec![
            cx.on_focus_in(&focus_handle, window, |view, _window, cx| {
                view.cursor_visible = true;
                view.report_focus_change(true);
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |view, _window, cx| {
                view.report_focus_change(false);
                cx.notify();
            }),
        ];

        Self {
            terminal,
            focus_handle,
            cell_width: px(0.),
            line_height: px(0.),
            line_height_mul: 1.3,
            grid_buf: Vec::new(),
            grid_snap: None,
            marked_text: String::new(),
            cursor_visible: true,
            selecting: false,
            scroll_debt: 0.,
            _subscriptions,
        }
    }

    pub(crate) fn shell_name(&self) -> &str {
        self.terminal.shell_name()
    }

    pub(crate) fn exited(&self) -> bool {
        self.terminal.exited()
    }

    fn key_flags(&self) -> KeyFlags {
        KeyFlags::from_mode(self.terminal.term.lock().mode())
    }

    /// Keyboard input: write to the PTY plus scroll to bottom and clear the selection (tty7 send_to_pty + jump_to_prompt)
    fn send_to_pty(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if self.terminal.exited() {
            return;
        }
        self.terminal.write(bytes);
        self.cursor_visible = true;
        let mut term = self.terminal.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        drop(term);
        cx.notify();
    }

    /// Focus in/out reporting (sent only when mode 1004 is on)
    fn report_focus_change(&self, focused: bool) {
        let mode = *self.terminal.term.lock().mode();
        if mode.contains(TermMode::FOCUS_IN_OUT) {
            self.terminal
                .write(if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// alacritty event handling (see handle_event at tty7 view.rs:2508, trimmed to the required subset)
    fn handle_event(&mut self, ev: AlacEvent, cx: &mut Context<Self>) {
        match ev {
            AlacEvent::Wakeup => cx.notify(),
            // The tab label only shows the shell name; title events are ignored
            AlacEvent::Title(_) | AlacEvent::ResetTitle => {}
            AlacEvent::PtyWrite(text) => self.terminal.write(text.as_bytes()),
            AlacEvent::ChildExit(_) | AlacEvent::Exit => {
                self.terminal.mark_exited();
                cx.emit(TerminalViewEvent::Exited);
                cx.notify();
            }
            AlacEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            AlacEvent::ClipboardLoad(_, fmt) => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    self.terminal.write(fmt(&text).as_bytes());
                }
            }
            AlacEvent::ColorRequest(idx, fmt) => {
                // The program queries the color scheme (OSC 10/11/12 and the 256-color table): answer from the current theme
                let colors = TermColors::resolve(cx);
                let rgb = match idx {
                    256 => colors.fg_rgb,
                    257 => colors.bg_rgb,
                    258 => colors::hsla_to_rgb(colors.caret),
                    i => colors.palette[i.min(255)],
                };
                self.terminal.write(fmt(rgb).as_bytes());
            }
            AlacEvent::TextAreaSizeRequest(fmt) => {
                let size = self.terminal.size();
                let reply = fmt(WindowSize {
                    num_lines: size.rows as u16,
                    num_cols: size.cols as u16,
                    cell_width: self.cell_width.as_f32().round().max(1.) as u16,
                    cell_height: self.line_height.as_f32().round().max(1.) as u16,
                });
                self.terminal.write(reply.as_bytes());
            }
            AlacEvent::Bell | AlacEvent::MouseCursorDirty | AlacEvent::CursorBlinkingChange => {}
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal.exited() {
            return;
        }
        let ks = &ev.keystroke;
        let m = &ks.modifiers;

        // Cmd chords are intercepted and handled here, never sent to the PTY (a trimmed tty7 handle_cmd_shortcut)
        if m.platform && !m.control && !m.alt {
            match ks.key.as_str() {
                "c" => {
                    if self.has_selection() {
                        self.copy_selection(cx);
                    } else {
                        // With no selection, Cmd+C passes ETX through (interrupts the foreground program)
                        self.send_to_pty(b"\x03", cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "v" => {
                    self.paste_from_clipboard(cx);
                    cx.stop_propagation();
                    return;
                }
                "a" => {
                    self.select_all(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => return,
            }
        }

        let flags = self.key_flags();
        if let Some(bytes) = input::keystroke_to_bytes(ks, flags) {
            self.send_to_pty(&bytes, cx);
            cx.stop_propagation();
        }
    }

    /// Scroll wheel: scrolls the scrollback by default; in an alt screen where
    /// the program enabled alternate scroll (1007, on by default in less/vim) it
    /// converts to arrow keys. Pixel-wheel deltas accumulate by line height, so
    /// smooth trackpad scrolling lands on whole lines naturally (see the
    /// quantization branch at tty7 view.rs:6056-6076).
    ///
    /// TODO: mouse reporting (under MOUSE_MODE the wheel should be encoded as
    /// mouse events and sent to the program)
    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let raw = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y.as_f32() / self.line_height.as_f32().max(1.),
        };
        let total = self.scroll_debt + raw;
        let lines = total.trunc() as i32;
        self.scroll_debt = total - lines as f32;
        if lines == 0 {
            return;
        }
        let mode = *self.terminal.term.lock().mode();
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            let seq: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
            let mut out = Vec::with_capacity(seq.len() * lines.unsigned_abs() as usize);
            for _ in 0..lines.unsigned_abs() {
                out.extend_from_slice(seq);
            }
            self.terminal.write(&out);
            return;
        }
        self.terminal
            .term
            .lock()
            .scroll_display(Scroll::Delta(lines));
        cx.notify();
    }

    /// Press starts a selection (shift+click extends the existing one; double
    /// click selects a word, triple click a line). The pixel → (line, col)
    /// conversion must add display_offset (see tty7 on_select_start)
    pub(crate) fn on_select_start(
        &mut self,
        col: usize,
        row: usize,
        left: bool,
        clicks: usize,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        let mut term = self.terminal.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(Line(row as i32 - display_offset), Column(col));
        let side = if left { Side::Left } else { Side::Right };
        if shift && clicks == 1 && term.selection.is_some() {
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
            drop(term);
            self.selecting = true;
            cx.notify();
            return;
        }
        let ty = match clicks {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        term.selection = Some(Selection::new(ty, point, side));
        drop(term);
        self.selecting = true;
        cx.notify();
    }

    pub(crate) fn on_select_update(
        &mut self,
        col: usize,
        row: usize,
        left: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        let mut term = self.terminal.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(Line(row as i32 - display_offset), Column(col));
        let side = if left { Side::Left } else { Side::Right };
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side);
        }
        drop(term);
        cx.notify();
    }

    pub(crate) fn on_select_end(&mut self, _cx: &mut Context<Self>) {
        self.selecting = false;
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.terminal
            .term
            .lock()
            .selection
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    }

    pub(crate) fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let text = self.terminal.term.lock().selection_to_string();
        if let Some(text) = text
            && !text.is_empty()
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub(crate) fn select_all(&mut self, cx: &mut Context<Self>) {
        let mut term = self.terminal.term.lock();
        let grid = term.grid();
        let start = Point::new(grid.topmost_line(), Column(0));
        let end = Point::new(grid.bottommost_line(), grid.last_column());
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        term.selection = Some(sel);
        drop(term);
        cx.notify();
    }

    pub(crate) fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let bracketed = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        let bytes = paste_bytes(&text, bracketed);
        self.send_to_pty(&bytes, cx);
    }

    /// IME committed text (arrives via InputHandler::replace_text_in_range)
    pub(crate) fn input_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        self.send_to_pty(text.as_bytes(), cx);
    }

    pub(crate) fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.marked_text = text;
        cx.notify();
    }

    pub(crate) fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if !self.marked_text.is_empty() {
            self.marked_text.clear();
            cx.notify();
        }
    }

    /// Called every frame in prepaint: record the cell size; resize only when
    /// the grid size changed (Term and PTY both go through terminal.resize)
    pub(crate) fn set_grid_size(
        &mut self,
        cols: usize,
        rows: usize,
        cell_width: Pixels,
        line_height: Pixels,
        scale: f32,
    ) {
        self.cell_width = cell_width;
        self.line_height = line_height;
        // Cell pixel sizes reported to the child use device pixels (logical × scale), same as kitty/ghostty
        let scale = if scale.is_finite() && scale > 0. {
            scale
        } else {
            1.
        };
        self.terminal.resize(
            TermSize::new(cols, rows),
            (cell_width.as_f32() * scale).round().max(1.) as u16,
            (line_height.as_f32() * scale).round().max(1.) as u16,
        );
    }
}

impl gpui_kit::Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("terminal-surface")
            .track_focus(&self.focus_handle)
            .key_context("terminal")
            .size_full()
            .overflow_hidden()
            .px(px(GRID_PAD_X))
            .py(px(GRID_PAD_Y))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .child(TerminalElement::new(cx.entity()))
    }
}

/// Opening/closing brackets for bracketed paste (DEC mode 2004)
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Pasted text → bytes written to the PTY (a verbatim port of tty7
/// `crates/tty7-core/src/core/paste.rs`).
///
/// When bracketed, every ESC is stripped: an `ESC[201~` inside the content would
/// close the bracket early, and the text after it would arrive at the shell as
/// if typed, which is how pasted text turns into executed commands. CRLF is
/// always folded into a single newline; without brackets, newlines are sent as
/// \r (the keyboard Enter byte) so multi-line commands run line by line, the
/// long-standing semantics of terminals without 2004.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let mut folded = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            folded.push(b'\n');
            i += 2;
        } else {
            folded.push(bytes[i]);
            i += 1;
        }
    }
    if bracketed {
        let mut out = Vec::with_capacity(PASTE_START.len() + folded.len() + PASTE_END.len());
        out.extend_from_slice(PASTE_START);
        out.extend(folded.iter().copied().filter(|&b| b != 0x1b));
        out.extend_from_slice(PASTE_END);
        out
    } else {
        for b in &mut folded {
            if *b == b'\n' {
                *b = b'\r';
            }
        }
        folded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 粘贴内容不能自己关上括号() {
        let out = paste_bytes("a\x1b[201~; rm -rf /\nb", true);
        assert!(out.starts_with(PASTE_START) && out.ends_with(PASTE_END));
        let inner = &out[PASTE_START.len()..out.len() - PASTE_END.len()];
        assert!(!inner.contains(&0x1b), "{inner:?}");
    }

    #[test]
    fn crlf折叠与无括号换行转回车() {
        assert_eq!(paste_bytes("a\r\nb\n", true), b"\x1b[200~a\nb\n\x1b[201~");
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc");
    }
}
