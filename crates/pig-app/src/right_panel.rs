use super::*;
use gpui_kit::component::text::TextView;

/// Right panel menu item: (name, icon, shortcut action, placeholder-disabled,
/// tab to open on click)
pub(crate) type RightMenuItem = (
    std::borrow::Cow<'static, str>,
    AssetsIconName,
    Option<&'static dyn Action>,
    bool,
    Option<RightTab>,
);

impl AppView {
    /// Right panel tab toggle (for shortcuts): triggering on the active tab =
    /// collapse the panel; otherwise open it and activate that tab.
    pub(crate) fn toggle_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if self.right_open && self.right_active.as_ref() == Some(&tab) {
            self.right_open = false;
        } else {
            if !self.right_tabs.contains(&tab) {
                self.right_tabs.push(tab.clone());
            }
            self.right_active = Some(tab);
            self.right_open = true;
        }
        cx.notify();
    }

    /// Open and activate a right tab (for menu clicks, pure open with no
    /// collapse semantics); when the trajectory tab opens, reread the
    /// persisted records for the current session
    pub(crate) fn open_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if !self.right_tabs.contains(&tab) {
            self.right_tabs.push(tab.clone());
        }
        if tab == RightTab::Trajectory {
            self.reload_trajectory();
        }
        self.right_active = Some(tab);
        self.right_open = true;
        cx.notify();
    }

    /// Close a right tab: when the active tab is closed, switch to the last
    /// remaining one; when the last tab is closed the panel has no content to
    /// show and collapses automatically (collapse animation via
    /// step_dock_anim).
    pub(crate) fn close_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        self.right_tabs.retain(|t| *t != tab);
        // The "Subagent" tab's content panel is released when the tab closes
        if let RightTab::Subagent { agent_id } = &tab {
            self.subagent_tabs.remove(agent_id);
        }
        // The "File" tab's content panel is released when the tab closes
        if let RightTab::File { path } = &tab {
            self.file_tabs.remove(path);
        }
        // The "Compact summary" tab's render state is released when the tab closes
        if tab == RightTab::CompactSummary {
            self.compact_summary = None;
        }
        if self.right_active.as_ref() == Some(&tab) {
            self.right_active = self.right_tabs.last().cloned();
        }
        if self.right_tabs.is_empty() {
            self.right_open = false;
        }
        cx.notify();
    }

    /// Open/focus the "Subagent" tab (notification card click): if not open,
    /// create the panel entity and send the load request; if already open
    /// (same agent_id), just focus it without loading again.
    pub(crate) fn open_subagent_tab(
        &mut self,
        session_id: String,
        agent_id: String,
        title: String,
        cx: &mut Context<Self>,
    ) {
        if !self.subagent_tabs.contains_key(&agent_id) {
            let panel = cx.new(|_| SubagentPanel::new(session_id.clone(), title));
            self.subagent_tabs.insert(agent_id.clone(), panel);
            self.agent.load_subagent(session_id, agent_id.clone());
        }
        let tab = RightTab::Subagent { agent_id };
        if !self.right_tabs.contains(&tab) {
            self.right_tabs.push(tab.clone());
        }
        self.right_active = Some(tab);
        self.right_open = true;
        cx.notify();
    }

    /// Open/focus the "File" tab (Read card path click): the path is resolved
    /// against the session cwd into an absolute path (canonicalized for
    /// dedup); if already open, reread the file (to get the latest content)
    /// and focus it.
    /// `line`: first line number from the Read output; scrolls to that line
    /// after loading completes.
    pub(crate) fn open_file_tab(
        &mut self,
        session_id: &str,
        raw_path: String,
        line: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let cwd = self
            .metas
            .iter()
            .find(|m| m.id == session_id)
            .map(|m| m.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        let joined = {
            let path = std::path::PathBuf::from(&raw_path);
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        };
        // canonicalize for dedup (normalizes ./ ../ and symlinks); fall back
        // to the original path when the file does not exist (the panel shows
        // a "read failed" error state)
        let full = joined.canonicalize().unwrap_or(joined);
        let key = full.to_string_lossy().to_string();
        match self.file_tabs.get(&key) {
            Some(panel) => panel.update(cx, |panel, cx| panel.reload(line, cx)),
            None => {
                let panel = cx.new(|_| FileViewPanel::new(raw_path, full));
                panel.update(cx, |panel, cx| panel.reload(line, cx));
                self.file_tabs.insert(key.clone(), panel);
            }
        }
        let tab = RightTab::File { path: key };
        if !self.right_tabs.contains(&tab) {
            self.right_tabs.push(tab.clone());
        }
        self.right_active = Some(tab);
        self.right_open = true;
        cx.notify();
    }

    /// Open/focus the "compact summary" tab (the compact divider's "view summary"
    /// link): one shared tab whose content is rebuilt per click, so each compaction
    /// point shows its own summary.
    pub(crate) fn open_compact_summary_tab(&mut self, text: String, cx: &mut Context<Self>) {
        let state = cx.new(|cx| TextViewState::markdown("", cx));
        state.update(cx, |state, cx| state.set_text(&text, cx));
        self.compact_summary = Some(state);
        let tab = RightTab::CompactSummary;
        if !self.right_tabs.contains(&tab) {
            self.right_tabs.push(tab.clone());
        }
        self.right_active = Some(tab);
        self.right_open = true;
        cx.notify();
    }

    /// Toggles the tab bar "+" add-panel menu.
    pub(crate) fn toggle_right_menu(&mut self, click: &ClickEvent, cx: &mut Context<Self>) {
        // Clicking the button while the menu is open: the press first fires
        // the menu's outside-close (recording the press position), and the
        // immediately following click is swallowed by matching the same press
        // position, avoiding collapse-then-instantly-reopen (same handling as
        // the composer popup)
        let down_pos = match click {
            ClickEvent::Mouse(event) => Some(event.down.position),
            _ => None,
        };
        if self
            .right_menu_outside_close
            .take()
            .is_some_and(|pos| Some(pos) == down_pos)
        {
            return;
        }
        self.right_menu_open = !self.right_menu_open;
        cx.notify();
    }

    /// Browser/side chat are placeholder-disabled items; the shortcuts are
    /// shown first and the features come later.
    /// (The terminal already landed as a bottom panel: title bar button /
    /// ctrl-`, see terminal/, not in this menu)
    pub(crate) fn right_menu_items() -> [RightMenuItem; 4] {
        [
            (
                rust_i18n::t!("panel.changes"),
                AssetsIconName::GitBranch,
                Some(&ToggleChanges),
                false,
                Some(RightTab::Changes),
            ),
            (
                rust_i18n::t!("panel.trajectory"),
                AssetsIconName::FileText,
                None,
                false,
                Some(RightTab::Trajectory),
            ),
            (
                rust_i18n::t!("panel.browser"),
                AssetsIconName::Globe,
                Some(&ToggleBrowser),
                true,
                None,
            ),
            (
                rust_i18n::t!("panel.side_chat"),
                AssetsIconName::MessageCircle,
                Some(&ToggleSideChat),
                true,
                None,
            ),
        ]
    }

    /// Shortcut chip set (ZCode style: one small chip per key; hidden when no
    /// key is bound).
    /// page = panel home page: large bordered keycaps, macOS modifier symbols
    /// split per key; otherwise (dropdown menu): small chips on a muted
    /// background, the macOS symbol string as a single chip.
    pub(crate) fn render_shortcut_chips(
        &self,
        action: &dyn Action,
        page: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let binding = window
            .highest_precedence_binding_for_action_in_context(action, KeyContext::default())?;
        let stroke = binding.keystrokes().first()?.as_keystroke().clone();
        let text = Kbd::format(&stroke);
        // Windows-style "Ctrl+Shift+G" splits on + into single-key chips;
        // macOS symbol strings have no +: in page mode each modifier becomes
        // its own keycap ("⌃⇧G" → ⌃ | ⇧ | G) and consecutive plain characters
        // merge into one
        let keys: Vec<String> = if text.contains('+') {
            text.split('+').map(|s| s.to_string()).collect()
        } else if page {
            let mut keys = Vec::new();
            let mut run = String::new();
            for ch in text.chars() {
                if matches!(ch, '⌃' | '⌥' | '⇧' | '⌘') {
                    if !run.is_empty() {
                        keys.push(std::mem::take(&mut run));
                    }
                    keys.push(ch.to_string());
                } else {
                    run.push(ch);
                }
            }
            if !run.is_empty() {
                keys.push(run);
            }
            keys
        } else {
            vec![text]
        };
        Some(
            h_flex()
                .gap_1()
                .flex_shrink_0()
                .children(keys.into_iter().map(|key| {
                    let chip = div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(key);
                    if page {
                        chip.min_w_5()
                            .h_5()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(cx.theme().border)
                    } else {
                        chip.px_1()
                            .py_0p5()
                            .min_w_5()
                            .rounded(cx.theme().radius.half())
                            .bg(cx.theme().muted)
                    }
                    .into_any_element()
                }))
                .into_any_element(),
        )
    }

    /// Menu row: icon + name + shortcut chips; disabled marks placeholder
    /// items (not clickable).
    /// page = panel home page: whole column centered, bordered keycaps;
    /// otherwise (the "+" dropdown menu): compact rows. In both modes the
    /// shortcut hugs the row's right edge.
    pub(crate) fn render_right_menu_row(
        &self,
        ix: usize,
        page: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, icon, shortcut, disabled, tab) = Self::right_menu_items()[ix].clone();
        let chips =
            shortcut.and_then(|action| self.render_shortcut_chips(action, page, window, cx));
        h_flex()
            .id(("right-menu-item", ix))
            .w_full()
            .px_2()
            .gap_2()
            .map(|this| if page { this.py_2() } else { this.py_1p5() })
            .rounded(cx.theme().radius)
            .when(disabled, |this| this.opacity(0.5))
            .when(!disabled, |this| {
                this.cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.right_menu_open = false;
                        if let Some(tab) = tab.clone() {
                            this.open_right_tab(tab, cx);
                        }
                    }))
            })
            .child(Icon::new(icon).size_4().text_color(if disabled {
                cx.theme().muted_foreground
            } else {
                cx.theme().foreground
            }))
            .child(
                div()
                    .when(page, |this| this.text_xs())
                    .when(!page, |this| this.text_sm())
                    .flex_1()
                    .whitespace_nowrap()
                    .text_color(if disabled {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .child(label),
            )
            .when_some(chips, |this, chips| this.child(chips))
            .into_any_element()
    }

    /// Panel home page (menu page): shown when the panel is expanded but no
    /// tab is open — four items: changes/browser/terminal/side chat (same as
    /// ZCode, effectively the panel's home page).
    /// Spacious large rows, whole column centered: row width capped at 320,
    /// names hug the left and keycaps the right; the cap is fixed so rows do
    /// not wobble when the panel is dragged wider.
    pub(crate) fn render_right_menu_page(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .justify_center()
            .items_center()
            .child(
                v_flex().w_full().max_w(px(280.)).px_2().gap_1().children(
                    (0..Self::right_menu_items().len())
                        .map(|ix| self.render_right_menu_row(ix, true, window, cx)),
                ),
            )
            .into_any_element()
    }

    /// The tab bar "+" add-panel menu: drawn deferred at the window layer,
    /// `Positioner::side(Bottom)` anchored right below the "+" button
    /// (gpui-kit's dropdown_menu uses corner anchoring, where BottomRight
    /// pops the menu above the button and past the window top; and when the
    /// popup covers the title bar's HTCAPTION drag area, clicks get swallowed
    /// by the system's window-move modal loop — hence the hand-drawn menu,
    /// same pattern as the turn navigation bar's preview card).
    pub(crate) fn render_right_menu_dropdown(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bounds = self.tab_add_btn_bounds.get();
        deferred(
            Positioner::side(bounds)
                .placement(Placement::Bottom)
                .align(Align::End)
                .offset(px(6.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("right-menu")
                        .w(px(220.))
                        .p_1()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.right_menu_open = false;
                            this.right_menu_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .children(
                            (0..Self::right_menu_items().len())
                                .map(|ix| self.render_right_menu_row(ix, false, window, cx)),
                        ),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// A single tab in the right tab bar: icon + name + close button (click
    /// to activate, × to close)
    pub(crate) fn render_right_tab(&self, tab: RightTab, cx: &mut Context<Self>) -> AnyElement {
        let active = self.right_active.as_ref() == Some(&tab);
        // "Subagent" tab: Bot icon + panel title (truncated description);
        // "Changes" and "Trajectory" are built-in pages
        let (icon, label) = match &tab {
            RightTab::Changes => (
                Icon::new(AssetsIconName::GitBranch)
                    .size_3p5()
                    .into_any_element(),
                rust_i18n::t!("panel.changes").to_string(),
            ),
            RightTab::Trajectory => (
                Icon::new(AssetsIconName::FileText)
                    .size_3p5()
                    .into_any_element(),
                rust_i18n::t!("panel.trajectory").to_string(),
            ),
            RightTab::Subagent { agent_id } => {
                let title = self
                    .subagent_tabs
                    .get(agent_id)
                    .map(|panel| panel.read(cx).title().to_string())
                    .unwrap_or_else(|| rust_i18n::t!("panel.subagent").to_string());
                (
                    Icon::new(IconName::Bot).size_3p5().into_any_element(),
                    truncate_tab_label(&title),
                )
            }
            RightTab::File { path } => {
                let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
                (
                    Icon::new(IconName::FileText).size_3p5().into_any_element(),
                    truncate_tab_label(name),
                )
            }
            RightTab::CompactSummary => (
                Icon::new(AssetsIconName::Archive)
                    .size_3p5()
                    .into_any_element(),
                rust_i18n::t!("panel.compact_summary").to_string(),
            ),
        };
        h_flex()
            .id(format!("right-tab-{}", tab.key()))
            .gap_2()
            .pl_3()
            .pr_1()
            .py_1()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .when(active, |this| this.bg(cx.theme().accent))
            .when(!active, |this| {
                this.text_color(cx.theme().muted_foreground)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            })
            .child(icon)
            .child(div().text_sm().child(label))
            .child(
                div()
                    .id(format!("right-tab-close-{}", tab.key()))
                    .p(px(1.))
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .child(
                        Icon::new(IconName::Close)
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .on_click(cx.listener({
                        let tab = tab.clone();
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_right_tab(tab.clone(), cx);
                        }
                    })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.right_active = Some(tab.clone());
                this.right_open = true;
                // Reread when the trajectory tab is activated (data may have
                // changed after switching sessions or a new turn)
                if tab == RightTab::Trajectory {
                    this.reload_trajectory();
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// Tab bar at the top of the right panel: tab list + trailing "+"
    /// (add-tab menu) and the collapse button
    pub(crate) fn render_right_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .flex_shrink_0()
            .h(px(36.))
            .pl_2()
            .pr_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .children(
                self.right_tabs
                    .iter()
                    .map(|tab| self.render_right_tab(tab.clone(), cx)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("right-tab-add-btn")
                    .on_prepaint({
                        let cell = self.tab_add_btn_bounds.clone();
                        move |bounds, _, _| cell.set(bounds)
                    })
                    .child(
                        Button::new("right-tab-add")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                this.toggle_right_menu(event, cx);
                            })),
                    ),
            )
            .child(
                Button::new("right-panel-collapse")
                    .ghost()
                    .xsmall()
                    .icon(IconName::PanelRightClose)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_open = false;
                        cx.notify();
                    })),
            )
    }

    /// Right dock panel content: tab bar + (the active tab's content if any,
    /// otherwise the panel home/menu page)
    pub(crate) fn render_right_dock_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current_views = self.current.as_ref().and_then(|id| self.views.get(id));
        let content: AnyElement = match &self.right_active {
            Some(RightTab::Changes) => match current_views {
                Some(views) => views.review.clone().into_any_element(),
                None => v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("panel.changes_empty")),
                    )
                    .into_any_element(),
            },
            Some(RightTab::Trajectory) => self.render_trajectory_panel(cx),
            Some(RightTab::Subagent { agent_id }) => match self.subagent_tabs.get(agent_id) {
                Some(panel) => panel.clone().into_any_element(),
                None => self.render_right_menu_page(window, cx),
            },
            Some(RightTab::File { path }) => match self.file_tabs.get(path) {
                Some(panel) => panel.clone().into_any_element(),
                None => self.render_right_menu_page(window, cx),
            },
            Some(RightTab::CompactSummary) => match &self.compact_summary {
                // Read-only markdown view of the clicked compaction point's summary
                // (the outer div scrolls, same as the trajectory panel)
                Some(state) => div()
                    .id("compact-summary-panel")
                    .size_full()
                    .overflow_y_scroll()
                    .p_3()
                    .child(TextView::new(state).selectable(true).text_sm())
                    .into_any_element(),
                None => self.render_right_menu_page(window, cx),
            },
            None => self.render_right_menu_page(window, cx),
        };
        // Open/close animation anchor layer (same as Sidebar::render):
        // dock_frame has overflow_hidden built in; during the tween the
        // dock's real width is smaller than the content width. The content is
        // fixed at right_w and left-anchored to the divider, so collapsing
        // slides the whole thing right and gets clipped instead of being
        // squeezed and re-laid out. In steady state the real width == right_w
        // and the absolutely positioned child layer fills it exactly
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(self.right_w))
                    .child(
                        v_flex()
                            .size_full()
                            // The divider is drawn by the dock handle itself
                            // (same as the sidebar, no self-drawn border_l)
                            .child(self.render_right_tab_bar(cx))
                            .child(div().flex_1().min_h_0().child(content)),
                    ),
            )
            .into_any_element()
    }
}
