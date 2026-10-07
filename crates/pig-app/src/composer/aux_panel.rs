use super::*;

impl Composer {
    /// Chip row for current progress (TodoList) + background Bash tasks + session
    /// changes.
    /// Clicking a progress/task chip opens a read-only popup above the chip (v1 has
    /// no stop button); the changes chip emits an event for AppView to open the
    /// changes tab of the right panel.
    pub(crate) fn render_aux(&self, cx: &mut Context<Self>) -> AnyElement {
        let bash_running = self
            .tasks
            .iter()
            .filter(|t| TaskChipKind::Bash.matches(t) && matches!(t.status, TaskStatus::Running))
            .count();
        let agent_running = self
            .tasks
            .iter()
            .filter(|t| TaskChipKind::Agent.matches(t) && matches!(t.status, TaskStatus::Running))
            .count();
        let done = self
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Done)
            .count();

        let mut chips = h_flex().w_full().gap_2();
        // Chips are split by task type (same as kimi-code): "background Bash" and
        // "background Agent" have independent visibility and popups; a chip does not
        // appear when its category's task list is empty
        if self.tasks.iter().any(|t| TaskChipKind::Bash.matches(t)) {
            let open = matches!(self.popup, Some((Popup::Tasks, _)));
            chips = chips.child(
                div()
                    .relative()
                    .child(self.render_aux_chip(
                        "aux-tasks",
                        AssetIconName::Terminal,
                        TaskChipKind::Bash.label(bash_running),
                        open,
                        Popup::Tasks,
                        cx,
                    ))
                    .when(open, |this| {
                        this.child(self.render_tasks_panel(TaskChipKind::Bash, cx))
                    }),
            );
        }
        if self.tasks.iter().any(|t| TaskChipKind::Agent.matches(t)) {
            let open = matches!(self.popup, Some((Popup::AgentTasks, _)));
            chips = chips.child(
                div()
                    .relative()
                    .child(self.render_aux_chip(
                        "aux-agent-tasks",
                        IconName::Bot,
                        TaskChipKind::Agent.label(agent_running),
                        open,
                        Popup::AgentTasks,
                        cx,
                    ))
                    .when(open, |this| {
                        this.child(self.render_tasks_panel(TaskChipKind::Agent, cx))
                    }),
            );
        }
        if !self.change_files.is_empty() {
            let (added, removed) = self.changes;
            chips = chips.child(
                h_flex()
                    .id("aux-changes")
                    .gap_1()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent))
                    // No more popup: clicking directly opens the right panel's
                    // changes tab
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(ComposerEvent::OpenChanges);
                    }))
                    .child(
                        Icon::new(AssetIconName::Diff)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().text_sm().child(rust_i18n::t!("composer.changes")))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().success)
                            .child(format!("+{added}")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(format!("-{removed}")),
                    ),
            );
        }
        if !self.todos.is_empty() {
            let open = matches!(self.popup, Some((Popup::Todos, _)));
            chips = chips.child(
                div()
                    .relative()
                    .child(
                        self.render_aux_chip(
                            "aux-todos",
                            AssetIconName::ListTodo,
                            rust_i18n::t!(
                                "composer.todo_progress",
                                done = done,
                                total = self.todos.len()
                            )
                            .to_string(),
                            open,
                            Popup::Todos,
                            cx,
                        ),
                    )
                    .when(open, |this| this.child(self.render_todos_panel(cx))),
            );
        }
        chips.into_any_element()
    }

    pub(crate) fn render_aux_chip(
        &self,
        id: &'static str,
        icon: impl Into<Icon>,
        label: String,
        active: bool,
        kind: Popup,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        h_flex()
            .id(id)
            .gap_1()
            .px_3()
            .py_1()
            .rounded_full()
            .cursor_pointer()
            .when(active, |this| this.bg(cx.theme().accent.opacity(0.5)))
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.toggle_popup(kind, event, None, window, cx);
            }))
            .child(
                Icon::new(icon)
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().text_sm().child(label))
    }

    /// Panel content shell: popup lining (rounded_xl + popover background + border),
    /// visually consistent with the other popups.
    pub(crate) fn aux_panel_shell(&self, content: Div, cx: &mut Context<Self>) -> Div {
        content
            .w_full()
            .gap_2()
            .p_3()
            .rounded_xl()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
    }

    /// Approval bar (same as kimi): replaces the input area while an approval is
    /// pending. Orange dot + title, a dark inset block showing the command/diff, and
    /// at the bottom: always allow in this session (Ctrl+⏎) / reject (Esc) /
    /// approve (⏎).
    pub(crate) fn render_approval_bar(
        &self,
        approval: &PendingApproval,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if approval.tool == "ExitPlanMode" {
            return self.render_plan_approval_bar(approval, cx);
        }
        let title = match approval.tool.as_str() {
            "Bash" => rust_i18n::t!("composer.approve_bash").to_string(),
            "Write" => rust_i18n::t!("composer.approve_write").to_string(),
            "Edit" => rust_i18n::t!("composer.approve_edit").to_string(),
            tool => rust_i18n::t!("composer.approve_other", tool = tool).to_string(),
        };
        let detail = if approval.tool == "Bash" {
            format!("$ {}", approval.detail)
        } else {
            approval.detail.clone()
        };
        let cwd = approval.cwd.clone();
        // High-risk command: add a warning line above the detail (⚠️ high-risk
        // command: {localized reason})
        let danger_line = approval
            .danger_key
            .as_deref()
            .and_then(danger_reason_text)
            .map(|reason| {
                format!(
                    "⚠️ {}：{reason}",
                    rust_i18n::t!("approval.danger.high_risk")
                )
            });

        v_flex()
            .id("approval-bar")
            .w_full()
            .gap_3()
            .p_2()
            .track_focus(&self.approval_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let decision = match event.keystroke.key.as_str() {
                    "enter" if event.keystroke.modifiers.control => {
                        Some(ApprovalDecision::AlwaysAllow)
                    }
                    "enter" => Some(ApprovalDecision::Allow),
                    "escape" => Some(ApprovalDecision::Reject),
                    _ => None,
                };
                if let Some(decision) = decision {
                    this.decide_approval(decision, None, window, cx);
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().bg(cx.theme().warning))
                    .child(div().text_sm().font_medium().child(title)),
            )
            .when_some(danger_line, |this, line| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(line))
            })
            .child(
                div()
                    .id("approval-detail")
                    .w_full()
                    .rounded(px(10.))
                    .bg(cx.theme().background)
                    .p_3()
                    .max_h(px(200.))
                    .overflow_y_scroll()
                    .text_sm()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(detail),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("composer.working_dir", cwd = cwd).to_string()),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    // Same size as the questionnaire action buttons (Small), keeping
                    // button font sizes uniform across confirmation bars
                    .child(
                        Button::new("approval-always")
                            .secondary()
                            .small()
                            .label(format!(
                                "{}  Ctrl+⏎",
                                rust_i18n::t!("composer.approve_always")
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(
                                    ApprovalDecision::AlwaysAllow,
                                    None,
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("approval-reject")
                            .secondary()
                            .small()
                            .label(format!("{}  Esc", rust_i18n::t!("composer.reject")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::Reject, None, window, cx);
                            })),
                    )
                    .child(
                        Button::new("approval-allow")
                            .primary()
                            .small()
                            .label(format!("{}  ⏎", rust_i18n::t!("composer.approve")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::Allow, None, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// Plan approval panel (same as kimi-code figure 2): orange dot + "start
    /// implementing this plan?", the full plan scrolls inline as markdown
    /// (TextView), and at the bottom: revise / reject and exit (Esc) / approve plan
    /// (⏎). "Revise" = reject plus focus returning to the composer automatically
    /// (existing decide_approval behavior), with the user's revision typed directly
    /// as the next message
    fn render_plan_approval_bar(
        &self,
        approval: &PendingApproval,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cwd = approval.cwd.clone();
        let plan = self.plan_state.clone();
        v_flex()
            .id("approval-bar")
            .w_full()
            .gap_3()
            .p_2()
            .track_focus(&self.approval_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Revise input mode: ⏎ submits and rejects (with feedback), Esc
                // cancels back to the three buttons;
                // three-button mode: ⏎ approves, Esc rejects and exits
                if this.plan_revise {
                    match event.keystroke.key.as_str() {
                        "enter" => this.submit_plan_revise(window, cx),
                        "escape" => this.cancel_plan_revise(window, cx),
                        _ => {}
                    }
                    return;
                }
                let decision = match event.keystroke.key.as_str() {
                    "enter" => Some(ApprovalDecision::Allow),
                    "escape" => Some(ApprovalDecision::Reject),
                    _ => None,
                };
                if let Some(decision) = decision {
                    this.decide_approval(decision, None, window, cx);
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().bg(cx.theme().warning))
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(rust_i18n::t!("composer.plan_approve_title")),
                    ),
            )
            // Plan file path link (kimi-style blue link): core has already written
            // it to disk before requesting approval; clicking opens it in the right
            // panel's "files" tab
            .child(
                div()
                    .id("plan-path")
                    .test_support()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(cx.theme().info)
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(approval.plan_path())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let path = this
                            .approval
                            .as_ref()
                            .map(|a| a.plan_path())
                            .unwrap_or_default();
                        cx.emit(ComposerEvent::OpenFile { path });
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("plan-approval-detail")
                    .test_support()
                    .w_full()
                    .rounded(px(10.))
                    .bg(cx.theme().background)
                    .p_3()
                    .max_h(px(480.))
                    .overflow_y_scroll()
                    .when_some(plan, |this, state| {
                        this.child(
                            gpui_kit::component::text::TextView::new(&state)
                                .selectable(true)
                                .text_sm(),
                        )
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("composer.working_dir", cwd = cwd).to_string()),
            )
            // Revise input mode (kimi Revise): feedback input + cancel / submit and
            // reject;
            // three-button mode: revise / reject and exit / approve plan
            .when(self.plan_revise, |this| {
                this.child(
                    div()
                        .w_full()
                        .rounded(px(10.))
                        .bg(cx.theme().background)
                        .px_3()
                        .py_2()
                        .when_some(self.plan_revise_input.clone(), |this, input| {
                            this.child(gpui_kit::component::input::Input::new(&input))
                        }),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .when(self.plan_revise, |this| {
                        this.child(div().flex_1())
                            .child(
                                Button::new("plan-revise-cancel")
                                    .secondary()
                                    .small()
                                    .label(format!("{}  Esc", rust_i18n::t!("common.cancel")))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_plan_revise(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("plan-revise-submit")
                                    .primary()
                                    .small()
                                    .label(format!(
                                        "{}  ↵",
                                        rust_i18n::t!("composer.submit_and_reject")
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_plan_revise(window, cx);
                                    })),
                            )
                    })
                    .when(!self.plan_revise, |this| {
                        this.child(
                            Button::new("plan-revise")
                                .secondary()
                                .small()
                                .label(rust_i18n::t!("composer.revise"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    // kimi Revise: enter input mode; the feedback is
                                    // carried by "submit and reject"
                                    this.plan_revise = true;
                                    if this.plan_revise_input.is_none() {
                                        this.plan_revise_input = Some(cx.new(|cx| {
                                            InputState::new(window, cx).placeholder(rust_i18n::t!(
                                                "composer.revise_placeholder"
                                            ))
                                        }));
                                    }
                                    let input =
                                        this.plan_revise_input.clone().expect("revise input");
                                    input.update(cx, |input, cx| {
                                        input.set_value("", window, cx);
                                        input.focus(window, cx);
                                    });
                                    cx.notify();
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("plan-reject")
                                .secondary()
                                .small()
                                .label(format!(
                                    "{}  Esc",
                                    rust_i18n::t!("composer.reject_and_exit")
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.decide_approval(
                                        ApprovalDecision::Reject,
                                        None,
                                        window,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new("plan-approve")
                                .primary()
                                .small()
                                .label(format!("{}  ↵", rust_i18n::t!("composer.approve_plan")))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.decide_approval(ApprovalDecision::Allow, None, window, cx);
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}
