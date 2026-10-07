use super::*;

impl Composer {
    pub fn set_todos(&mut self, todos: Vec<TodoItem>, cx: &mut Context<Self>) {
        // Collapse the popup when the list empties, avoiding a dangling popup state
        if todos.is_empty() && matches!(self.popup, Some((Popup::Todos, _))) {
            self.popup = None;
        }
        self.todos = todos;
        cx.notify();
    }

    /// For self-test: ("background Bash" chip visible, "background Agent" chip
    /// visible); chip visibility is dispatched by agent_id type from the task snapshot
    pub fn debug_task_chips(&self) -> (bool, bool) {
        (
            self.tasks.iter().any(|t| t.agent_id.is_none()),
            self.tasks.iter().any(|t| t.agent_id.is_some()),
        )
    }

    /// For self-test: the agent_id list of Agent task rows
    pub fn debug_agent_task_ids(&self) -> Vec<String> {
        self.tasks
            .iter()
            .filter_map(|t| t.agent_id.clone())
            .collect()
    }

    /// For self-test: simulate opening the "background Agent" chip popup (reproduces
    /// the crash path where palette_open misses AgentTasks and render_popup hits
    /// unreachable); returns whether the popup is open
    pub fn debug_open_agent_tasks_popup(&mut self, cx: &mut Context<Self>) -> bool {
        self.popup = Some((Popup::AgentTasks, 0));
        cx.notify();
        matches!(self.popup, Some((Popup::AgentTasks, _)))
    }

    /// For self-test: collapse any popup
    pub fn debug_close_popup(&mut self, cx: &mut Context<Self>) {
        self.popup = None;
        cx.notify();
    }

    pub fn set_tasks(&mut self, tasks: Vec<TaskSummary>, cx: &mut Context<Self>) {
        // Collapse the matching popup when that category's tasks empty (chips are
        // split by agent_id and judged separately)
        if !tasks.iter().any(|t| t.agent_id.is_none())
            && matches!(self.popup, Some((Popup::Tasks, _)))
        {
            self.popup = None;
        }
        if !tasks.iter().any(|t| t.agent_id.is_some())
            && matches!(self.popup, Some((Popup::AgentTasks, _)))
        {
            self.popup = None;
        }
        self.tasks = tasks;
        cx.notify();
    }

    /// Change stats + file list (ReviewPanel snapshot)
    pub fn set_changes(
        &mut self,
        added: u32,
        removed: u32,
        files: Vec<(String, u32, u32)>,
        cx: &mut Context<Self>,
    ) {
        self.changes = (added, removed);
        self.change_files = files;
        cx.notify();
    }

    /// For self-test: returns (used, total).
    pub fn debug_context_usage(&self) -> Option<(u64, u64)> {
        self.context_usage.map(|(used, total, _, _)| (used, total))
    }

