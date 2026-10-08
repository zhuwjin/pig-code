use super::*;

/// One quick-switcher result row (raw identifiers only; display text is built
/// at draw time, so a language switch updates the open popup too)
pub(crate) enum SearchRow {
    Workspace { path: String },
    Session { id: String },
}

/// Session rows shown at most (bounds the rendered children; the list scrolls)
const MAX_SESSION_ROWS: usize = 50;

impl AppView {
    /// Open the global search popup (Ctrl+K / the sidebar "Search" row).
    /// The placeholder is re-applied on every open so it follows the current
    /// locale (InputState stores the placeholder as a plain string)
    pub(crate) fn open_search_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = true;
        self.search_selected = 0;
        self.search_input.update(cx, |input, cx| {
            input.set_placeholder(rust_i18n::t!("search.placeholder"), window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// Close the popup and reset its transient state. set_value emits no
    /// Change (upstream emit_events=false), so the selection is reset here
    pub(crate) fn close_search_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_open {
            return;
        }
        self.search_open = false;
        self.search_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        self.search_selected = 0;
        cx.notify();
    }

    /// Workspace display name: prefer the user alias, else the directory name
    /// (same rule as the sidebar)
    pub(crate) fn workspace_display_name(&self, path: &str) -> String {
        self.workspace_aliases
            .get(path)
            .cloned()
            .unwrap_or_else(|| {
                std::path::Path::new(path)
                    .file_name()
                    .map(|name| name.display().to_string())
                    .unwrap_or_else(|| path.to_string())
            })
    }

    /// Result rows for the current query: workspaces first (matched by display
    /// name or full path, in activity order), then sessions (matched by title,
    /// newest first, archived excluded). An empty query lists everything — the
    /// popup doubles as a quick switcher
    pub(crate) fn search_rows(&self, cx: &App) -> Vec<SearchRow> {
        let query = self.search_input.read(cx).value().to_lowercase();
        let hits = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
        let mut rows: Vec<SearchRow> = self
            .compute_workspaces()
            .iter()
            .filter(|path| hits(&self.workspace_display_name(path)) || hits(path.as_str()))
            .map(|path| SearchRow::Workspace { path: path.clone() })
            .collect();
        let mut sessions: Vec<&SessionMeta> = self
            .metas
            .iter()
            .filter(|meta| !meta.archived && hits(&meta.title))
            .collect();
        sessions.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
        rows.extend(
            sessions
                .into_iter()
                .take(MAX_SESSION_ROWS)
                .map(|meta| SearchRow::Session {
                    id: meta.id.clone(),
                }),
        );
        rows
    }

    /// Index of a row among the scroll container's children: group headers
    /// are children too (one per non-empty group), so the child index is the
    /// row index plus the headers rendered above it
    fn search_child_index(&self, row_ix: usize, ws_count: usize, total: usize) -> usize {
        row_ix + usize::from(ws_count > 0) + usize::from(row_ix >= ws_count && total > ws_count)
    }

    /// ↑/↓ selection move (wrapping, same as the composer popup); the list
    /// scrolls the selected row into view via scroll_to_item
    pub(crate) fn move_search_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        let rows = self.search_rows(cx);
        let count = rows.len();
        if count == 0 {
            return;
        }
        self.search_selected =
            (self.search_selected as i32 + delta).rem_euclid(count as i32) as usize;
        let ws_count = rows
            .iter()
            .filter(|row| matches!(row, SearchRow::Workspace { .. }))
            .count();
        let child_ix = self.search_child_index(self.search_selected, ws_count, count);
        self.search_scroll.scroll_to_item(child_ix);
        cx.notify();
    }

    /// Enter/click on the selected row: a workspace enters hero preset to it
    /// (same as the sidebar's "new task in workspace"); a session switches to
    /// it. The popup always closes first so the switch lands on a clean state
    pub(crate) fn confirm_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.search_rows(cx);
        match rows.get(self.search_selected) {
            Some(SearchRow::Workspace { path }) => {
                let path = path.clone();
                self.close_search_popup(window, cx);
                self.hero_cwd = Some(PathBuf::from(path));
                self.enter_hero(cx);
            }
            Some(SearchRow::Session { id }) => {
                let id = id.clone();
                self.close_search_popup(window, cx);
                self.switch_session(id, cx);
            }
            None => self.close_search_popup(window, cx),
        }
    }

