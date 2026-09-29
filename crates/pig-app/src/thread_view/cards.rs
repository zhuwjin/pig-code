use super::*;

impl ThreadView {
    /// 思考折叠块（ZCode reasoning.tsx 同款）：无边框的一行 header（大脑图标 + 文案），
    /// 进行中文案为扫光「正在思考」，后随 `·` + 滚动输出行（累计思考全文的最后一个非空
    /// 行，单行钉尾显示最新内容、左缘渐隐遮罩；纵向滚轮冒泡给外层消息列表）；箭头悬停/
    /// 展开时才显示；展开后正文以左侧竖线缩进展示，超高内部滚动。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_thinking(
        &self,
        message_ix: usize,
        segment_ix: usize,
        text: &str,
        open: bool,
        duration: Option<std::time::Duration>,
        body_scroll: &ScrollHandle,
        ticker_scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let secs = |d: std::time::Duration| (d.as_secs_f64().ceil() as u64).max(1);
        let in_progress = duration.is_none() && self.streaming && !self.replay_turn;
        let label = match duration {
            Some(d) => format!("思考 · 持续了 {} 秒", secs(d)),
            // 进行中只显示「正在思考」（ZCode：秒数只在完成态出现）
            None if self.streaming && !self.replay_turn => "正在思考".to_string(),
            // 回放重建的历史段没有真实时钟
            None => "思考 · 持续了几秒".to_string(),
        };
        // 滚动输出行 = 累计思考全文的最后一个非空行（折叠且进行中才显示）
        let ticker_line = if in_progress && !open {
            text.lines().rev().map(str::trim).find(|l| !l.is_empty())
        } else {
            None
        };
        let muted = cx.theme().muted_foreground;
        let subtlest = muted.opacity(0.6);
        // ZCode：滚动行比标签亮一档（subtle vs subtlest）
        let ticker_color = muted.opacity(0.85);
        let group_id = format!("thinking-row-{message_ix}-{segment_ix}");
        let ticker_key = message_ix * 1024 + segment_ix;
        // 滚动行钉尾：offset 右滚为负，钉尾 = -max（首帧未测量为 0，流式渲染中快速收敛）
        if ticker_line.is_some() {
            let max = ticker_scroll.max_offset();
            ticker_scroll.set_offset(point(-max.x, px(0.)));
        }
        let ticker = ticker_line.map(|line| {
            let bg = cx.theme().background;
            let max = ticker_scroll.max_offset().x;
            let offset = ticker_scroll.offset().x;
            let scrollable = max > px(1.);
            let hides_leading = scrollable && offset < px(-1.);
            let hides_trailing = scrollable && offset > px(1.) - max;
            let fade = |leading: bool| {
                let (from, to) = if leading {
                    (bg, bg.opacity(0.))
                } else {
                    (bg.opacity(0.), bg)
                };
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .when(leading, |this| this.left_0())
                    .when(!leading, |this| this.right_0())
                    .w(px(16.))
                    .bg(linear_gradient(
                        90.,
                        linear_color_stop(from, 0.),
                        linear_color_stop(to, 1.),
                    ))
            };
            div()
                .relative()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .id(("thinking-ticker", ticker_key))
                        .w_full()
                        .overflow_x_scroll()
                        .track_scroll(ticker_scroll)
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(ticker_color)
                                .child(line.to_string()),
                        ),
                )
                // 滚轮接管：横向滚动由本行消费，纵向滚轮冒泡给外层消息列表
                .child(
                    ScrollableMask::new(Axis::Horizontal, ticker_scroll)
                        .id(("thinking-ticker-mask", ticker_key)),
                )
                .when(hides_leading, |this| this.child(fade(true)))
                .when(hides_trailing, |this| this.child(fade(false)))
                .into_any_element()
        });
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("thinking", message_ix * 1024 + segment_ix))
                    .group(group_id.clone())
                    .w_full()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::Thinking { open, pinned, .. }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            *open = !*open;
                            *pinned = true;
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(AssetIconName::Brain)
                            .size_4()
                            .text_color(subtlest),
                    )
                    // 思考进行中：shimmer 扫过高亮；id 必须稳定（默认动画 id 取文案，
                    // 变化会导致扫光重启）
                    .child(if in_progress {
                        ShimmerText::new(label)
                            .id(("thinking-shimmer", message_ix * 1024 + segment_ix))
                            .text_sm()
                            .text_color(subtlest)
                            .into_any_element()
                    } else {
                        div()
                            .text_sm()
                            .text_color(subtlest)
                            .child(label)
                            .into_any_element()
                    })
                    // 进行中的滚动输出行（ZCode reasoning trigger 同款）
                    .when(ticker.is_some(), |this| {
                        this.child(div().text_sm().text_color(subtlest).child("·"))
                    })
                    .children(ticker)
                    // 箭头默认隐藏，行悬停或展开时显示
                    .child(
                        div()
                            .invisible()
                            .group_hover(group_id, |this| this.visible())
                            .when(open, |this| this.visible())
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_4()
                                .text_color(subtlest),
                            ),
                    ),
            )
            .when(open, |this| {
                this.child(
                    // 包装层携带滚动链处理：正文能滚时吞掉滚轮，避免外层消息列表联动
                    div()
                        .relative()
                        .on_scroll_wheel(consume_scroll(body_scroll))
                        .child(
                            div()
                                .id(("thinking-body", message_ix * 1024 + segment_ix))
                                .mt_1()
                                .ml(px(8.))
                                .border_l_1()
                                .border_color(cx.theme().border)
                                .pl(px(14.))
                                .max_h(px(240.))
                                .overflow_y_scroll()
                                .track_scroll(body_scroll)
                                .text_sm()
                                .text_color(subtlest)
                                .child(text.to_string()),
                        ),
                )
            })
            .into_any_element()
    }

    /// 工具调用（ZCode 同款）：无边框摘要行（图标 + 中文工具名 + 单行摘要 + 状态词），
    /// 箭头仅悬停/展开时显示；展开后是圆角描边卡片：完整输入（终端类带 `$` 前缀）+
    /// 等宽输出，输出限高内部滚动。运行中不用 spinner，工具名扫光（ZCode 的取舍：
    /// 流式期间工具多，持续动画耗渲染资源）。
    /// `approval_pending`：该工具正在等待批准（行尾显示黄色「等待批准」）。
    /// `agent_cards`：SubagentCard 写入的代理卡列表（Agent 一张、AgentSwarm 多张）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_tool_card(
        &self,
        message_ix: usize,
        segment_ix: usize,
        tool: &str,
        summary: &str,
        live_note: Option<&str>,
        output: &str,
        is_error: bool,
        done: bool,
        expanded: bool,
        approval_pending: bool,
        edit: Option<&EditDiff>,
        agent_cards: &[AgentCardMeta],
        body_scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 代理卡（A3c，kimi-code 同款气质）：带 SubagentCard 元信息的 Agent/
        // AgentSwarm 工具卡升级为描述卡（bot 图标 + 任务标题 + profile · model），
        // 点击开右侧子代理对话 tab；卡体不再提供展开区（完整结果与过程见右侧
        // 「子代理」tab）。多张（swarm 每个子代理一张）纵向叠放，逐卡独立运行态。
        // 无卡（live 中 SubagentCard 事件到达前的瞬时态）回落下方标准工具卡渲染。
        if !agent_cards.is_empty() {
            return v_flex()
                .w_full()
                .gap_1()
                .children(agent_cards.iter().enumerate().map(|(card_ix, card)| {
                    self.render_agent_card(
                        message_ix,
                        segment_ix,
                        card_ix,
                        card,
                        done,
                        is_error,
                        live_note,
                        approval_pending,
                        cx,
                    )
                }))
                .into_any_element();
        }
        // ZCode 三级文字层级：正文 > subtle(60%) > subtlest(30~40%)，靠层级而非边框/色彩造信息密度
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let group_id = format!("tool-row-{message_ix}-{segment_ix}");
        let running = !done && !approval_pending;
        let tool_icon = match tool {
            "Bash" => AssetIconName::Terminal,
            "Read" => AssetIconName::Eye,
            "Write" => AssetIconName::FilePlus,
            "Edit" => AssetIconName::FilePen,
            "Glob" => AssetIconName::FolderSearch,
            "Grep" => AssetIconName::TextSearch,
            "TodoList" => AssetIconName::ListTodo,
            "FetchURL" => AssetIconName::Globe,
            "AskUserQuestion" => AssetIconName::MessageCircleQuestionMark,
            _ => AssetIconName::Wrench,
        };
        let kind_label = match tool {
            "Bash" => "终端",
            "Read" => "读取",
            "Write" => "写入",
            "Edit" => "编辑",
            "Glob" => "查找文件",
            "Grep" => "搜索",
            "TodoList" => "待办",
            "FetchURL" => "抓取网页",
            "TaskList" | "TaskOutput" | "TaskStop" => "后台任务",
            "AskUserQuestion" => "提问",
            _ => tool,
        };
        // 摘要压成单行：多行命令的换行折叠为空格（否则折叠行会被撑成多行）
        let summary_line = summary.split_whitespace().collect::<Vec<_>>().join(" ");

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("tool", message_ix * 1024 + segment_ix))
                    .group(group_id.clone())
                    .w_full()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::ToolCall { expanded, .. }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            *expanded = !*expanded;
                        }
                        cx.notify();
                    }))
                    .child(Icon::new(tool_icon).size_4().text_color(subtlest))
                    // 工具运行中：工具名 shimmer 扫光（等批准/已结束回静态文本）
                    .child(if running {
                        ShimmerText::new(kind_label)
                            .id(("tool-label-shimmer", message_ix * 1024 + segment_ix))
                            .text_sm()
                            .text_color(subtlest)
                            .into_any_element()
                    } else {
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(subtlest)
                            .child(kind_label.to_string())
                            .into_any_element()
                    })
                    // 成功不给标记；失败在行尾放叉号（悬停显示原因）
                    // 编辑类：文件名（亮一档）+ 目录路径（最暗，优先截断）；其余工具单行摘要
                    // 摘要只占内容宽（过长时收缩截断），让统计/箭头跟在文字后面而非靠右
                    .child(if edit.is_some() {
                        let (dir, name) = split_path(summary);
                        h_flex()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .gap_1()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .text_color(subtle)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_sm()
                                    .text_color(subtlest)
                                    .child(dir),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .text_color(subtle)
                            .child(summary_line)
                            .into_any_element()
                    })
                    // 增删计数（等宽，为零的一侧不显示）
                    .when_some(edit, |this, edit| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .flex_shrink_0()
                                .text_sm()
                                .font_family(cx.theme().mono_font_family.clone())
                                .when(edit.additions > 0, |this| {
                                    this.child(
                                        div()
                                            .text_color(cx.theme().success)
                                            .child(format!("+{}", edit.additions)),
                                    )
                                })
                                .when(edit.deletions > 0, |this| {
                                    this.child(
                                        div()
                                            .text_color(cx.theme().danger)
                                            .child(format!("-{}", edit.deletions)),
                                    )
                                }),
                        )
                    })
                    .when(approval_pending, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child("等待批准"),
                        )
                    })
                    // 箭头默认隐藏，行悬停或展开时显示（保持行内干净）
                    .child(
                        div()
                            .invisible()
                            .group_hover(group_id, |this| this.visible())
                            .when(expanded, |this| this.visible())
                            .child(
                                Icon::new(if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_4()
                                .text_color(subtlest),
                            ),
                    )
                    // 失败：行尾叉号常显，悬停展示失败原因（输出压单行并截断）
                    .when(done && is_error, |this| {
                        let collapsed = output.split_whitespace().collect::<Vec<_>>().join(" ");
                        const MAX_REASON_CHARS: usize = 200;
                        let reason = if collapsed.chars().count() > MAX_REASON_CHARS {
                            let head: String =
                                collapsed.chars().take(MAX_REASON_CHARS - 3).collect();
                            format!("{}...", head.trim_end())
                        } else {
                            collapsed
                        };
                        this.child(
                            div()
                                .id(("tool-err", message_ix * 1024 + segment_ix))
                                .flex_shrink_0()
                                .tooltip(move |window, cx| {
                                    Tooltip::new(reason.clone()).build(window, cx)
                                })
                                .child(
                                    Icon::new(IconName::Close)
                                        .size_3()
                                        .text_color(cx.theme().danger),
                                ),
                        )
                    }),
            )
            // 前台子代理运行中的实时进度行（摘要行下方）：旋转小图标 + 单行省略，
            // 左缩进对齐摘要行的图标列（图标 16px + gap 8px）；收尾/回放无此行
            .when(
                !done && live_note.is_some_and(|note| !note.is_empty()),
                |this| {
                    let note = live_note
                        .unwrap_or_default()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    this.child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .pl_6()
                            .child(Spinner::new().small().color(subtlest))
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_xs()
                                    .text_color(subtle)
                                    .child(note),
                            ),
                    )
                },
            )
            .when(expanded, |this| {
                // 展开正文统一放进带滚动条的视口（track_scroll 持久滚动位置 + 可见滚动条）
                this.child(
                    div()
                        .relative()
                        .mt_2()
                        .w_full()
                        // 滚动链：正文能滚时吞掉滚轮，避免外层消息列表联动
                        .on_scroll_wheel(consume_scroll(body_scroll))
                        // 编辑类工具展开为内联 diff 代码卡；其余工具是通用输入+输出卡
                        .child(if let Some(edit) = edit {
                            Self::render_edit_diff(
                                ("tool-body", message_ix * 1024 + segment_ix),
                                edit,
                                body_scroll,
                                cx,
                            )
                        } else {
                            v_flex()
                                .w_full()
                                .gap_3()
                                .rounded_xl()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().group_box)
                                .px_4()
                                .py_3()
                                // 完整输入：终端类带 `$` 前缀，其余工具直接全文（折叠行里被截断的部分）
                                .when(!summary.is_empty(), |this| {
                                    this.child(
                                        h_flex()
                                            .w_full()
                                            .gap_2()
                                            .items_start()
                                            .when(tool == "Bash", |this| {
                                                this.child(
                                                    div().text_sm().text_color(subtle).child("$"),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_sm()
                                                    .text_color(cx.theme().foreground)
                                                    .child(summary.to_string()),
                                            ),
                                    )
                                })
                                .child(
                                    div()
                                        .id(("tool-body", message_ix * 1024 + segment_ix))
                                        .max_h(px(120.))
                                        .overflow_y_scroll()
                                        .track_scroll(body_scroll)
                                        .text_sm()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(if is_error {
                                            cx.theme().danger
                                        } else {
                                            subtle
                                        })
                                        .child(if output.is_empty() && done {
                                            "没有输出。".to_string()
                                        } else {
                                            output.to_string()
                                        }),
                                )
                                .into_any_element()
                        })
                        // diff 卡的滚动条已内置（随圆角补丁收角）；通用卡的补在这里
                        .when(edit.is_none(), |this| {
                            this.child(Scrollbar::vertical(body_scroll))
                        }),
                )
            })
            .into_any_element()
    }

    /// 代理卡：子代理 Agent/AgentSwarm 工具卡的升级样式（A3c，kimi-code 同款气质）——
    /// 圆角卡 + bot 图标方块 + 任务描述标题 + `{profile} · {model}` 副标题；
    /// 前台运行中多一行实时进度（段级 live_note；后台卡用卡级 live_note）；
    /// 右侧状态：等待批准/Spinner/成功勾/失败词。
    /// 点击卡体开右侧子代理对话 tab（完整结果与过程在那里看，故不提供展开区）。
    /// `card_ix`：同一工具卡里的第几张（swarm 多卡叠放时区分元素 id）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_agent_card(
        &self,
        message_ix: usize,
        segment_ix: usize,
        card_ix: usize,
        card: &AgentCardMeta,
        done: bool,
        is_error: bool,
        live_note: Option<&str>,
        approval_pending: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        // 运行态真值表：前台卡跟工具调用同生命周期（!done；等审批暂停转圈）；
        // 后台卡的工具调用立即收尾（running 回执），真实运行态由子代理生命周期
        // 驱动（SubagentActivity finished 置卡级 finished；回放由 core 补发）
        let running = if card.background {
            !card.finished
        } else {
            !done && !approval_pending
        };
        // 后台卡的实时进度在卡级 live_note（SubagentActivity 按 agent_id 写入）；
        // 前台卡走段级 live_note（SubagentProgress 按 item_id 写入）
        let progress_note = if card.background {
            card.live_note.as_deref()
        } else {
            live_note
        };
        let failed = done && is_error;
        let title = if card.description.is_empty() {
            "子代理".to_string()
        } else {
            card.description.clone()
        };
        let subtitle = if failed {
            "失败".to_string()
        } else if card.model.is_empty() {
            card.profile.clone()
        } else {
            format!("{} · {}", card.profile, card.model)
        };
        let agent_id = card.agent_id.clone();
        let title_click = title.clone();
        h_flex()
            .id((
                "agent-card",
                (message_ix * 1024 + segment_ix) * 256 + card_ix,
            ))
            .w_full()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().group_box)
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(ThreadEvent::OpenSubagent {
                    agent_id: agent_id.clone(),
                    title: title_click.clone(),
                });
            }))
            // bot 图标方块（圆角 info 淡底）
            .child(
                div()
                    .flex_shrink_0()
                    .w_8()
                    .h_8()
                    .rounded_md()
                    .bg(cx.theme().info.opacity(0.12))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::Bot)
                            .size_4()
                            .text_color(cx.theme().info),
                    ),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_0p5()
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(if failed { cx.theme().danger } else { subtle })
                            .child(subtitle),
                    )
                    // 运行中的实时进度行（挪进卡里， spinner + 单行省略）
                    .when(
                        running && progress_note.is_some_and(|note| !note.is_empty()),
                        |this| {
                            let note = progress_note
                                .unwrap_or_default()
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ");
                            this.child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .child(Spinner::new().small().color(subtlest))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_xs()
                                            .text_color(subtle)
                                            .child(note),
                                    ),
                            )
                        },
                    ),
            )
            // 右侧状态：等待批准（仅前台）/ 运行中 Spinner / 失败词 / 成功勾。
            // 注意：SubagentActivity 的 finished 不带成败——后台卡终态的成败沿用
            // 工具回执的 is_error（子代理失败由通知气泡呈现，卡上勾仅代表「跑完」）
            .child(if !card.background && approval_pending {
                div()
                    .text_xs()
                    .text_color(cx.theme().warning)
                    .child("等待批准")
                    .into_any_element()
            } else if running {
                Spinner::new().small().color(subtlest).into_any_element()
            } else if failed {
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child("失败")
                    .into_any_element()
            } else {
                Icon::new(IconName::CircleCheck)
                    .size_4()
                    .text_color(cx.theme().success)
                    .into_any_element()
            })
            .child(
                Icon::new(IconName::ChevronRight)
                    .size_4()
                    .text_color(subtlest),
            )
            .into_any_element()
    }

    /// 给圆角卡片补四角：填充每个角落的"R×R 方形 − 半径 R 的四分之一圆"区域
    /// （圆角缺口）。gpui 的 ContentMask 只有矩形裁剪，行底色/色条/滚动条
    /// 都会越过圆角描边；用卡片**背后**的颜色补上缺口后，内容在视觉上
    /// 即被圆角收住，不出框。曲线用二次贝塞尔逼近四分之一圆（控制点取外角，
    /// 偏差 <0.5px）。调用方需在此之后再描一次圆角边框（补丁盖住了角上的描边）。
    pub(crate) fn paint_rounded_corner_patches(
        bounds: Bounds<Pixels>,
        radius: Pixels,
        color: Hsla,
        window: &mut Window,
    ) {
        let r = radius;
        let w = bounds.size.width;
        let h = bounds.size.height;
        let mut path = PathBuilder::fill();
        // 左上
        path.move_to(point(px(0.), px(0.)));
        path.line_to(point(r, px(0.)));
        path.curve_to(point(px(0.), r), point(px(0.), px(0.)));
        path.close();
        // 右上
        path.move_to(point(w, px(0.)));
        path.line_to(point(w, r));
        path.curve_to(point(w - r, px(0.)), point(w, px(0.)));
        path.close();
        // 左下
        path.move_to(point(px(0.), h));
        path.line_to(point(px(0.), h - r));
        path.curve_to(point(r, h), point(px(0.), h));
        path.close();
        // 右下
        path.move_to(point(w, h));
        path.line_to(point(w, h - r));
        path.curve_to(point(w - r, h), point(w, h));
        path.close();
        path.translate(bounds.origin);
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }
}