    /// Bottom toolbar chip: icon + text + dropdown arrow, styled the same as the
    /// hero area's workspace/branch chips.
    /// When `color` is not None, the icon and text are tinted (used by mode chips
    /// colored by danger level); the arrow stays muted.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_bar_chip(
        &self,
        id: &'static str,
        icon: Option<AssetIconName>,
        label: String,
        filled: bool,
        color: Option<Hsla>,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        h_flex()
            .id(id)
            .gap_1()
            .px_3()
            .py_1()
            .rounded_full()
            .cursor_pointer()
            .when(filled, |this| this.bg(cx.theme().accent.opacity(0.5)))
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(on_click)
            .when_some(icon, |this, icon| {
                this.child(
                    Icon::new(icon)
                        .size_4()
                        .text_color(color.unwrap_or(cx.theme().muted_foreground)),
                )
            })
            .child(
                div()
                    .text_sm()
                    .when_some(color, |this, color| this.text_color(color))
                    .child(label),
            )
            .child(
                Icon::new(IconName::ChevronDown)
                    .size_3()
                    .text_color(cx.theme().muted_foreground),
            )
    }

    pub(crate) fn render_todos_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let done = self
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Done)
            .count();
        let mut list = v_flex()
            .id("aux-todos-list")
            .w_full()
            .gap_1()
            .max_h(px(280.))
            .overflow_y_scroll();
        for item in &self.todos {
            // In-progress uses a Spinner (rotating animation); other states use
            // static icons
            let icon = match item.status {
                TodoStatus::Done => Icon::new(AssetIconName::CircleCheck)
                    .size_4()
                    .text_color(cx.theme().success)
                    .into_any_element(),
                TodoStatus::InProgress => Spinner::new()
                    .icon(AssetIconName::LoaderCircle)
                    .color(cx.theme().progress_bar)
                    .into_any_element(),
                TodoStatus::Pending => Icon::new(AssetIconName::Circle)
                    .size_4()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
            };
            list = list.child(
                h_flex().w_full().gap_2().child(icon).child(
                    div()
                        .text_sm()
                        .when(item.status == TodoStatus::Done, |this| {
                            this.text_color(cx.theme().muted_foreground)
                        })
                        .child(item.content.clone()),
                ),
            );
        }
        let content = self.aux_panel_shell(
            v_flex()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            rust_i18n::t!(
                                "composer.todo_progress",
                                done = done,
                                total = self.todos.len()
                            )
                            .to_string(),
                        ),
                )
                .child(list),
            cx,
        );
        self.popup_shell(
            "composer-todos-popup",
            content.into_any_element(),
            PopupAnchor::Left,
            None,
            cx,
        )
    }

    /// Background task popup (split by kind into two independent panels: "background
    /// Bash / background Agent"): clicking a Bash row expands the output tail (as
    /// today); clicking an Agent row opens the subagent conversation tab on the
    /// right (collapse the popup plus ComposerEvent::OpenSubagent bubbles up to
    /// AppView)
    pub(crate) fn render_tasks_panel(
        &self,
        kind: TaskChipKind,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let running = self
            .tasks
            .iter()
            .filter(|t| kind.matches(t) && matches!(t.status, TaskStatus::Running))
            .count();
        let title = kind.label(running);

        // Filter tabs: running / completed / all
        let mut tabs = h_flex().gap_1();
        for (ix, filter) in TaskFilter::TABS.iter().enumerate() {
            let active = self.task_filter == *filter;
            let filter = *filter;
            tabs = tabs.child(
                div()
                    .id(("aux-task-filter", ix))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_xs()
                    .when(active, |this| this.bg(cx.theme().accent))
                    .when(!active, |this| this.text_color(cx.theme().muted_foreground))
                    .hover(|this| this.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.task_filter = filter;
                        cx.notify();
                    }))
                    .child(filter.label()),
            );
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let visible: Vec<&TaskSummary> = self
            .tasks
            .iter()
            .filter(|t| kind.matches(t) && self.task_filter.matches(t.status))
            .collect();
        let list_id = match kind {
            TaskChipKind::Bash => "aux-tasks-list",
            TaskChipKind::Agent => "aux-agent-tasks-list",
        };
        let mut list = v_flex()
            .id(list_id)
            .w_full()
            .gap_1()
            .max_h(px(280.))
            .overflow_y_scroll();
        if visible.is_empty() {
            let empty = match kind {
                TaskChipKind::Bash => rust_i18n::t!("composer.no_bash_tasks"),
                TaskChipKind::Agent => rust_i18n::t!("composer.no_agent_tasks"),
            };
            list = list.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(empty),
            );
        }
        for (ix, task) in visible.iter().enumerate() {
            let icon = match task.status {
                TaskStatus::Running => Spinner::new()
                    .icon(AssetIconName::LoaderCircle)
                    .color(cx.theme().progress_bar)
                    .into_any_element(),
                TaskStatus::Exited(0) => Icon::new(AssetIconName::CircleCheck)
                    .size_4()
                    .text_color(cx.theme().success)
                    .into_any_element(),
                TaskStatus::Exited(_) => Icon::new(AssetIconName::TriangleAlert)
                    .size_4()
                    .text_color(cx.theme().warning)
                    .into_any_element(),
                TaskStatus::Killed => Icon::new(AssetIconName::TriangleAlert)
                    .size_4()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
            };
            let duration = format_task_duration(task.started_at, task.ended_at.unwrap_or(now));
            let expanded = self.expanded_task.as_deref() == Some(task.id.as_str());
            let task_id = task.id.clone();
            let agent_open = task
                .agent_id
                .clone()
                .map(|agent_id| (agent_id, task.command.clone()));
            let mut row = v_flex().w_full().child(
                h_flex()
                    .id(("aux-task-row", ix))
                    .w_full()
                    .gap_2()
                    .px_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.5)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        // Agent row: clicking opens the subagent conversation tab on
                        // the right (collapse the popup plus bubble the event);
                        // Bash row: expand/collapse the output tail
                        if let Some((agent_id, title)) = &agent_open {
                            this.popup = None;
                            cx.emit(ComposerEvent::OpenSubagent {
                                agent_id: agent_id.clone(),
                                title: title.clone(),
                            });
                        } else {
                            this.expanded_task =
                                if this.expanded_task.as_deref() == Some(task_id.as_str()) {
                                    None
                                } else {
                                    Some(task_id.clone())
                                };
                        }
                        cx.notify();
                    }))
                    .child(icon)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_x_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(task.command.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(duration),
                    )
                    .child(
                        Icon::new(AssetIconName::ChevronRight)
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    ),
            );
            // Output tail expansion is for Bash rows only (an Agent's full
            // conversation lives in the right tab)
            if kind == TaskChipKind::Bash && expanded {
                row = row.child(
                    div()
                        .id(("aux-task-output", ix))
                        .w_full()
                        .mt_1()
                        .p_2()
                        .rounded_md()
                        .bg(cx.theme().accent.opacity(0.3))
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().muted_foreground)
                        .child(if task.output_tail.is_empty() {
                            rust_i18n::t!("composer.no_output_yet").to_string()
                        } else {
                            task.output_tail.clone()
                        }),
                );
            }
            list = list.child(row);
        }

        // Fixed width: same width with or without content. The width must not exceed
        // popup_shell's inner container max_w(360): the overhang has no hitbox
        // (renders fine, but clicks are judged outside and close the popup);
        // overlong command text is ellipsized (text_ellipsis on the row)
        let content = self
            .aux_panel_shell(
                v_flex()
                    .child(
                        h_flex()
                            .w_full()
                            .child(div().text_sm().child(title))
                            .child(div().flex_1())
                            .child(tabs),
                    )
                    .child(list),
                cx,
            )
            .w(px(360.));
        let popup_id = match kind {
            TaskChipKind::Bash => "composer-tasks-popup",
            TaskChipKind::Agent => "composer-agent-tasks-popup",
        };
        self.popup_shell(
            popup_id,
            content.into_any_element(),
            PopupAnchor::Left,
            None,
            cx,
        )
    }
}
