use super::*;

impl Sidebar {
    /// Action rows ("new task" / "search"): the trailing shortcut keycaps
    /// (same bordered style as the right panel home page) appear only while
    /// the row is hovered — stateless via group/group_hover; hidden chips
    /// keep their layout space (opacity 0) so the row never reflows
    pub(crate) fn render_action_rows(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .p_2()
            .gap_1()
            .child(
                h_flex()
                    .id("new-task")
                    .group("action-row")
                    .gap_2()
                    .px_2()
                    .py_1()
                    .w_full()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SidebarEvent::NewTask);
                    }))
                    .child(Icon::new(IconName::Plus).size_4())
                    .child(
                        div()
                            .text_sm()
                            .flex_1()
                            .child(rust_i18n::t!("sidebar.new_task")),
                    )
                    .when_some(
                        crate::AppView::render_shortcut_chips(&crate::NewTask, true, window, cx),
                        |this, chips| {
                            this.child(
                                div()
                                    .opacity(0.)
                                    .group_hover("action-row", |mut style| {
                                        style.opacity = Some(1.);
                                        style
                                    })
                                    .child(chips),
                            )
                        },
                    ),
            )
            .child(
                h_flex()
                    .id("open-search")
                    .group("action-row")
                    .gap_2()
                    .px_2()
                    .py_1()
                    .w_full()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SidebarEvent::OpenSearch);
                    }))
                    .child(Icon::new(IconName::Search).size_4())
                    .child(
                        div()
                            .text_sm()
                            .flex_1()
                            .child(rust_i18n::t!("sidebar.search")),
                    )
                    .when_some(
                        crate::AppView::render_shortcut_chips(
                            &crate::FocusSearch,
                            true,
                            window,
                            cx,
                        ),
                        |this, chips| {
                            this.child(
                                div()
                                    .opacity(0.)
                                    .group_hover("action-row", |mut style| {
                                        style.opacity = Some(1.);
                                        style
                                    })
                                    .child(chips),
                            )
                        },
                    ),
            )
            .into_any_element()
    }

    /// "Sessions" header row: collapse/expand all workspaces (grouped view only)
    /// plus the list management menu
    pub(crate) fn render_list_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let any_expanded = self.workspaces.iter().any(|w| self.expanded.contains(w));
        let mut row = h_flex().pl_3().pr_2().py_1().gap_1().child(
            div()
                .flex_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("sidebar.sessions")),
        );
        if self.view == SidebarView::Workspace {
            row = row.child(
                Button::new("toggle-all-workspaces")
                    .ghost()
                    .xsmall()
                    .icon(if any_expanded {
                        AssetsIconName::FoldVertical
                    } else {
                        AssetsIconName::UnfoldVertical
                    })
                    .tooltip(if any_expanded {
                        rust_i18n::t!("sidebar.collapse_all")
                    } else {
                        rust_i18n::t!("sidebar.expand_all")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        let any_expanded =
                            this.workspaces.iter().any(|w| this.expanded.contains(w));
                        if any_expanded {
                            // Collapse all: unload immediately (no animation) and
                            // cancel in-progress collapsing states
                            this.expanded.clear();
                            for anim in this.expand_anims.values_mut() {
                                anim.collapsing = false;
                            }
                        } else {
                            // Expand all: play the slide-open animation one by one;
                            // generation+1 also invalidates in-progress collapse
                            // unload timers
                            for w in &this.workspaces {
                                this.expanded.insert(w.clone());
                                let anim = this.expand_anims.entry(w.clone()).or_default();
                                anim.generation += 1;
                                anim.collapsing = false;
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        row.child(
            Button::new("list-manage")
                .ghost()
                .xsmall()
                .icon(AssetsIconName::SlidersHorizontal)
                .tooltip(rust_i18n::t!("sidebar.list_manage"))
                .dropdown_menu_with_anchor(
                    Anchor::TopRight,
                    Self::view_menu(&cx.entity().downgrade(), self.view),
                ),
        )
        .into_any_element()
    }

    /// Workspace-nested single-line session row: the 32px left padding
    /// indents the content so the title aligns with the workspace name (row
    /// mx 8 + px 8 + icon 16 + gap 8 = 40 from the sidebar edge), while the
    /// row's own highlight box keeps the workspace row's mx_2 span — the two
    /// highlight rectangles stay left-aligned
    pub(crate) fn render_session_row(
        &self,
        window: &Window,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = &self.sessions[ix];
        let id = session.id.clone();
        let active = self.active.as_deref() == Some(session.id.as_str());
        // Status indicator: yellow dot for pending approval; while running no dot is
        // drawn and the spinner takes the row-tail time slot (see below)
        let status_color = if session.waiting_approval {
            Some(cx.theme().warning)
        } else {
            None
        };
        let renaming = self.renaming == Some(RenameTarget::Session(session.id.clone()));
        let hovered = self.hovered_session.as_deref() == Some(session.id.as_str());
        // Fade base color = row background: selected and hovered both sidebar +
        // accent 60%, otherwise plain sidebar
        let (fade_base, fade_tint) = if hovered || active {
            (cx.theme().sidebar, Some(cx.theme().accent.opacity(0.6)))
        } else {
            (cx.theme().sidebar, None)
        };

        let hover_id = session.id.clone();
        let mut row = h_flex()
            .id(("session", ix))
            .mx_2()
            .pl_8()
            .pr_2()
            .py_1()
            .gap_2()
            .rounded(cx.theme().radius)
            .when(session.archived, |this| {
                this.text_color(cx.theme().muted_foreground)
            })
            .when(active, |this| this.bg(cx.theme().accent.opacity(0.6)))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::Select(id.clone()));
            }))
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                let was = this.hovered_session.as_deref() == Some(hover_id.as_str());
                if *is_hovered != was {
                    this.hovered_session = (*is_hovered).then(|| hover_id.clone());
                    cx.notify();
                }
            }));
        if renaming {
            // Inline rename: keep only the input (Enter/blur commits, empty cancels)
            row = row.child(div().flex_1().child(Input::new(&self.rename_input).small()));
            return row.into_any_element();
        }
        row = row
            .child(self.render_title_scroll(
                &session.id,
                ("session-title", ix),
                &display_title(&session.title),
                fade_base,
                fade_tint,
                window,
                cx,
            ))
            .when_some(status_color, |this, color| {
                this.child(div().size_2().rounded_full().bg(color))
            });
        if !hovered {
            // On hover the row tail yields to the pin/archive buttons; while running
            // the time becomes a spinner
            row = row.child(if session.running {
                Spinner::new()
                    .xsmall()
                    .color(cx.theme().muted_foreground)
                    .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(session.updated_at.relative())
                    .into_any_element()
            });
        }
        if hovered {
            // Pin/archive buttons render only on hover: the yielded width goes to
            // the title, which is clipped only then
            row = row
                .child({
                    let id = session.id.clone();
                    let pinned = session.pinned;
                    Button::new(("pin", ix))
                        .ghost()
                        .xsmall()
                        .icon(if pinned {
                            IconName::StarFill
                        } else {
                            IconName::Star
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetPinned(id.clone(), !pinned));
                        }))
                })
                .child({
                    let id = session.id.clone();
                    let archived = session.archived;
                    Button::new(("archive", ix))
                        .ghost()
                        .xsmall()
                        .icon(if archived {
                            IconName::Undo2
                        } else {
                            IconName::Inbox
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetArchived(id.clone(), !archived));
                        }))
                });
        }
        // Context menu: rename / pin / archive / delete
        let menu = Self::session_menu(&cx.entity().downgrade(), session);
        row.context_menu(menu).into_any_element()
    }

    /// Flat list: a single timeline across workspaces, pinned sessions first (no
    /// section headers), with a second in-row line annotating the workspace;
    /// archived sessions are managed on the settings page's "archived sessions"
    pub(crate) fn render_flat_view(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut pinned: Vec<usize> = vec![];
        let mut rest: Vec<usize> = vec![];
        for (ix, s) in self.sessions.iter().enumerate() {
            if s.archived {
                continue;
            }
            if s.pinned {
                pinned.push(ix);
            } else {
                rest.push(ix);
            }
        }
        let by_recency = |a: &usize, b: &usize| {
            self.sessions[*b]
                .updated_at
                .cmp(&self.sessions[*a].updated_at)
        };
        pinned.sort_by(by_recency);
        rest.sort_by(by_recency);

        // Archived sessions are not rendered in the sidebar: the settings page's
        // "archived sessions" manages them uniformly
        pinned
            .into_iter()
            .chain(rest)
            .map(|ix| self.render_detailed_session_row(window, ix, cx))
            .collect()
    }

    /// Two-line detail session row: title + time on one line, workspace on the
    /// other. Used by the pinned section (cross-workspace central display needs
    /// ownership annotation) and the flat view
    pub(crate) fn render_detailed_session_row(
        &self,
        window: &Window,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = &self.sessions[ix];
        let id = session.id.clone();
        let active = self.active.as_deref() == Some(session.id.as_str());
        let hovered = self.hovered_session.as_deref() == Some(session.id.as_str());
        // Status indicator: yellow dot for pending approval; while running no dot is
        // drawn and the spinner takes the row-tail time slot (see below)
        let status_color = if session.waiting_approval {
            Some(cx.theme().warning)
        } else {
            None
        };
        let (fade_base, fade_tint) = if hovered || active {
            (cx.theme().sidebar, Some(cx.theme().accent.opacity(0.6)))
        } else {
            (cx.theme().sidebar, None)
        };
        let marquee_key = format!("pinned-{}", session.id);
        let workspace_name = self.workspace_name(&session.cwd.display().to_string());

        let mut line1 = h_flex()
            .gap_2()
            .child(self.render_title_scroll(
                &marquee_key,
                ("pinned-title", ix),
                &display_title(&session.title),
                fade_base,
                fade_tint,
                window,
                cx,
            ))
            .when_some(status_color, |this, color| {
                this.child(div().size_2().rounded_full().bg(color))
            });
        if hovered {
            // Pin/archive buttons render only on hover (same as single-line session
            // rows); the time yields and is hidden, and only then is the title
            // clipped
            let pin_id = session.id.clone();
            let pinned = session.pinned;
            let archive_id = session.id.clone();
            let archived = session.archived;
            line1 = line1
                .child(
                    Button::new(("pin", ix))
                        .ghost()
                        .xsmall()
                        .icon(if pinned {
                            IconName::StarFill
                        } else {
                            IconName::Star
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetPinned(pin_id.clone(), !pinned));
                        })),
                )
                .child(
                    Button::new(("archive", ix))
                        .ghost()
                        .xsmall()
                        .icon(if archived {
                            IconName::Undo2
                        } else {
                            IconName::Inbox
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetArchived(archive_id.clone(), !archived));
                        })),
                );
        } else if session.running {
            // Running: the time slot shows a spinner
            line1 = line1.child(Spinner::new().xsmall().color(cx.theme().muted_foreground));
        } else {
            line1 = line1.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(session.updated_at.relative()),
            );
        }

        let hover_id = session.id.clone();
        let row = div()
            .id(("pinned-session", ix))
            .mx_2()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .when(active, |this| this.bg(cx.theme().accent.opacity(0.6)))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::Select(id.clone()));
            }))
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                let was = this.hovered_session.as_deref() == Some(hover_id.as_str());
                if *is_hovered != was {
                    this.hovered_session = (*is_hovered).then(|| hover_id.clone());
                    cx.notify();
                }
            }))
            .child(
                v_flex().gap_0p5().child(line1).child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::FolderClosed)
                                .size_3()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(workspace_name),
                        ),
                ),
            );
        // Context menu: rename / pin / archive / delete
        let menu = Self::session_menu(&cx.entity().downgrade(), session);
        row.context_menu(menu).into_any_element()
    }

    pub(crate) fn render_workspace_view(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = vec![];

        // Pinned section: pinned sessions across workspaces in one place (archived
        // ones hidden), rows annotated with their workspace
        let mut pinned: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.pinned && !s.archived)
            .map(|(ix, _)| ix)
            .collect();
        pinned.sort_by_key(|ix| std::cmp::Reverse(self.sessions[*ix].updated_at));
        if !pinned.is_empty() {
            out.push(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("sidebar.pinned"))
                    .into_any_element(),
            );
            out.extend(
                pinned
                    .iter()
                    .map(|ix| self.render_detailed_session_row(window, *ix, cx)),
            );
        }

        for (p_ix, workspace) in self.workspaces.iter().enumerate() {
            let name = self.workspace_name(workspace);
            let expanded = self.expanded.contains(workspace);
            // While the collapse animation plays the content stays mounted: expanded
            // remains true while open flips to false immediately (instant icon
            // feedback; clicking again now = cancel the collapse and re-expand)
            let collapsing = self
                .expand_anims
                .get(workspace)
                .is_some_and(|a| a.collapsing);
            let open = expanded && !collapsing;
            let renaming = self.renaming == Some(RenameTarget::Workspace(workspace.to_string()));
            let workspace_path = workspace.clone();

            // Sessions under this workspace: pinned (moved to the pinned section)
            // and archived ones excluded, sorted by updated descending
            let mut sessions: Vec<usize> = self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.cwd.display().to_string() == *workspace && !s.pinned && !s.archived
                })
                .map(|(ix, _)| ix)
                .collect();
            sessions.sort_by_key(|ix| std::cmp::Reverse(self.sessions[*ix].updated_at));

            let hovered = self.hovered_workspace.as_deref() == Some(workspace.as_str());
            let mut row = h_flex()
                .id(("workspace", p_ix))
                .relative()
                .mx_2()
                .px_2()
                .py_1()
                .gap_2()
                .rounded(cx.theme().radius)
                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                .child(
                    Icon::new(if open {
                        IconName::FolderOpen
                    } else {
                        IconName::FolderClosed
                    })
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
                );
            if renaming {
                row = row.child(div().flex_1().child(Input::new(&self.rename_input).small()));
            } else {
                row = row.child(
                    div()
                        .text_sm()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(name),
                );
            }
            if renaming {
                out.push(row.into_any_element());
            } else {
                let menu = Self::workspace_menu(&cx.entity().downgrade(), workspace);
                let new_task_path = workspace.clone();
                let hover_path = workspace.clone();
                let mut row = row
                    .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                        let was = this.hovered_workspace.as_deref() == Some(hover_path.as_str());
                        if *is_hovered != was {
                            this.hovered_workspace = (*is_hovered).then(|| hover_path.clone());
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let collapsing = this
                            .expand_anims
                            .get(&workspace_path)
                            .is_some_and(|a| a.collapsing);
                        if this.expanded.contains(&workspace_path) && !collapsing {
                            // Collapse: keep content mounted playing the slide-shut
                            // animation, unload only when the timer expires;
                            // clicking open again meanwhile (generation mismatch)
                            // auto-invalidates it
                            let anim = this.expand_anims.entry(workspace_path.clone()).or_default();
                            anim.generation += 1;
                            anim.collapsing = true;
                            let generation = anim.generation;
                            let path = workspace_path.clone();
                            cx.spawn(async move |this, cx| {
                                cx.background_executor()
                                    .timer(EXPAND_ANIM_DUR + std::time::Duration::from_millis(50))
                                    .await;
                                this.update(cx, |this, cx| {
                                    let stale = this.expand_anims.get(&path).is_none_or(|a| {
                                        !a.collapsing || a.generation != generation
                                    });
                                    if !stale {
                                        this.expanded.remove(&path);
                                        if let Some(a) = this.expand_anims.get_mut(&path) {
                                            a.collapsing = false;
                                        }
                                        cx.notify();
                                    }
                                })
                                .ok();
                            })
                            .detach();
                        } else {
                            this.expanded.insert(workspace_path.clone());
                            let anim = this.expand_anims.entry(workspace_path.clone()).or_default();
                            anim.generation += 1;
                            anim.collapsing = false;
                        }
                        cx.notify();
                    }))
                    .context_menu(menu.clone());
                if hovered {
                    // The row-tail overlay renders only on hover: name right-edge
                    // fade + options (...)/new task (+).
                    // The overlay is absolutely positioned and takes no space; when
                    // not hovered the name uses the full row width unclipped; the
                    // fade and the button backing each stack two layers of background
                    // (sidebar + accent 60%), compositing to exactly the row's
                    // .hover() background, so there is no color shift where the
                    // overlay covers text
                    let sidebar_bg = cx.theme().sidebar;
                    let hover_tint = cx.theme().accent.opacity(0.6);
                    row = row.child(
                        h_flex()
                            .absolute()
                            .right_2()
                            .top_0()
                            .bottom_0()
                            .child(
                                div()
                                    .w(px(20.))
                                    .h_full()
                                    .bg(linear_gradient(
                                        90.,
                                        linear_color_stop(sidebar_bg.opacity(0.), 0.),
                                        linear_color_stop(sidebar_bg, 1.),
                                    ))
                                    .child(div().size_full().bg(linear_gradient(
                                        90.,
                                        linear_color_stop(hover_tint.opacity(0.), 0.),
                                        linear_color_stop(hover_tint, 1.),
                                    ))),
                            )
                            .child(
                                div().h_full().bg(sidebar_bg).child(
                                    div().h_full().bg(hover_tint).child(
                                        h_flex()
                                            .h_full()
                                            .child(
                                                Button::new(("workspace-menu", p_ix))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Ellipsis)
                                                    .dropdown_menu(menu.clone()),
                                            )
                                            .child(
                                                Button::new(("workspace-add", p_ix))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Plus)
                                                    .on_click(cx.listener(move |_, _, _, cx| {
                                                        cx.emit(SidebarEvent::NewTaskInWorkspace(
                                                            new_task_path.clone(),
                                                        ));
                                                    })),
                                            ),
                                    ),
                                ),
                            ),
                    );
                }
                out.push(row.into_any_element());
            }

            if expanded || collapsing {
                // Pagination: one page (5 rows) by default, "show more" adds one
                // page each time, collapse returns to one page.
                // The base page renders straight; the "extra pages" get an
                // independent sub-animation block: show more = slide open and fade
                // in, collapse = content stays mounted sliding shut and fading out
                // (returns to one page only when the timer expires); throughout
                // both directions, old rows are clipped by the container instead of
                // being swapped instantly
                let shown = self
                    .workspace_shown
                    .get(workspace)
                    .copied()
                    .unwrap_or(WORKSPACE_PAGE_SIZE);
                let total = sessions.len();
                let paginate_collapsing = self
                    .paginate_anims
                    .get(workspace)
                    .is_some_and(|a| a.collapsing);
                let mut block = v_flex().gap_1();
                for ix in sessions.iter().take(shown.min(WORKSPACE_PAGE_SIZE)) {
                    block = block.child(self.render_session_row(window, *ix, cx));
                }
                if shown > WORKSPACE_PAGE_SIZE || paginate_collapsing {
                    let mut extra = v_flex().gap_1();
                    for ix in sessions
                        .iter()
                        .skip(WORKSPACE_PAGE_SIZE)
                        .take(shown.saturating_sub(WORKSPACE_PAGE_SIZE))
                    {
                        extra = extra.child(self.render_session_row(window, *ix, cx));
                    }
                    match self.paginate_anims.get(workspace) {
                        Some(anim) => {
                            block = block.child(crate::anim::expand_anim_wrap(
                                format!("ws-page:{}:{}", workspace, anim.generation),
                                anim,
                                extra.into_any_element(),
                            ));
                        }
                        None => block = block.child(extra),
                    }
                }
                let can_more = shown < total;
                let can_collapse = shown > WORKSPACE_PAGE_SIZE;
                if can_more || can_collapse {
                    let mut controls = h_flex()
                        .mx_2()
                        .px_2()
                        .py_1()
                        .gap_3()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground);
                    if can_more {
                        let path = workspace.clone();
                        controls = controls.child(
                            h_flex()
                                .id(("workspace-more", p_ix))
                                .gap_1()
                                .px_1()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    // Show more: extra rows slide open in the
                                    // sub-animation block
                                    *this
                                        .workspace_shown
                                        .entry(path.clone())
                                        .or_insert(WORKSPACE_PAGE_SIZE) += WORKSPACE_PAGE_SIZE;
                                    let anim = this.paginate_anims.entry(path.clone()).or_default();
                                    anim.generation += 1;
                                    anim.collapsing = false;
                                    cx.notify();
                                }))
                                .child(Icon::new(IconName::ChevronDown).size_3())
                                .child(rust_i18n::t!("sidebar.show_more"))
                                .test_support(),
                        );
                    }
                    if can_more && can_collapse {
                        controls = controls.child("·");
                    }
                    if can_collapse {
                        let path = workspace.clone();
                        controls = controls.child(
                            h_flex()
                                .id(("workspace-collapse", p_ix))
                                .gap_1()
                                .px_1()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    // Collapse (pagination): extra rows stay mounted
                                    // playing slide-shut fade-out, returning to one
                                    // page only when the timer expires (re-expanding
                                    // meanwhile invalidates by generation mismatch)
                                    let anim = this.paginate_anims.entry(path.clone()).or_default();
                                    anim.generation += 1;
                                    anim.collapsing = true;
                                    let generation = anim.generation;
                                    let path = path.clone();
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(
                                                EXPAND_ANIM_DUR
                                                    + std::time::Duration::from_millis(50),
                                            )
                                            .await;
                                        this.update(cx, |this, cx| {
                                            let stale =
                                                this.paginate_anims.get(&path).is_none_or(|a| {
                                                    !a.collapsing || a.generation != generation
                                                });
                                            if !stale {
                                                this.workspace_shown.remove(&path);
                                                if let Some(a) = this.paginate_anims.get_mut(&path)
                                                {
                                                    a.collapsing = false;
                                                }
                                                cx.notify();
                                            }
                                        })
                                        .ok();
                                    })
                                    .detach();
                                    cx.notify();
                                }))
                                .child(Icon::new(IconName::ChevronUp).size_3())
                                .child(rust_i18n::t!("sidebar.collapse"))
                                .test_support(),
                        );
                    }
                    block = block.child(div().pl_6().child(controls));
                }
                match self.expand_anims.get(workspace) {
                    Some(anim) => out.push(
                        // Observation wrapper (passes through without the
                        // test_support feature; tests use it to measure the
                        // container's actual height)
                        div()
                            .id(("ws-block", p_ix))
                            .test_support()
                            .child(crate::anim::expand_anim_wrap(
                                format!("ws-expand:{}:{}", workspace, anim.generation),
                                anim,
                                block.into_any_element(),
                            ))
                            .into_any_element(),
                    ),
                    None => out.push(block.into_any_element()),
                }
            }
        }
        out
    }
}
