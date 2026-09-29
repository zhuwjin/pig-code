use super::*;

impl Composer {
    /// 当前进度（TodoList）+ 后台 Bash 任务 + 会话改动：chip 行。
    /// 进度/任务 chip 点击在芯片上方弹出只读面板（v1 无停止按钮）；
    /// 改动 chip 发事件让 AppView 打开右侧面板的改动 tab。
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
        // chip 按任务类型拆分（kimi-code 同款）：「后台 Bash」「后台 Agent」各自独立
        // 显隐与弹层；对应类别的任务列表为空时该 chip 不出现
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
                    // 不再弹层：点击直接打开右侧面板的改动 tab
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(ComposerEvent::OpenChanges);
                    }))
                    .child(
                        Icon::new(AssetIconName::Diff)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().text_sm().child("改动"))
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
                    .child(self.render_aux_chip(
                        "aux-todos",
                        AssetIconName::ListTodo,
                        format!("当前进度 {done}/{}", self.todos.len()),
                        open,
                        Popup::Todos,
                        cx,
                    ))
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


    /// 面板内容外壳：弹层内衬（rounded_xl + popover 背景 + 边框），观感与其他弹层一致。
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


    /// 审批条（kimi 同款）：审批期间替换输入区。橙色圆点 + 标题，深色内嵌块
    /// 展示命令/diff，底部 本会话内批准(Ctrl+⏎) / 拒绝(Esc) / 批准(⏎)。
    pub(crate) fn render_approval_bar(
        &self,
        approval: &PendingApproval,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = match approval.tool.as_str() {
            "Bash" => "运行命令？".to_string(),
            "Write" => "写入文件？".to_string(),
            "Edit" => "修改文件？".to_string(),
            tool => format!("执行 {tool}？"),
        };
        let detail = if approval.tool == "Bash" {
            format!("$ {}", approval.detail)
        } else {
            approval.detail.clone()
        };
        let cwd = approval.cwd.clone();

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
                    this.decide_approval(decision, window, cx);
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().bg(cx.theme().warning))
                    .child(div().text_sm().font_medium().child(title)),
            )
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
                    .child(format!("工作目录： {cwd}")),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    // 与问卷动作按钮同尺寸（Small），各确认条按钮字号一致
                    .child(
                        Button::new("approval-always")
                            .secondary()
                            .small()
                            .label("本会话内批准  Ctrl+⏎")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::AlwaysAllow, window, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("approval-reject")
                            .secondary()
                            .small()
                            .label("拒绝  Esc")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::Reject, window, cx);
                            })),
                    )
                    .child(
                        Button::new("approval-allow")
                            .primary()
                            .small()
                            .label("批准  ⏎")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::Allow, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }


}
