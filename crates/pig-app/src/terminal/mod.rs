//! Embedded terminal: the public entry of the bottom panel.
//!
//! TerminalPanel holds several TerminalView tabs plus the active index plus the
//! cwd used for new tabs. The tab bar styling follows render_right_tab_bar in
//! crates/pig-app/src/right_panel.rs. Only TerminalPanel / TerminalPanelEvent
//! are pub; every other module is a crate-internal implementation detail.

mod colors;
mod element;
mod input;
mod pty;
mod term;
mod view;

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, px,
};

use term::Terminal;
use view::{TerminalView, TerminalViewEvent};

/// Panel events: currently only "request collapse" (clicking the down arrow at the end of the tab bar)
pub enum TerminalPanelEvent {
    RequestCollapse,
}

impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

/// Bottom terminal panel: top = tab bar; bottom = the active tab's TerminalView.
/// Once every tab is closed an empty state remains ("no terminal" plus a new
/// button); it does not reopen automatically.
pub struct TerminalPanel {
    tabs: Vec<Entity<TerminalView>>,
    /// Meaningless when tabs is empty (rendering takes the empty-state branch)
    active: usize,
    /// Working directory for new tabs (updated by set_cwd; already-open tabs are unaffected)
    cwd: PathBuf,
    /// Shell for new tabs (None = system default; updated by set_shell; already-open tabs are unaffected)
    shell: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl TerminalPanel {
    /// Create the panel and immediately open one terminal tab (cwd is the shell's working directory; shell None = system default)
    pub fn new(
        cwd: PathBuf,
        shell: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            cwd,
            shell,
            _subscriptions: Vec::new(),
        };
        this.open_tab(window, cx);
        this
    }

    /// Update the working directory for subsequently created tabs (called on session switch; open tabs unchanged)
    pub fn set_cwd(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        self.cwd = cwd;
        cx.notify();
    }

    /// Update the shell for subsequently created tabs (called on settings change; open tabs unchanged)
    pub fn set_shell(&mut self, shell: Option<String>, cx: &mut Context<Self>) {
        self.shell = shell;
        cx.notify();
    }

    /// For self-tests: current tab count
    pub(crate) fn debug_tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Open a new tab with the current cwd and configured shell (a failed PTY spawn only logs; no tab is added)
    fn open_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminal = match Terminal::spawn(&self.cwd, self.shell.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!("failed to start shell ({}): {e}", self.cwd.display());
                return;
            }
        };
        let view = cx.new(|cx| TerminalView::new(terminal, window, cx));
        // Child exit → refresh the label (append "(exited)")
        self._subscriptions.push(
            cx.subscribe(&view, |_this, _view, _ev: &TerminalViewEvent, cx| {
                cx.notify()
            }),
        );
        self.tabs.push(view);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Close a tab: dropping the TerminalView releases the PTY (Pty's Drop
    /// kills the child). When the active tab is closed a neighbor becomes active
    /// (the next one slides in, or the previous one when at the end).
    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        drop(self.tabs.remove(ix));
        if self.tabs.is_empty() {
            self.active = 0;
        } else {
            if ix < self.active {
                self.active -= 1;
            }
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            }
        }
        cx.notify();
    }

    /// Focus the active tab's terminal view (called when creating/switching tabs or re-expanding the panel)
    pub(crate) fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.tabs.get(self.active) {
            let handle = view.read(cx).focus_handle.clone();
            handle.focus(window, cx);
        }
    }

    /// One tab: terminal icon plus shell name ("(exited)" appended after the
    /// process exits) plus the × close button. Styling follows render_right_tab
    /// in right_panel.rs.
    fn render_tab(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let view = &self.tabs[ix];
        let id = view.entity_id().as_u64();
        let (label, exited) = {
            let v = view.read(cx);
            (v.shell_name().to_string(), v.exited())
        };
        let label = if exited {
            rust_i18n::t!("terminal.exited", label = label).to_string()
        } else {
            label
        };
        let active = ix == self.active;

        h_flex()
            .id(format!("terminal-tab-{id}"))
            .gap_2()
            .pl_3()
            .pr_1()
            // Fixed 24px height (the tab bar is 30px, leaving a 3px gap top and bottom, not flush with the bar edges)
            .h(px(24.))
            .items_center()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .when(active, |this| this.bg(cx.theme().muted))
            .when(!active, |this| {
                this.text_color(cx.theme().muted_foreground)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            })
            .child(
                Icon::new(IconName::SquareTerminal)
                    .size_3p5()
                    .into_any_element(),
            )
            .child(div().text_sm().child(label))
            .child(
                div()
                    .id(format!("terminal-tab-close-{id}"))
                    .p(px(1.))
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .child(
                        Icon::new(IconName::Close)
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_tab(ix, cx);
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.active = ix;
                this.focus_active(window, cx);
                cx.notify();
            }))
            .into_any_element()
    }

    /// Tab bar: the tab list plus a trailing "+" new-tab and collapse button (mirroring render_right_tab_bar)
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .flex_shrink_0()
            .h(px(30.))
            // Children vertically centered: tabs do not stretch to the full bar height (gap top and bottom)
            .items_center()
            .pl_2()
            .pr_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .children((0..self.tabs.len()).map(|ix| self.render_tab(ix, cx)))
            .child(div().flex_1())
            .child(
                Button::new("terminal-tab-add")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_tab(window, cx);
                    })),
            )
            .child(
                Button::new("terminal-panel-collapse")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .on_click(cx.listener(|_this, _, _, cx| {
                        cx.emit(TerminalPanelEvent::RequestCollapse);
                    })),
            )
    }

    /// Empty state: after all tabs are closed, show "no terminal" plus a new button; no automatic reopen
    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("terminal.none")),
            )
            .child(
                Button::new("terminal-empty-new")
                    .outline()
                    .xsmall()
                    .label(rust_i18n::t!("terminal.new"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_tab(window, cx);
                    })),
            )
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match self.tabs.get(self.active) {
            Some(view) => div()
                .flex_1()
                .min_h_0()
                .child(view.clone())
                .into_any_element(),
            None => self.render_empty(cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .child(self.render_tab_bar(cx))
            .child(content)
    }
}