    /// The global search popup: a top-biased modal dialog (official Dialog
    /// component — focus trap, Esc, backdrop-press close) with the query
    /// input, grouped results, and a keyboard-hint footer. ↑/↓ bindings live
    /// on the "search" context around the input (main.rs); Enter rides the
    /// input's PressEnter; Esc rides the Dialog's own Cancel
    pub(crate) fn render_search_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.search_rows(cx);
        let ws_count = rows
            .iter()
            .filter(|row| matches!(row, SearchRow::Workspace { .. }))
            .count();
        let session_count = rows.len() - ws_count;

        // Group headers ("Workspaces 7"): count labels pick the singular key
        // for n == 1 (t! needs literal keys, so the pair is selected here)
        let workspaces_label = if ws_count == 1 {
            rust_i18n::t!("search.workspaces_one").to_string()
        } else {
            rust_i18n::t!("search.workspaces", n = ws_count).to_string()
        };
        let sessions_label = if session_count == 1 {
            rust_i18n::t!("search.sessions_one").to_string()
        } else {
            rust_i18n::t!("search.sessions", n = session_count).to_string()
        };
        let group_header = |label: String| {
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label)
                .into_any_element()
        };

        // Result rows (direct children of the scroll container — scroll_to_item
        // targets children by index, so group headers participate in the count).
        // Headers are injected inline before each group's first row
        let mut children: Vec<AnyElement> = Vec::new();
        for (row_ix, row) in rows.iter().enumerate() {
            if row_ix == 0 && ws_count > 0 {
                children.push(group_header(workspaces_label.clone()));
            }
            if row_ix == ws_count && session_count > 0 {
                children.push(group_header(sessions_label.clone()));
            }
            let selected = row_ix == self.search_selected;
            let row = match row {
                SearchRow::Workspace { path } => {
                    let name = self.workspace_display_name(path);
                    h_flex()
                        .id(("search-row", row_ix))
                        .gap_2()
                        .items_center()
                        .mx_1()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |this| this.bg(cx.theme().accent.opacity(0.55)))
                        .when(!selected, |this| {
                            this.hover(|this| this.bg(cx.theme().accent.opacity(0.3)))
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.search_selected = row_ix;
                            this.confirm_search(window, cx);
                        }))
                        .child(
                            Icon::new(IconName::Folder)
                                .size_4()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                // Selection stays a background tint only — the
                                // text keeps the default foreground (accent text
                                // on the accent-tinted selection background
                                // washes out; same convention as the sidebar's
                                // active row)
                                .text_sm()
                                .child(name),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .min_w_0()
                                .max_w(px(240.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(path.clone()),
                        )
                        .into_any_element()
                }
                SearchRow::Session { id } => {
                    let Some(meta) = self.metas.iter().find(|meta| &meta.id == id) else {
                        continue;
                    };
                    let title = crate::sidebar::display_title(&meta.title);
                    let workspace = self.workspace_display_name(&meta.cwd.display().to_string());
                    let time = meta.updated_at.relative();
                    v_flex()
                        .id(("search-row", row_ix))
                        .gap_0p5()
                        .mx_1()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |this| this.bg(cx.theme().accent.opacity(0.55)))
                        .when(!selected, |this| {
                            this.hover(|this| this.bg(cx.theme().accent.opacity(0.3)))
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.search_selected = row_ix;
                            this.confirm_search(window, cx);
                        }))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        // Selection = background tint only (see
                                        // the workspace name note above)
                                        .text_sm()
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(time),
                                ),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(workspace),
                        )
                        .into_any_element()
                }
            };
            children.push(row);
        }

        let empty = rows.is_empty().then(|| {
            div()
                .px_3()
                .py_6()
                .text_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("search.empty"))
                .into_any_element()
        });

        let view = cx.entity();
        Dialog::new(cx)
            .open(self.search_open)
            .on_open_change(move |open, _, window, cx| {
                if !open {
                    view.update(cx, |this, cx| this.close_search_popup(window, cx));
                }
            })
            .backdrop(
                div()
                    .id("search-popup-backdrop")
                    // The Dialog wraps the backdrop element in an absolutely
                    // positioned full-viewport div but gives it no size — the
                    // caller's element must fill it itself, otherwise it has
                    // zero height and the bg never paints (exactly what
                    // happened for the first iterations of this popup; the
                    // wrapper's comment calls this "a caller's absolute()
                    // surface has a box to fill")
                    .size_full()
                    // Occlude: the dimmed mask must also block hit-testing for
                    // elements BEHIND it — without this, cursor styles from the
                    // covered content (pointer over rows, I-beam over text)
                    // bleed through the overlay (same fix as the card below;
                    // the Dialog component itself doesn't occlude — only
                    // Popover does, upstream)
                    .occlude()
                    // Occluding also blocks the Dialog wrapper's own
                    // backdrop-press close handler (dispatch stops at this
                    // hitbox), so the close lives here directly; the card is
                    // painted above and stops propagation, so its clicks never
                    // reach this handler
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.close_search_popup(window, cx);
                        }),
                    )
                    // "Frosted glass" approximation: this gpui fork has no
                    // element-level backdrop blur (only whole-window Mica/
                    // Acrylic materials, useless for in-window overlays), so
                    // the mask suppresses the covered content with a heavy
                    // translucent veil (~12% content visibility — the visual
                    // weight of a full blur). The veil uses the popover color:
                    // slightly lighter than the background in the dark theme
                    // (a smoked-glass tint that reads over the same-colored
                    // app surfaces) and near-white in the light theme (milky
                    // frost). Masking with the background color itself was a
                    // visual no-op in the dark theme. Real blur needs upstream
                    // backdrop-filter support
                    .bg(cx.theme().popover.opacity(0.88)),
            )
            // Bias the dialog toward the upper third (the host centers by
            // default; style refinements apply to the host container)
            .items_start()
            .pt(px(120.))
            .child(
                v_flex()
                    .id("search-popup-card")
                    // Occlude the card: blocks cursor-style/event dispatch to
                    // everything painted below it (the app content behind the
                    // popup), the upstream Popover's approach
                    .occlude()
                    // Shield the card from the backdrop's mouse-down close:
                    // gpui hit-testing dispatches to every element containing
                    // the point (the full-viewport backdrop sibling included),
                    // so the card must stop propagation at mouse-DOWN time —
                    // an on_click shield (mouse up) would fire too late
                    .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                    .w(px(600.))
                    .rounded_xl()
                    .border_1()
                    .border_color(cx.theme().border)
                    // Solid popover surface: stands out from the frosted mask
                    // by being fully opaque (plus the border), keeping text
                    // crisp
                    .bg(cx.theme().popover)
                    .overflow_hidden()
                    // Query input: the "search" context carries the ↑/↓
                    // selection bindings (main.rs)
                    .child(
                        div()
                            .key_context("search")
                            .on_action(cx.listener(|this, _: &SearchPrev, _, cx| {
                                this.move_search_selection(-1, cx)
                            }))
                            .on_action(cx.listener(|this, _: &SearchNext, _, cx| {
                                this.move_search_selection(1, cx)
                            }))
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(Input::new(&self.search_input)),
                    )
                    .child(
                        div()
                            .id("search-popup-list")
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .track_scroll(&self.search_scroll)
                            .py_1()
                            .children(children)
                            .when_some(empty, |this, empty| this.child(empty)),
                    )
                    .child(
                        h_flex()
                            .gap_6()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("search.hint_select"))
                            .child(rust_i18n::t!("search.hint_open"))
                            .child(rust_i18n::t!("search.hint_close")),
                    ),
            )
            .into_any_element()
    }
}
