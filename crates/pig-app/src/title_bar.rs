use super::*;

impl AppView {
    pub(crate) fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode.is_dark();
        let title = self
            .current
            .as_ref()
            .and_then(|id| self.metas.iter().find(|m| &m.id == id))
            .map(|m| crate::sidebar::display_title(&m.title))
            .unwrap_or_else(|| "Pig Code".to_string());
        // Debug builds carry the version in the title bar, making it easy
        // to tell daily debugging/self-tests from release distribution
        let label = if cfg!(debug_assertions) {
            let version = env!("CARGO_PKG_VERSION");
            if self.current.is_some() {
                format!("Pig Code v{version} · {title}")
            } else {
                format!("Pig Code v{version}")
            }
        } else {
            format!("Pig Code · {title}")
        };

        // On Windows the title bar hits HTCAPTION: a left press still
        // dispatches MouseDownEvent, but the release is swallowed by the
        // OS's window-move modal loop, so window-level text selection never
        // sees the gesture end once started, and moving the mouse afterwards
        // becomes a drag-select. Suppressing selection while the title bar
        // is pressed means the gesture never starts.
        div()
            .id("title-bar-selection-guard")
            .w_full()
            .flex_shrink_0()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                GlobalState::suppress_text_selection(cx);
            })
            .child(
                TitleBar::new()
                    .child(if self.settings_open {
                        // Settings mode: the left zone swaps to "back to
                        // workspace" (same style as the other title bar
                        // buttons, instead of stacking a back button inside
                        // the settings sidebar)
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("back-to-workspace")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::ArrowLeft)
                                    .label(rust_i18n::t!("title_bar.back_to_workspace"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings_open = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(rust_i18n::t!("title_bar.settings")),
                            )
                    } else {
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("toggle-sidebar")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::PanelLeft)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sidebar_collapsed = !this.sidebar_collapsed;
                                        cx.notify();
                                    })),
                            )
                            .child(div().text_sm().font_semibold().child(label))
                            // Session actions menu (three dots): only shown
                            // with an active session
                            .when(self.current.is_some(), |this| {
                                this.child(
                                    div()
                                        .on_prepaint({
                                            let cell = self.session_menu_btn_bounds.clone();
                                            move |bounds, _, _| cell.set(bounds)
                                        })
                                        .child(
                                            Button::new("session-menu")
                                                .ghost()
                                                .small()
                                                .occlude()
                                                .icon(IconName::Ellipsis)
                                                .on_click(cx.listener(
                                                    |this, event: &ClickEvent, _, cx| {
                                                        // Same as the branch chip: with the
                                                        // menu open, the press first fires
                                                        // outside-close (recording the
                                                        // position) and the click of the
                                                        // same press is swallowed by
                                                        // position, preventing
                                                        // collapse-then-reopen
                                                        let down_pos = match event {
                                                            ClickEvent::Mouse(e) => {
                                                                Some(e.down.position)
                                                            }
                                                            _ => None,
                                                        };
                                                        if this
                                                            .session_menu_outside_close
                                                            .take()
                                                            .is_some_and(|pos| {
                                                                Some(pos) == down_pos
                                                            })
                                                        {
                                                            return;
                                                        }
                                                        this.session_menu_open =
                                                            !this.session_menu_open;
                                                        cx.notify();
                                                    },
                                                )),
                                        ),
                                )
                            })
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .px_2()
                            // "Open in Finder/file manager" split button:
                            // the main button opens the current workspace
                            // directly, the chevron opens a menu (open in
                            // terminal etc. hang on the same menu)
                            .when(self.current.is_some(), |this| {
                                let fm_label =
                                    rust_i18n::t!("title_bar.open_in", name = file_manager_name())
                                        .to_string();
                                // Colorful icons cannot go through Icon
                                // (svg renders monochrome in the text
                                // color): macOS uses the real Finder icon
                                // fetched via NSWorkspace (img keeps
                                // colors), falling back to the Lucide
                                // folder before it arrives / on other
                                // platforms
                                let fm_icon_el: AnyElement = match &self.fm_icon {
                                    Some(icon) => img(icon.clone()).size_4().into_any_element(),
                                    None => Icon::new(AssetsIconName::FolderOpen)
                                        .size_4()
                                        .into_any_element(),
                                };
                                let view = cx.entity().downgrade();
                                let fm_menu_icon = self.fm_icon.clone();
                                // The dropdown rows carry the bare localized
                                // app name next to the app icon, like app
                                // entries; the full "open in …" sentence stays
                                // on the button tooltip
                                let fm_menu_label = file_manager_name();
                                let term_menu_icon = self.terminal_icon.clone();
                                let term_menu_label = terminal_name();
                                this.child(
                                    DropdownButton::new("fm-split")
                                        .outline()
                                        .small()
                                        .button(
                                            Button::new("fm-open")
                                                .occlude()
                                                .tooltip(fm_label.clone())
                                                .child(fm_icon_el)
                                                .on_click(cx.listener(|this, _, _, _| {
                                                    this.open_current_in_file_manager();
                                                })),
                                        )
                                        .dropdown_menu(move |menu, _, _| {
                                            let view = view.clone();
                                            // App rows: "real icon + name" via
                                            // the ElementItem variant (the icon
                                            // slot only takes monochrome Icons,
                                            // colorful images must go through
                                            // img); a Lucide glyph stands in
                                            // until the real icon arrives (or
                                            // where extraction is unavailable)
                                            let app_row = |icon: &Option<
                                                std::sync::Arc<Image>,
                                            >,
                                             fallback: AssetsIconName,
                                             label: &str|
                                             -> PopupMenuItem {
                                                let label = label.to_string();
                                                match icon.clone() {
                                                    Some(icon) => {
                                                        PopupMenuItem::element(move |_, _| {
                                                            h_flex()
                                                                .gap_2()
                                                                .items_center()
                                                                .child(
                                                                    img(icon.clone()).size_4(),
                                                                )
                                                                .child(label.clone())
                                                        })
                                                    }
                                                    None => PopupMenuItem::element(
                                                        move |_, _| {
                                                            h_flex()
                                                                .gap_2()
                                                                .items_center()
                                                                .child(
                                                                    Icon::new(fallback).size_4(),
                                                                )
                                                                .child(label.clone())
                                                        },
                                                    ),
                                                }
                                            };
                                            menu.item(
                                                app_row(
                                                    &fm_menu_icon,
                                                    AssetsIconName::FolderOpen,
                                                    &fm_menu_label,
                                                )
                                                .on_click({
                                                    let view = view.clone();
                                                    move |_, _, cx| {
                                                        let _ = view.update(cx, |this, _| {
                                                            this.open_current_in_file_manager();
                                                        });
                                                    }
                                                }),
                                            )
                                            .item(
                                                app_row(
                                                    &term_menu_icon,
                                                    AssetsIconName::Terminal,
                                                    &term_menu_label,
                                                )
                                                .on_click({
                                                    let view = view.clone();
                                                    move |_, _, cx| {
                                                        let _ = view.update(cx, |this, _| {
                                                            this.open_current_in_terminal();
                                                        });
                                                    }
                                                }),
                                            )
                                        }),
                                )
                            })
                            .when(self.git_branch.is_some(), |this| {
                                this.child(
                                    div()
                                        .on_prepaint({
                                            let cell = self.title_branch_btn_bounds.clone();
                                            move |bounds, _, _| cell.set(bounds)
                                        })
                                        .child(
                                            Button::new("title-branch")
                                                .ghost()
                                                .small()
                                                .occlude()
                                                .label(format!(
                                                    "⎇ {}",
                                                    self.git_branch.as_deref().unwrap_or_default()
                                                ))
                                                .on_click(cx.listener(
                                                    |this, event: &ClickEvent, _, cx| {
                                                        // Clicking the chip while the menu is
                                                        // open: the press first fires
                                                        // outside-close (recording the
                                                        // position), and the click of the
                                                        // same press is swallowed by
                                                        // position, avoiding
                                                        // collapse-then-reopen
                                                        let down_pos = match event {
                                                            ClickEvent::Mouse(e) => {
                                                                Some(e.down.position)
                                                            }
                                                            _ => None,
                                                        };
                                                        if this
                                                            .title_branch_outside_close
                                                            .take()
                                                            .is_some_and(|pos| {
                                                                Some(pos) == down_pos
                                                            })
                                                        {
                                                            return;
                                                        }
                                                        this.title_branch_menu_open =
                                                            !this.title_branch_menu_open;
                                                        cx.notify();
                                                    },
                                                )),
                                        ),
                                )
                            })
                            .child(
                                Button::new("toggle-theme")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(if is_dark {
                                        IconName::Sun
                                    } else {
                                        IconName::Moon
                                    })
                                    .on_click(move |_, _, cx| {
                                        Theme::change(
                                            if is_dark {
                                                ThemeMode::Light
                                            } else {
                                                ThemeMode::Dark
                                            },
                                            None,
                                            cx,
                                        );
                                        cx.set_global(ThemeFollowSystem(false));
                                    }),
                            )
                            .child(
                                Button::new("open-settings")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::Settings)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_settings(cx);
                                    })),
                            )
                            .child(
                                Button::new("toggle-terminal")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::SquareTerminal)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_terminal_panel(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("right-panel-menu")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(if self.right_open {
                                        IconName::PanelRightClose
                                    } else {
                                        IconName::PanelRight
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_right_panel(cx);
                                    })),
                            ),
                    ),
            )
            .when(self.title_branch_menu_open, |this| {
                this.child(self.render_branch_menu(cx))
            })
            .when(self.session_menu_open, |this| {
                this.child(self.render_session_menu(cx))
            })
    }

    /// Title bar session menu (three-dot popup): anchored below the button,
    /// left-aligned (the menu expands rightward), closes on outside click.
    /// Pin/archive/rename share the same path as the sidebar context menu;
    /// the last item is "view trajectory"
    pub(crate) fn render_session_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let meta = self
            .current
            .as_ref()
            .and_then(|id| self.metas.iter().find(|m| &m.id == id));
        let pinned = meta.is_some_and(|m| m.pinned);
        let archived = meta.is_some_and(|m| m.archived);
        let pin_id = self.current.clone().unwrap_or_default();
        let archive_id = pin_id.clone();
        let rename_id = pin_id.clone();
        let rename_title = meta.map(|m| m.title.clone()).unwrap_or_default();
        deferred(
            Positioner::side(self.session_menu_btn_bounds.get())
                .placement(Placement::Bottom)
                .align(Align::Start)
                .offset(px(6.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("title-session-menu")
                        .w(px(220.))
                        .p_1()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.session_menu_open = false;
                            this.session_menu_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .child(
                            h_flex()
                                .id("menu-toggle-pin")
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .text_sm()
                                .cursor_pointer()
                                .hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                .child(
                                    Icon::new(if pinned {
                                        IconName::StarFill
                                    } else {
                                        IconName::Star
                                    })
                                    .size_4(),
                                )
                                .child(div().child(if pinned {
                                    rust_i18n::t!("title_bar.unpin")
                                } else {
                                    rust_i18n::t!("title_bar.pin")
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.session_menu_open = false;
                                    this.agent.set_pinned(&pin_id, !pinned);
                                    cx.notify();
                                })),
                        )
                        .child(
                            h_flex()
                                .id("menu-toggle-archive")
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .text_sm()
                                .cursor_pointer()
                                .hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                .child(
                                    Icon::new(if archived {
                                        IconName::Undo2
                                    } else {
                                        IconName::Inbox
                                    })
                                    .size_4(),
                                )
                                .child(div().child(if archived {
                                    rust_i18n::t!("title_bar.unarchive")
                                } else {
                                    rust_i18n::t!("title_bar.archive")
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.session_menu_open = false;
                                    this.agent.set_archived(&archive_id, !archived);
                                    cx.notify();
                                })),
                        )
                        // Archived sessions are not rendered in the sidebar
                        // (managed in the settings page), and inline rename
                        // has no row to live on — hide this entry
                        .when(!archived, |this| {
                            this.child(
                                h_flex()
                                    .id("menu-rename-session")
                                    .gap_2()
                                    .px_2()
                                    .py_1()
                                    .rounded(cx.theme().radius)
                                    .text_sm()
                                    .cursor_pointer()
                                    .hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                    .child(Icon::new(AssetsIconName::SquarePen).size_4())
                                    .child(div().child(rust_i18n::t!("title_bar.rename")))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.session_menu_open = false;
                                        // The inline rename input is drawn
                                        // on the sidebar session row: expand
                                        // the sidebar first when collapsed
                                        this.sidebar_collapsed = false;
                                        this.sidebar.update(cx, |sidebar, cx| {
                                            sidebar.start_session_rename(
                                                rename_id.clone(),
                                                rename_title.clone(),
                                                window,
                                                cx,
                                            );
                                        });
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(div().h(px(1.)).my_1().bg(cx.theme().border))
                        .child(
                            h_flex()
                                .id("menu-view-trajectory")
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .text_sm()
                                .cursor_pointer()
                                .hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                .child(Icon::new(IconName::FileText).size_4())
                                .child(div().child(rust_i18n::t!("title_bar.view_trajectory")))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.session_menu_open = false;
                                    this.open_trajectory(cx);
                                })),
                        ),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// Open the current session's workspace in the system file manager
    /// (macOS Finder / Windows Explorer / Linux xdg-open; detached spawn,
    /// failures only logged)
    pub(crate) fn open_current_in_file_manager(&self) {
        let Some(cwd) = self.current_cwd() else {
            return;
        };
        if let Err(err) = pig_core::files::open_in_file_manager(&cwd) {
            tracing::debug!(
                "failed to open via {} {}: {err}",
                file_manager_name(),
                cwd.display()
            );
        }
    }

    /// Title-bar menu "open in terminal": same shape as the file-manager one
    pub(crate) fn open_current_in_terminal(&self) {
        let Some(cwd) = self.current_cwd() else {
            return;
        };
        if let Err(err) = pig_core::files::open_in_terminal(&cwd) {
            tracing::debug!(
                "failed to open via {} {}: {err}",
                terminal_name(),
                cwd.display()
            );
        }
    }

    /// Title bar branch switch menu: deferred to the window layer, anchored
    /// right below the branch chip (same pattern as the tab "+" menu). The
    /// current branch is highlighted; clicking another branch checks it
    /// out.
    pub(crate) fn render_branch_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.git_branch.clone().unwrap_or_default();
        deferred(
            Positioner::side(self.title_branch_btn_bounds.get())
                .placement(Placement::Bottom)
                .align(Align::End)
                .offset(px(6.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("title-branch-menu")
                        .w(px(240.))
                        .max_h(px(360.))
                        .overflow_y_scroll()
                        .p_1()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.title_branch_menu_open = false;
                            this.title_branch_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(rust_i18n::t!("title_bar.switch_branch")),
                        )
                        .children(self.title_branches.iter().map(|branch| {
                            let branch = branch.clone();
                            let is_current = branch == current;
                            div()
                                .id(gpui_kit::SharedString::from(branch.clone()))
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .text_sm()
                                .cursor_pointer()
                                .when(is_current, |this| this.bg(cx.theme().accent))
                                .when(!is_current, |this| {
                                    this.hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                })
                                .child(
                                    h_flex()
                                        .w_full()
                                        .justify_between()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_color(if is_current {
                                                    cx.theme().primary
                                                } else {
                                                    cx.theme().foreground
                                                })
                                                .child(branch.clone()),
                                        )
                                        .when(is_current, |this| {
                                            this.child(
                                                Icon::new(IconName::Check)
                                                    .size_3()
                                                    .text_color(cx.theme().primary),
                                            )
                                        }),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.title_branch_menu_open = false;
                                    if let Some(cwd) = this.current_cwd() {
                                        this.agent.checkout_branch(cwd, branch.clone());
                                    }
                                    cx.notify();
                                }))
                        })),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }
}

/// Platform name for "open in the system file manager" (localized): macOS
/// Finder / Windows File Explorer / Linux file manager. Decided locally by
/// the UI after core became language-agnostic (formerly
/// pig_core::files::file_manager_name).
pub(crate) fn file_manager_name() -> String {
    if cfg!(target_os = "macos") {
        rust_i18n::t!("files.manager_macos").to_string()
    } else if cfg!(target_os = "windows") {
        rust_i18n::t!("files.manager_windows").to_string()
    } else {
        rust_i18n::t!("files.manager_linux").to_string()
    }
}

/// Platform name for "open in a terminal" (localized), matching the hardcoded
/// preference list in pig_core::files::open_in_terminal: Windows Terminal on
/// Windows (the wt.exe preference), Terminal.app on macOS, the generic
/// "Terminal" for Linux's probe list
pub(crate) fn terminal_name() -> String {
    if cfg!(target_os = "macos") {
        rust_i18n::t!("files.terminal_macos").to_string()
    } else if cfg!(target_os = "windows") {
        rust_i18n::t!("files.terminal_windows").to_string()
    } else {
        rust_i18n::t!("files.terminal_linux").to_string()
    }
}
