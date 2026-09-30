use super::*;

impl Composer {
    pub fn set_todos(&mut self, todos: Vec<TodoItem>, cx: &mut Context<Self>) {
        // 清单清空时收起对应弹层，避免 popup 状态悬置
        if todos.is_empty() && matches!(self.popup, Some((Popup::Todos, _))) {
            self.popup = None;
        }
        self.todos = todos;
        cx.notify();
    }

    /// 自测用：(「后台 Bash」chip 可见, 「后台 Agent」chip 可见)——
    /// chip 显隐由任务快照按 agent_id 类型分派
    pub fn debug_task_chips(&self) -> (bool, bool) {
        (
            self.tasks.iter().any(|t| t.agent_id.is_none()),
            self.tasks.iter().any(|t| t.agent_id.is_some()),
        )
    }

    /// 自测用：Agent 任务行的 agent_id 列表
    pub fn debug_agent_task_ids(&self) -> Vec<String> {
        self.tasks
            .iter()
            .filter_map(|t| t.agent_id.clone())
            .collect()
    }

    /// 自测用：模拟点开「后台 Agent」chip 弹层（复现 palette_open 漏 AgentTasks
    /// 导致 render_popup 踩 unreachable 的崩溃路径）；返回弹层是否打开
    pub fn debug_open_agent_tasks_popup(&mut self, cx: &mut Context<Self>) -> bool {
        self.popup = Some((Popup::AgentTasks, 0));
        cx.notify();
        matches!(self.popup, Some((Popup::AgentTasks, _)))
    }

    /// 自测用：收起任意弹层
    pub fn debug_close_popup(&mut self, cx: &mut Context<Self>) {
        self.popup = None;
        cx.notify();
    }

    pub fn set_tasks(&mut self, tasks: Vec<TaskSummary>, cx: &mut Context<Self>) {
        // 对应类别的任务清空时收起对应弹层（chip 按 agent_id 拆分后各自判定）
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

    /// 改动统计 + 文件列表（ReviewPanel 快照）
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

    /// 自测用：返回 (used, total)。
    pub fn debug_context_usage(&self) -> Option<(u64, u64)> {
        self.context_usage.map(|(used, total, _, _)| (used, total))
    }

    /// 底部工具栏芯片：图标 + 文本 + 下拉箭头，样式与 hero 区工作区/分支芯片一致。
    /// `color` 非 None 时图标与文本着色（模式芯片按危险程度着色用），箭头保持 muted。
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
            // 进行中用 Spinner（旋转动画），其余状态静态图标
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
                        .child(format!("当前进度 {done}/{}", self.todos.len())),
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

    /// 后台任务弹层（按 kind 拆成「后台 Bash / 后台 Agent」两个独立面板）：
    /// Bash 行点击展开输出尾部（现状）；Agent 行点击开右侧子代理对话 tab
    ///（收起弹层 + ComposerEvent::OpenSubagent 上冒给 AppView）
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

        // 过滤 tab：进行中 / 已完成 / 全部
        let mut tabs = h_flex().gap_1();
        for (ix, (filter, label)) in TaskFilter::TABS.iter().enumerate() {
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
                    .child(*label),
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
                TaskChipKind::Bash => "无后台 Bash 任务",
                TaskChipKind::Agent => "无后台 Agent 任务",
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
                        // Agent 行：点击开右侧子代理对话 tab（收弹层 + 上冒事件）；
                        // Bash 行：展开/收起输出尾部
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
            // 输出尾部展开仅 Bash 行（Agent 的完整对话在右侧 tab 看）
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
                            "（暂无输出）".to_string()
                        } else {
                            task.output_tail.clone()
                        }),
                );
            }
            list = list.child(row);
        }

        // 固定宽：有/无内容同宽。宽度不许超过 popup_shell 内层容器的 max_w(360)——
        // 超出部分没有 hitbox（可视正常但点击被判 outside 触发弹层关闭）；
        // 命令文本超长走省略（行内 text_ellipsis）
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
