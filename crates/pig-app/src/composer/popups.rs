use super::*;

fn tag_for(kind: Popup) -> &'static str {
    match kind {
        Popup::Mention => "mention",
        Popup::Slash => "slash",
        Popup::ExecMode => "exec",
        Popup::Model => "model",
        Popup::Reasoning => "reasoning",
        Popup::Cwd => "cwd",
        Popup::Branch => "branch",
        Popup::Context => "context",
        Popup::Todos => "todos",
        Popup::Tasks => "tasks",
        Popup::AgentTasks => "agent-tasks",
    }
}

impl Composer {
    /// 上下文容量面板：标题 + 用量/占比 + 进度条 + 平均缓存命中率，
    /// 居中锚定在指示器芯片正上方（悬停展示）。
    pub(crate) fn render_context_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let (used, total, cache_read_total, input_total) =
            self.context_usage.unwrap_or((0, 1, 0, 0));
        let ratio = (used as f32 / total as f32).clamp(0.0, 1.0);
        let bar_color = if ratio > 0.8 {
            cx.theme().warning
        } else {
            cx.theme().progress_bar
        };
        let cache_total = cache_read_total + input_total;

        let content = v_flex()
            .w_full()
            .gap_2()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .p_3()
            .child(
                h_flex()
                    .w_full()
                    .child(div().text_sm().font_medium().child("上下文容量"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{} / {} ({:.1}%)",
                                Self::format_tokens_compact(used),
                                Self::format_tokens_compact(total),
                                ratio * 100.0
                            )),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(px(6.))
                    .rounded_full()
                    .bg(cx.theme().muted_foreground.opacity(0.15))
                    .when(used > 0, |this| {
                        this.child(
                            div()
                                .h_full()
                                .w(relative(ratio))
                                // 极小占比也保留可见的一截（0.1% 仅 0.3px，会被圆整掉）
                                .min_w(px(3.))
                                .rounded_full()
                                .bg(bar_color),
                        )
                    }),
            )
            .when(cache_total > 0, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "平均缓存命中率 {:.1}%（命中 {} / 输入 {}）",
                            cache_read_total as f64 / cache_total as f64 * 100.0,
                            Self::format_tokens_compact(cache_read_total),
                            Self::format_tokens_compact(cache_total),
                        )),
                )
            })
            .into_any_element();
        self.popup_shell(
            "composer-context-popup",
            content,
            PopupAnchor::Center,
            None,
            cx,
        )
    }

    pub(crate) fn popup_query(&self, cx: &App) -> Option<(Popup, usize, String)> {
        let (kind, start) = self.popup?;
        match kind {
            Popup::Mention | Popup::Slash => {
                let value = self.input.read(cx).value();
                let caret = self.input.read(cx).selected_range().start.min(value.len());
                if caret < start + 1 {
                    return None;
                }
                Some((kind, start, value[start + 1..caret].to_string()))
            }
            _ => Some((kind, start, String::new())),
        }
    }

    pub(crate) fn insert_file(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((Popup::Mention, start)) = self.popup else {
            return;
        };
        let caret = self.input.read(cx).selected_range().start;
        // 文档文本是完整 @path（发送时据此收集文件），展示文本只显示文件名
        let file_name = path.rsplit('/').next().unwrap_or(path.as_str());
        let token =
            InlineToken::new(path.clone(), format!("@{path}")).with_label(format!("@{file_name}"));
        self.input.update(cx, |input, cx| {
            if input
                .replace_range_with_token(start..caret, token, window, cx)
                .is_ok()
            {
                // token API 不自动加分隔符：插入后选区已塌缩在 token 尾，补一个尾随空格
                input.replace(" ", window, cx);
            } else {
                // token 校验失败等场景回落为纯文本插入
                input.set_selected_range(start..caret, cx);
                input.replace(format!("@{path} "), window, cx);
            }
            input.focus(window, cx);
        });
        self.popup = None;
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_list_item(
        &self,
        id: impl Into<ElementId>,
        icon: IconName,
        label: String,
        detail: Option<String>,
        selected: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .id(id)
            .w_full()
            .gap_2()
            .px_3()
            .py_1()
            .cursor_pointer()
            .rounded(cx.theme().radius)
            .when(selected, |this| this.bg(cx.theme().accent))
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(on_click)
            .child(
                Icon::new(icon)
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().text_sm().child(label))
            .when_some(detail, |this, detail| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail),
                )
            })
            .into_any_element()
    }

    pub(crate) fn render_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (kind, _start, query) = self.popup_query(cx)?;
        let items: Vec<AnyElement> = match kind {
            Popup::Mention => {
                let _ = query;
                self.mention_results
                    .iter()
                    .enumerate()
                    .map(|(ix, path)| {
                        let path = path.clone();
                        self.render_list_item(
                            ("mention", ix),
                            IconName::FileText,
                            path.clone(),
                            None,
                            ix == self.popup_sel,
                            cx.listener(move |this, _, window, cx| {
                                this.insert_file(path.clone(), window, cx);
                            }),
                            cx,
                        )
                    })
                    .collect()
            }
            Popup::Slash => Self::slash_filtered(&query)
                .enumerate()
                .map(|(ix, (name, desc))| {
                    self.render_list_item(
                        ("slash", ix),
                        IconName::SquareTerminal,
                        name.to_string(),
                        Some(desc.to_string()),
                        ix == self.popup_sel,
                        cx.listener(move |this, _, window, cx| {
                            this.stage_command(name, window, cx);
                        }),
                        cx,
                    )
                })
                .collect(),
            Popup::ExecMode
            | Popup::Cwd
            | Popup::Branch
            | Popup::Model
            | Popup::Reasoning
            | Popup::Context
            | Popup::Todos
            | Popup::Tasks
            | Popup::AgentTasks => {
                unreachable!(
                    "Cwd/Branch/ExecMode/Model/Reasoning/Context/Todos/Tasks/AgentTasks 由各自的专用面板渲染"
                )
            }
        };
        if items.is_empty() {
            return None;
        }

        Some(
            div()
                .id("composer-popup")
                .test_support()
                .absolute()
                .bottom_full()
                .left_0()
                .right_0()
                .mb_1()
                .max_h(px(240.))
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().popover)
                .py_1()
                .child(
                    div()
                        .relative()
                        .with_animation(
                            format!("popup-enter-{}", tag_for(kind)),
                            Animation::new(std::time::Duration::from_millis(150))
                                .with_easing(ease_out_quint()),
                            |el, delta| el.top(px(6.0 * (1.0 - delta))).opacity(delta),
                        )
                        .child(
                            // 行是滚动容器的直接子元素：选中项才能随键盘导航
                            // scroll_to_item 滚进视野（与消息列表同机制）。
                            // overflow_y_scroll/track_scroll 是 StatefulInteractiveElement
                            // 方法，滚动容器必须有 id；px_1 让选中高亮不贴弹层边框
                            div()
                                .id("composer-popup-scroll")
                                .max_h(px(228.))
                                .overflow_y_scroll()
                                .track_scroll(&self.popup_scroll)
                                .px_1()
                                .children(items),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 弹层外壳：锚定在触发芯片正上方，点击外部关闭，带进入动画。
    /// `anchor` 为 Center 时弹层水平中线对齐芯片中线（定宽 360）；
    /// 其余弹层宽度按内容伸缩（160 ~ 360）。
    /// `hover_clear`：Command 面板是「悬停即选中」，鼠标移出面板后选中行的高亮
    /// 会残留，传对应 CommandState 时在移出面板时清掉选中。
    pub(crate) fn popup_shell(
        &self,
        id: &'static str,
        content: AnyElement,
        anchor: PopupAnchor,
        hover_clear: Option<Entity<CommandState>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .absolute()
            .bottom_full()
            .mb_2()
            .map(|this| match anchor {
                PopupAnchor::Left => this.left_0(),
                PopupAnchor::Right => this.right_0(),
                // 外层拉伸到芯片宽度，再由 flex 把固定宽的面板居中到芯片中线
                PopupAnchor::Center => this.left_0().right_0().flex().flex_row().justify_center(),
            })
            // 滚轮事件不穿透到弹层背后的会话消息流；内容自身的滚动（Command 虚拟列表
            // 等更深的滚动区）先消费事件，不受影响
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                if let Some((kind, _)) = this.popup {
                    this.outside_closed = Some((kind, event.position));
                }
                this.popup = None;
                cx.notify();
            }))
            .when_some(hover_clear, |this, command| {
                this.on_hover(cx.listener(move |_, hovered: &bool, window, cx| {
                    if !*hovered {
                        command.update(cx, |state, cx| {
                            state.set_selected_index(None, window, cx);
                        });
                    }
                }))
            })
            .child(
                div()
                    .relative()
                    .map(|this| match anchor {
                        // 上下文容量面板内容固定（标题行 + 进度条），定宽居中
                        PopupAnchor::Center => this.w(px(360.)),
                        // 其余面板按内容伸缩：思考等级这类短列表不用撑满 360
                        _ => this.min_w(px(160.)).max_w(px(360.)),
                    })
                    .with_animation(
                        format!("{id}-enter"),
                        Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(ease_out_quint()),
                        |el, delta| el.top(px(6.0 * (1.0 - delta))).opacity(delta),
                    )
                    .child(content),
            )
            .into_any_element()
    }

    /// Command 弹层外壳：锚定在触发芯片正上方，点击外部关闭，带进入动画。
    pub(crate) fn command_popup_shell(
        &self,
        id: &'static str,
        state: &Entity<CommandState>,
        command: Command,
        anchor: PopupAnchor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.popup_shell(
            id,
            command.into_any_element(),
            anchor,
            Some(state.clone()),
            cx,
        )
    }

    /// 面板确认/取消的通用收尾：关闭弹层并回焦输入框。
    pub(crate) fn close_command_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// 芯片点击开合弹层：弹层打开时点击芯片会先触发弹层的 on_mouse_down_out 把它
    /// 关掉（中间隔着一次重渲染，渲染时捕获的开合状态不可靠），这里按「同一次按压
    /// 的按下位置」吞掉紧随其后的 click，避免收起又马上弹开。
    pub(crate) fn toggle_popup(
        &mut self,
        kind: Popup,
        click: &ClickEvent,
        command: Option<Entity<CommandState>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let down_pos = match click {
            ClickEvent::Mouse(event) => Some(event.down.position),
            _ => None,
        };
        if let Some((closed, pos)) = self.outside_closed.take()
            && closed == kind
            && Some(pos) == down_pos
        {
            return;
        }
        if matches!(self.popup, Some((k, _)) if k == kind) {
            self.close_command_popup(window, cx);
        } else {
            self.popup = Some((kind, 0));
            if let Some(command) = command {
                command.update(cx, |state, cx| {
                    state.set_query("", window, cx);
                    state.focus(window, cx);
                });
            }
            cx.notify();
        }
    }
}
