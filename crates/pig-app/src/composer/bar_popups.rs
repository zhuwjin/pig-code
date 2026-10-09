use super::*;

impl Composer {
    /// Workspace picker panel: a Command panel (search box + workspace list + action
    /// rows), anchored right above the workspace chip.
    pub(crate) fn render_cwd_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let command = Command::new(&self.cwd_command)
            .placeholder(rust_i18n::t!("composer.search_workspace"))
            .items(self.hero_cwds.iter().map(|cwd| {
                let name = std::path::Path::new(cwd)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| cwd.clone());
                CommandItem::new()
                    .label(name)
                    .keywords([cwd.clone()])
                    .icon(IconName::Folder)
                    .checked(self.hero_cwd.as_deref() == Some(cwd.as_str()))
            }))
            .separator()
            .item(
                CommandItem::new()
                    .label(rust_i18n::t!("composer.open_folder"))
                    .icon(IconName::FolderOpen),
            )
            .when(self.hero_cwd.is_some(), |this| {
                this.item(
                    CommandItem::new()
                        .label(rust_i18n::t!("composer.no_workspace"))
                        .icon(IconName::CircleX),
                )
            })
            .empty(|_, _, cx| {
                div()
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("composer.no_matching_workspaces"))
            })
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    let len = this.hero_cwds.len();
                    if let Some(cwd) = this.hero_cwds.get(ix.row) {
                        cx.emit(ComposerEvent::SelectCwd(cwd.clone()));
                    } else if ix.row == len {
                        cx.emit(ComposerEvent::PickDirectory);
                    } else {
                        cx.emit(ComposerEvent::ClearCwd);
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-cwd-popup",
            &self.cwd_command,
            command,
            PopupAnchor::Left,
            cx,
        )
    }

    /// Branch picker panel: a Command panel (search box + branch list), anchored
    /// right above the branch chip.
    pub(crate) fn render_branch_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let command = Command::new(&self.branch_command)
            .placeholder(rust_i18n::t!("composer.search_branch"))
            .items(self.hero_branches.iter().map(|branch| {
                CommandItem::new()
                    .label(branch.clone())
                    .keywords([branch.clone()])
                    .icon(IconName::Github)
                    .checked(self.hero_branch.as_deref() == Some(branch.as_str()))
            }))
            .empty(|_, _, cx| {
                div()
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("composer.no_matching_branches"))
            })
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    if let Some(branch) = this.hero_branches.get(ix.row) {
                        cx.emit(ComposerEvent::CheckoutBranch(branch.clone()));
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-branch-popup",
            &self.branch_command,
            command,
            PopupAnchor::Left,
            cx,
        )
    }

    /// Exec mode panel: the "Plan" toggle on top (orthogonal to permissions, same
    /// layout as ZCode's V4ComposerModeControls: plan checkbox above, separator,
    /// permission radio group below), with the "access outside workspace" toggles in
    /// the bottom footer slot. The plan/toggle rows do not take part in Command
    /// keyboard selection (mouse interaction).
    pub(crate) fn render_exec_mode_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        // Plan toggle row (mouse interaction, not in Command keyboard navigation):
        // lightbulb + "Plan" + description + Checkbox
        let plan_on = self.plan_enabled;
        let command = Command::new(&self.exec_command)
            .searchable(false)
            // Header slot: "Plan" toggle row + separator, placed inside Command's own
            // border (ZCode menu layout: plan checkbox above, separator, permission
            // radio group below); the hover highlight uses the same accent background
            // as list rows
            .header({
                let composer = cx.entity();
                move |_, _window, cx| {
                    let composer = composer.clone();
                    let composer_hover = composer.clone();
                    let (accent, radius, muted, info) = {
                        let theme = cx.theme();
                        (
                            theme.accent,
                            theme.radius,
                            theme.muted_foreground,
                            theme.info,
                        )
                    };
                    v_flex()
                        .w_full()
                        // Vertical inset matches the list container's p_1: the
                        // highlight block does not touch the popup's top edge
                        .pt_1()
                        .pb_1()
                        .child(
                            div()
                                .id("plan-mode-toggle")
                                .test_support()
                                // Highlight block as wide as the list rows: the list
                                // container has a p_1 inset while the header slot is
                                // full width, so the row must inset itself (the
                                // separator stays full-bleed); mb mirrors the p_1 top
                                // gap of the list below, so the highlight does not
                                // hug the separator
                                .mx_1()
                                .mb_1()
                                .cursor_pointer()
                                .rounded(radius)
                                .hover(move |style| style.bg(accent))
                                .child(
                                    h_flex()
                                        .w_full()
                                        .px_2()
                                        .py_1p5()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            Icon::new(AssetIconName::Lightbulb)
                                                .size_4()
                                                .text_color(info),
                                        )
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .child(
                                                    div()
                                                        .text_color(info)
                                                        .child(rust_i18n::t!("composer.plan")),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(muted)
                                                        .child(rust_i18n::t!("composer.plan_desc")),
                                                ),
                                        )
                                        .child(
                                            Checkbox::new("plan-mode")
                                                .checked(plan_on)
                                                .tab_stop(false),
                                        ),
                                )
                                .on_hover(move |hovered, window, cx| {
                                    // The highlight follows only the mouse: hovering
                                    // this row clears the list's leftover selection
                                    // block (row on_hover selects, and nothing clears
                                    // it when the mouse moves onto the header row,
                                    // the root cause of two highlights)
                                    if *hovered {
                                        composer_hover.update(cx, |this, cx| {
                                            this.exec_command.update(cx, |state, cx| {
                                                state.set_selected_index(None, window, cx);
                                            });
                                        });
                                    }
                                })
                                .on_click(move |_, _window, cx| {
                                    composer.update(cx, |this, cx| {
                                        this.plan_enabled = !this.plan_enabled;
                                        cx.emit(ComposerEvent::SetPlanMode(this.plan_enabled));
                                        cx.notify();
                                    });
                                })
                                .into_any_element(),
                        )
                        .child(Separator::horizontal())
                        .into_any_element()
                }
            })
            .items(EXEC_MODES.iter().enumerate().map(|(ix, mode)| {
                let icon = exec_mode_icon(*mode);
                CommandItem::new()
                    .label(exec_mode_label(*mode))
                    .checked(ix == self.exec_mode)
                    .child(move |_, cx| {
                        // Icon and label are colored by the mode's danger level
                        // (explicit text_color; the inherited hover/selected color
                        // cannot override the mode color); the description stays muted
                        let mode_color = exec_mode_color(*mode, cx);
                        h_flex()
                            .flex_1()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(icon).size_4().text_color(mode_color))
                            .child(
                                v_flex()
                                    .child(
                                        div().text_color(mode_color).child(exec_mode_label(*mode)),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(exec_mode_description(*mode)),
                                    ),
                            )
                    })
            }))
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    if let Some(mode) = EXEC_MODES.get(ix.row) {
                        if *mode == ExecMode::Yolo && this.exec_mode != ix.row {
                            // Unrestricted mode: close the popup first, then show the
                            // confirm dialog (SetExecMode is emitted only after
                            // confirmation)
                            this.close_command_popup(window, cx);
                            cx.emit(ComposerEvent::RequestYoloConfirm);
                            return;
                        }
                        this.exec_mode = ix.row;
                        cx.emit(ComposerEvent::SetExecMode(*mode));
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            })
            // Footer slot: separator + a one-line toggle area ("access outside
            // workspace  read ☐  write ☑"); the checkbox itself is the toggle state
            // visual, no longer using CommandItem.checked's ✓
            .footer({
                let composer = cx.entity();
                let read_on = self.fs_read_outside;
                let write_on = self.fs_write_outside;
                move |_, _window, cx| {
                    let muted = cx.theme().muted_foreground;
                    v_flex()
                        .w_full()
                        .pt_1()
                        .child(Separator::horizontal())
                        .child(
                            h_flex()
                                .w_full()
                                .px_2()
                                .py_1p5()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(rust_i18n::t!("composer.fs_outside_access")),
                                )
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .items_center()
                                        .child(fs_toggle(
                                            "fs-access-read",
                                            rust_i18n::t!("composer.fs_read"),
                                            read_on,
                                            true,
                                            composer.clone(),
                                            cx,
                                        ))
                                        .child(fs_toggle(
                                            "fs-access-write",
                                            rust_i18n::t!("composer.fs_write"),
                                            write_on,
                                            false,
                                            composer.clone(),
                                            cx,
                                        )),
                                ),
                        )
                        .into_any_element()
                }
            });
        // Fixed 300px width: the toggle area's description text wraps by width
        // instead of being clipped by the popup
        let content = v_flex().w(px(300.)).child(command).into_any_element();
        self.popup_shell(
            "composer-exec-popup",
            content,
            PopupAnchor::Left,
            Some(self.exec_command.clone()),
            cx,
        )
    }

    /// Model panel: search box + model list grouped by provider + a "manage models"
    /// action row.
    pub(crate) fn render_model_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        // Group by provider, preserving the order of appearance in the config
        let mut groups: Vec<(String, Vec<ModelOption>)> = Vec::new();
        for model in &self.models {
            match groups.iter_mut().find(|(name, _)| *name == model.0) {
                Some((_, items)) => items.push(model.clone()),
                None => groups.push((model.0.clone(), vec![model.clone()])),
            }
        }

        let mut command =
            Command::new(&self.model_command).placeholder(rust_i18n::t!("composer.search_model"));
        if groups.is_empty() {
            command = command.item(
                CommandItem::new()
                    .label(rust_i18n::t!("composer.no_models_configured"))
                    .icon(IconName::Info),
            );
        } else {
            for (provider_name, items) in &groups {
                command = command.group(CommandGroup::new().label(provider_name.clone()).items(
                    items.iter().map(|(provider_name, _, model_id, _)| {
                        CommandItem::new()
                            .label(model_id.clone())
                            .keywords([provider_name.clone()])
                            .checked(self.model == format!("{provider_name}/{model_id}"))
                    }),
                ));
            }
        }
        // Ungrouped items are fixed at section 0; groups start at section 1
        // (regardless of the order entries are written in)
        command = command.separator().item(
            CommandItem::new()
                .label(rust_i18n::t!("composer.manage_models"))
                .icon(AssetIconName::Settings),
        );
        let command = command
            .empty(|_, _, cx| {
                div()
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("composer.no_matching_models"))
            })
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    if ix.section == 0 {
                        tracing::debug!("clicked \"Manage models\" (section=0)");
                        cx.emit(ComposerEvent::OpenSettings);
                    } else if let Some((_, items)) = groups.get(ix.section - 1)
                        && let Some((provider_name, provider_id, model_id, _)) = items.get(ix.row)
                    {
                        tracing::debug!(
                            "selected section={} row={} -> {provider_id}/{model_id}",
                            ix.section,
                            ix.row
                        );
                        this.model = format!("{provider_name}/{model_id}");
                        cx.emit(ComposerEvent::SetModel {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                        });
                    } else {
                        tracing::debug!(
                            "click did not resolve to a model: section={} row={}",
                            ix.section,
                            ix.row
                        );
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-model-popup",
            &self.model_command,
            command,
            PopupAnchor::Right,
            cx,
        )
    }

    /// Reasoning level panel: no search box, "off" + level list with the current
    /// level checked.
    /// levels is (id, display name); the UI shows the display name and confirmation
    /// returns the id.
    pub(crate) fn render_reasoning_popup(
        &self,
        levels: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();
        let levels = levels.to_vec();

        let command = Command::new(&self.reasoning_command)
            .searchable(false)
            .item(
                CommandItem::new()
                    .label(rust_i18n::t!("composer.reasoning_off"))
                    .checked(self.reasoning_level.is_none()),
            )
            .items(levels.iter().map(|(id, label)| {
                // When the display name differs from the id, append the id in
                // parentheses to avoid ambiguity
                let text = if label == id {
                    id.clone()
                } else {
                    format!("{label}（{id}）")
                };
                CommandItem::new()
                    .label(text)
                    .checked(self.reasoning_level.as_deref() == Some(id.as_str()))
            }))
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    let level = if ix.row == 0 {
                        None
                    } else {
                        levels.get(ix.row - 1).map(|(id, _)| id.clone())
                    };
                    this.reasoning_level = level.clone();
                    cx.emit(ComposerEvent::SetReasoning(level));
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-reasoning-popup",
            &self.reasoning_command,
            command,
            PopupAnchor::Right,
            cx,
        )
    }
}
