use super::*;

impl ThreadView {
    /// 思考折叠块（ZCode reasoning.tsx 同款）：无边框的一行 header（大脑图标 + 文案），
    /// 进行中文案为扫光「正在思考」，后随 `·` + 滚动输出行（纵滚状态机提供：换行时旧行
    /// 向上滚出、新行从下方滚入，钉尾显示最新内容、左缘渐隐遮罩；纵向滚轮冒泡给外层
    /// 消息列表）；箭头悬停/展开时才显示；展开后正文以左侧竖线缩进展示，超高内部滚动。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_thinking(
        &self,
        message_ix: usize,
        segment_ix: usize,
        text: &str,
        open: bool,
        duration: Option<std::time::Duration>,
        ticker: &TickerRoll,
        body_scroll: &ScrollHandle,
        ticker_scroll: &ScrollHandle,
        expand_anim: &ExpandAnim,
        window: &Window,
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
        // 滚动输出行由段内纵滚状态机提供（TickerRoll，见 model.rs），折叠且进行中才显示
        let ticker_line = if in_progress && !open {
            ticker.displayed.clone()
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
        let ticker = ticker_line.map(|(line_ix, line)| {
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
            // 纵滚容器（ZCode QueuedSummaryContent 同款）：单行高、纵向裁切；退场行
            // absolute 不参与布局（popLayout 同款）。构造抽成自由函数供布局回归测试复用，
            // 语义见 ticker_roll_content
            let roll = ticker_roll_content(
                ticker_key,
                line_ix,
                // 显式量宽：不显式给宽时滚动容器内的文本宽度被钳进可用空间，
                // ScrollHandle 感知不到溢出（max_offset 恒 0），横向钉尾失效
                measure_ticker_width(&line, window, cx),
                line,
                ticker.exiting.as_ref(),
                ticker.rolled_in,
                ticker_color,
            );
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
                        .child(roll),
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
                        let mut open_now = false;
                        if let Some(Segment::Thinking {
                            open,
                            pinned,
                            text,
                            ticker,
                            ..
                        }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            *open = !*open;
                            open_now = *open;
                            *pinned = true;
                            // 展开/收起都把滚动行重置到最新行（ZCode：展开时滚动行卸载，
                            // 回折叠时以最新行重新挂载，不重播滚动）
                            ticker.reset_to(ticker_target_line(text));
                        }
                        // 开合动画驱动（gen 重播 + 收起时保持挂载播滑收）
                        this.drive_expand_anim(message_ix, segment_ix, None, open_now, cx);
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
            .when(open || expand_anim.collapsing, |this| {
                // 开合动画包装（滑开/滑收 + 淡入淡出）
                this.child(
                    self.expand_anim_wrap(
                        format!(
                            "thinking-expand-{message_ix}-{segment_ix}-{}",
                            expand_anim.generation
                        ),
                        expand_anim,
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
                            )
                            .into_any_element(),
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
    /// `read_ui`：Read 工具代码卡的 UI 态（换行/复制/高亮缓存；仅 Read 有值）
    /// `bash_ui`：Bash 工具代码卡的 UI 态（命令卡+输出卡；仅 Bash 有值）
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
        read_ui: Option<&ReadCardUi>,
        bash_ui: Option<&BashCardUi>,
        expand_anim: &ExpandAnim,
        body_scroll: &ScrollHandle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // AgentSwarm 工具卡升级为 Swarm 面板（kimi-code 同款）：可折叠汇总行
        //（分支图标 + 「Swarm」+ 任务标题 + 完成计数 + 箭头），展开后是母卡
        //（bot 图标方块 + 标题 + model 副标题 + 蓝色计数）+ 子代理列表（完成
        // 优先排序、行号、逐行点击开右侧子代理 tab）。无卡（live 中 SubagentCard
        // 事件到达前的瞬时态）回落下方标准工具卡渲染
        if tool == "AgentSwarm" && !agent_cards.is_empty() {
            return self.render_swarm_panel(
                message_ix,
                segment_ix,
                summary,
                live_note,
                done,
                expanded,
                agent_cards,
                expand_anim,
                cx,
            );
        }
        // 代理卡（A3c，kimi-code 同款气质）：带 SubagentCard 元信息的 Agent
        // 工具卡升级为描述卡（bot 图标 + 任务标题 + profile · model），
        // 点击开右侧子代理对话 tab；卡体不再提供展开区（完整结果与过程见右侧
        // 「子代理」tab）。
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
                        let mut expanded_now = false;
                        if let Some(Segment::ToolCall { expanded, .. }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            *expanded = !*expanded;
                            expanded_now = *expanded;
                        }
                        // 开合动画驱动（gen 重播 + 收起时保持挂载播滑收）
                        this.drive_expand_anim(message_ix, segment_ix, None, expanded_now, cx);
                        cx.notify();
                    }))
                    .child(Icon::new(tool_icon).size_4().text_color(subtlest))
                    // 工具运行中：工具名 shimmer 扫光（等批准/已结束回静态文本）。
                    // 两个分支都禁收缩+禁折行：flex 收缩按基准宽比例分摊，长命令行会
                    // 把标签挤窄几 px；中文任意字间断行，min-content 仅 1 字宽，
                    // 「终端」会被挤成两行（截断只应发生在摘要上）
                    .child(if running {
                        ShimmerText::new(kind_label)
                            .id(("tool-label-shimmer", message_ix * 1024 + segment_ix))
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_sm()
                            .text_color(subtlest)
                            .into_any_element()
                    } else {
                        div()
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(subtlest)
                            .child(kind_label.to_string())
                            .into_any_element()
                    })
                    // 成功不给标记；失败在行尾放叉号（悬停显示原因）
                    // Read：路径保持原有单一全文展示（过长截断），可点击（右侧
                    // 文件面板看全文），悬停高亮+下划线；完成后追加「N 行」。
                    // 编辑类：文件名（亮一档）+ 目录路径（最暗，优先截断）；
                    // 其余工具单行摘要。摘要只占内容宽（过长收缩截断），
                    // 让统计/箭头跟在文字后面而非靠右
                    .child(if tool == "Read" {
                        let key = message_ix * 1024 + segment_ix;
                        let path = summary.to_string();
                        let path_hover = cx.theme().foreground;
                        let line_count = if done && !is_error {
                            read_output_line_count(output)
                        } else {
                            0
                        };
                        h_flex()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .gap_1()
                            .child(
                                div()
                                    .id(("read-path", key))
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_sm()
                                    .text_color(subtle)
                                    .cursor_pointer()
                                    .hover(move |this| this.text_color(path_hover).underline())
                                    .tooltip(move |window, cx| {
                                        Tooltip::new("在右侧查看完整文件").build(window, cx)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        let line = this
                                            .messages
                                            .get(message_ix)
                                            .and_then(|m| m.segments.get(segment_ix))
                                            .and_then(|s| match s {
                                                Segment::ToolCall { output, .. } => {
                                                    read_output_first_line(output)
                                                }
                                                _ => None,
                                            });
                                        cx.emit(ThreadEvent::OpenFile {
                                            path: path.clone(),
                                            line,
                                        });
                                    }))
                                    .child(summary.to_string()),
                            )
                            .when(line_count > 0, |this| {
                                this.child(
                                    div()
                                        .flex_shrink_0()
                                        .whitespace_nowrap()
                                        .text_sm()
                                        .text_color(subtlest)
                                        .child(format!("{line_count} 行")),
                                )
                            })
                            .into_any_element()
                    } else if edit.is_some() {
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
                                .flex_shrink_0()
                                .whitespace_nowrap()
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
            .when(expanded || expand_anim.collapsing, |this| {
                // Read 完成且输出是带行号的文件内容 → 代码卡（read.rs）；
                // 运行中/报错/非内容输出（空文件、未变化）回落通用卡。
                // Bash 收尾（含失败）→ 命令卡 + 输出卡（bash.rs）；
                // 运行中/等审批走通用卡（实时输出）
                let read_card = tool == "Read"
                    && done
                    && !is_error
                    && read_ui.is_some()
                    && is_read_code_output(output);
                let bash_card = tool == "Bash" && done && bash_ui.is_some();
                // 展开正文统一放进带滚动条的视口（track_scroll 持久滚动位置 + 可见滚动条）；
                // 内容整体包一层开合动画（滑开/滑收 + 淡入淡出）
                this.child(
                    div().relative().mt_2().w_full().child(
                        self.expand_anim_wrap(
                            format!(
                                "tool-expand-{message_ix}-{segment_ix}-{}",
                                expand_anim.generation
                            ),
                            expand_anim,
                            div()
                                .relative()
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
                                } else if read_card {
                                    match read_ui {
                                        Some(ui) => self.render_read_card(
                                            message_ix,
                                            segment_ix,
                                            summary,
                                            output,
                                            ui,
                                            body_scroll,
                                            window,
                                            cx,
                                        ),
                                        None => div().into_any_element(),
                                    }
                                } else if bash_card {
                                    match bash_ui {
                                        Some(ui) => self.render_bash_card(
                                            message_ix, segment_ix, summary, output, is_error, ui,
                                            window, cx,
                                        ),
                                        None => div().into_any_element(),
                                    }
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
                                                            div()
                                                                .text_sm()
                                                                .text_color(subtle)
                                                                .child("$"),
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
                                // diff 卡与 Read/Bash 代码卡的滚动条已内置（随圆角补丁收角）；通用卡的补在这里
                                .when(edit.is_none() && !read_card && !bash_card, |this| {
                                    this.child(Scrollbar::vertical(body_scroll))
                                })
                                .into_any_element(),
                        ),
                    ),
                )
            })
            .into_any_element()
    }

    /// 代理卡：子代理 Agent 工具卡的升级样式（A3c，kimi-code 同款气质）——
    /// 圆角卡 + bot 图标方块 + 任务描述标题 + `{profile} · {model}` 副标题；
    /// 前台运行中多一行实时进度（段级 live_note；后台卡用卡级 live_note）；
    /// 右侧状态：等待批准/Spinner/成功勾/失败词。
    /// 点击卡体开右侧子代理对话 tab（完整结果与过程在那里看，故不提供展开区）。
    /// `card_ix`：同一工具卡里的第几张（防御多卡；Agent 恒为 0，AgentSwarm
    /// 走 render_swarm_panel 不到这里）。
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
        // 运行态真值表：后台卡的工具调用立即收尾（running 回执），真实运行态由
        // 子代理生命周期驱动（SubagentActivity finished 置卡级 finished；回放由
        // core 补发）；前台卡跟工具调用同生命周期（!done；等审批暂停转圈）——
        // 前台 Swarm 的单卡同样由 finished 提前落终态（先完成的子代理不等整批）
        let running = if card.background {
            !card.finished
        } else {
            !done && !card.finished && !approval_pending
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

    /// Swarm 面板：AgentSwarm 工具卡的升级样式（kimi-code 同款）——
    /// 可折叠汇总行（分支图标 + 「Swarm」+ 任务标题 + `{完成}/{总数}` + 箭头）；
    /// 展开后（折叠态复用段级 `expanded`，Swarm 卡默认展开）是母卡（bot 图标
    /// 方块 + 任务标题 + model 副标题 + 蓝色完成计数，点击同样折叠）+ 子代理
    /// 列表（圆角描边容器，逐行「{子代理名} ({profile})」+ 状态 + 两位行号 +
    /// 箭头；名字里的 #n 是发起方命名，UI 不追加序号）。子代理按完成先后排序
    /// （finished_seq；回放无此事件落回发起序），行号跟随显示序。
    /// 点击子行开右侧子代理对话 tab。
    /// 标题/副标题取首张子代理卡（同一 swarm 的子代理同 template/profile/model，
    /// 首卡即代表）；运行中汇总行标签扫光（与工具行同 idiom）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_swarm_panel(
        &self,
        message_ix: usize,
        segment_ix: usize,
        summary: &str,
        live_note: Option<&str>,
        done: bool,
        open: bool,
        cards: &[AgentCardMeta],
        expand_anim: &ExpandAnim,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let key = message_ix * 1024 + segment_ix;
        // 行级终态：后台卡看子代理真实生命周期（card.finished）；前台卡随工具
        // 调用收尾全体落终态（done；逐卡 finished 由 SubagentActivity 提前落位）
        let row_done = |card: &AgentCardMeta| card.finished || (done && !card.background);
        let finished_count = cards.iter().filter(|card| row_done(card)).count();
        let total = cards.len();
        let count_text = format!("{finished_count} / {total}");
        let running = finished_count < total;
        // 标题/副标题取首卡为代表（同质 swarm 全卡同值）；空描述回退工具摘要
        let first = &cards[0];
        let title = if first.description.is_empty() {
            summary.to_string()
        } else {
            first.description.clone()
        };
        let subtitle = if first.model.is_empty() {
            first.profile.clone()
        } else {
            first.model.clone()
        };
        // 显示序：已结束的按完成次序在前，未完成的保持发起序在后；
        // 回放没有 finished_seq（全 None）→ 稳定保持发起序
        let mut order: Vec<usize> = (0..total).collect();
        order.sort_by_key(|&ix| match cards[ix].finished_seq {
            Some(seq) => (0, seq),
            None => (1, ix as u64),
        });
        // 汇总行/母卡共用同一个折叠开关（写段级 expanded）；listener 返回值不
        // 可 Clone，两处各写一份
        let header = h_flex()
            .id(("swarm-header", key))
            .w_full()
            .items_center()
            .gap_2()
            .py_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let mut open_now = false;
                if let Some(Segment::ToolCall { expanded, .. }) = this
                    .messages
                    .get_mut(message_ix)
                    .and_then(|m| m.segments.get_mut(segment_ix))
                {
                    *expanded = !*expanded;
                    open_now = *expanded;
                }
                this.drive_expand_anim(message_ix, segment_ix, None, open_now, cx);
                cx.notify();
            }))
            .child(
                Icon::new(AssetIconName::Share2)
                    .size_4()
                    .text_color(subtlest),
            )
            // 运行中：标签 shimmer 扫光（与工具行同 idiom），收尾回静态文本
            .child(if running {
                ShimmerText::new("Swarm")
                    .id(("swarm-label-shimmer", key))
                    .text_sm()
                    .text_color(subtle)
                    .into_any_element()
            } else {
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(subtle)
                    .child("Swarm")
                    .into_any_element()
            })
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_sm()
                    .text_color(subtle)
                    .child(title.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(subtlest)
                    .child(count_text.clone()),
            )
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size_4()
                .text_color(subtlest),
            );

        // 母卡：bot 图标方块 + 标题/model 副标题 + 蓝色完成计数（点击折叠）。
        // 底色之外再描边：group_box 与页面底色接近的主题下只靠填充卡面会「隐身」
        let parent = h_flex()
            .id(("swarm-parent", key))
            .w_full()
            .items_center()
            .gap_3()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().group_box)
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(cx.listener(move |this, _, _, cx| {
                let mut open_now = false;
                if let Some(Segment::ToolCall { expanded, .. }) = this
                    .messages
                    .get_mut(message_ix)
                    .and_then(|m| m.segments.get_mut(segment_ix))
                {
                    *expanded = !*expanded;
                    open_now = *expanded;
                }
                this.drive_expand_anim(message_ix, segment_ix, None, open_now, cx);
                cx.notify();
            }))
            .child(
                div()
                    .flex_shrink_0()
                    .w_10()
                    .h_10()
                    .rounded_md()
                    .bg(cx.theme().accent)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::Bot)
                            .size_5()
                            .text_color(cx.theme().foreground),
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
                            .text_color(subtle)
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(cx.theme().info)
                    .child(count_text),
            );

        // 子代理列表：圆角描边容器（行悬停底色越角处由补丁收住，与 diff 卡同款），
        // 逐行「{子代理名} ({profile})」+ 状态 + 两位行号 + 箭头
        let border = cx.theme().border;
        // 列表背后 = 页面底色（消息区自身透明，与 Root 的 tokens.background 同值）
        let behind = cx.theme().background;
        let rows: Vec<AnyElement> = order
            .iter()
            .enumerate()
            .map(|(row_ix, &ix)| {
                let card = &cards[ix];
                let card_done = row_done(card);
                // 运行中的进度行：后台卡用卡级 live_note（SubagentActivity 写入），
                // 前台卡共用段级 live_note（SubagentProgress 按 item_id 写入）
                let note = if card.background {
                    card.live_note.as_deref()
                } else {
                    live_note
                };
                let base_title = if card.description.is_empty() {
                    "子代理".to_string()
                } else {
                    card.description.clone()
                };
                // 标题即子代理名（名里带不带 #n 由发起方决定，UI 不追加序号）
                let row_title = format!("{base_title} ({})", card.profile);
                let tab_title = base_title.clone();
                let agent_id = card.agent_id.clone();
                h_flex()
                    .id(("swarm-row", key * 256 + ix))
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py(px(5.))
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ThreadEvent::OpenSubagent {
                            agent_id: agent_id.clone(),
                            title: tab_title.clone(),
                        });
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(row_title),
                    )
                    // 右侧状态：已结束 = 绿勾 + 词；运行中 = Spinner + 进度行
                    // （无进度回退「运行中」）。SubagentActivity finished 不带成败，
                    // 勾仅代表「跑完」（子代理失败由通知气泡呈现）
                    .child(if card_done {
                        h_flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .child(
                                Icon::new(IconName::CircleCheck)
                                    .size_4()
                                    .text_color(cx.theme().success),
                            )
                            .child(div().text_xs().text_color(subtle).child("已结束"))
                            .into_any_element()
                    } else {
                        let note = note
                            .map(|n| n.split_whitespace().collect::<Vec<_>>().join(" "))
                            .filter(|n| !n.is_empty());
                        h_flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .child(Spinner::new().small().color(subtlest))
                            .child(
                                div()
                                    .max_w(px(240.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_xs()
                                    .text_color(subtle)
                                    .child(note.unwrap_or_else(|| "运行中".to_string())),
                            )
                            .into_any_element()
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .w_5()
                            .text_right()
                            .text_xs()
                            .text_color(subtlest)
                            .child(format!("{:02}", row_ix + 1)),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .size_4()
                            .text_color(subtlest),
                    )
                    .into_any_element()
            })
            .collect();
        let list = div()
            .relative()
            .w_full()
            .child(
                v_flex()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(border)
                    .py_1()
                    .children(rows),
            )
            .child(
                canvas(
                    |bounds, window, _| (bounds, rems(0.5).to_pixels(window.rem_size())),
                    move |bounds, (_, radius), window, _| {
                        Self::paint_rounded_corner_patches(bounds, radius, behind, window);
                    },
                )
                .absolute()
                .inset_0(),
            )
            // 补丁盖住了角上的描边，重描一遍圆角边框
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded_lg()
                    .border_1()
                    .border_color(border),
            );

        v_flex()
            .w_full()
            .child(header)
            .when(open || expand_anim.collapsing, |this| {
                // 开合动画包装（滑开/滑收 + 淡入淡出）
                this.child(
                    self.expand_anim_wrap(
                        format!("swarm-expand-{key}-{}", expand_anim.generation),
                        expand_anim,
                        // 缩进用 padding 而非 margin：w_full 子级不会因外边距溢出
                        div()
                            .w_full()
                            .pl(px(24.))
                            .pt_1()
                            .child(v_flex().w_full().gap_2().child(parent).child(list))
                            .into_any_element(),
                    ),
                )
            })
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

/// 思考滚动行的纵滚容器（从 render_thinking 抽出，布局回归测试复用同一构造）。
/// 单行高、纵向裁切：滚入行自下方 +0.8em 起、滚出行向上 -0.8em 止，超高部分由
/// 外层 viewport 的滚动 mask 裁掉（overflow 任一轴非 visible 即按 bounds 双轴裁剪，
/// 见 gpui style::overflow_mask；容器自身不设 overflow，也不依赖它裁切）。
/// 退场行 absolute 不参与布局（ZCode popLayout 同款）。
/// 动画 id 含行号：换行号才重播，同行追加（同 id）原位刷新不重启动画。
///
/// `width` 必须是调用方量出的文本自然宽（measure_ticker_width）：不显式给宽时，
/// 滚动容器内的内容会被布局钳到视口宽，ScrollHandle 感知不到溢出（max_offset
/// 恒 0），横向钉尾失效（sidebar 跑马灯同款坑，2026-09-30 在滚动行上重现）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn ticker_roll_content(
    ticker_key: usize,
    line_ix: usize,
    width: Pixels,
    line: String,
    exiting: Option<&(usize, String)>,
    rolled_in: bool,
    color: Hsla,
) -> AnyElement {
    let mut roll = div()
        .flex_none()
        .relative()
        .w(width)
        .whitespace_nowrap()
        .text_sm();
    if let Some((exit_ix, exit_line)) = exiting {
        roll = roll.child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .text_color(color)
                .child(exit_line.clone())
                .with_animation(
                    format!("thinking-ticker-exit-{ticker_key}-{exit_ix}"),
                    Animation::new(TICKER_ROLL_TRANSITION).with_easing(ticker_roll_easing),
                    |el, delta| {
                        el.top(px(-TICKER_ROLL_OFFSET_PX * delta))
                            .opacity(1.0 - delta)
                    },
                ),
        );
    }
    let entering = div().text_color(color).child(line);
    roll.child(if rolled_in {
        entering
            .with_animation(
                format!("thinking-ticker-enter-{ticker_key}-{line_ix}"),
                Animation::new(TICKER_ROLL_TRANSITION).with_easing(ticker_roll_easing),
                |el, delta| {
                    el.top(px(TICKER_ROLL_OFFSET_PX * (1.0 - delta)))
                        .opacity(delta)
                },
            )
            .into_any_element()
    } else {
        entering.into_any_element()
    })
    .into_any_element()
}

/// 用文本系统量出思考滚动行的自然单行宽度（sidebar 跑马灯 `measure_title_width`
/// 同款）：不显式量宽时，滚动容器内的文本宽度会被布局钳进可用空间，ScrollHandle
/// 感知不到溢出（max_offset 恒 0），横向钉尾失效。text_sm = 0.875rem；+2px 防
/// 字宽取整误差
pub(crate) fn measure_ticker_width(text: &str, window: &Window, cx: &App) -> Pixels {
    let font_size = rems(0.875).to_pixels(window.rem_size());
    let font = Font {
        family: cx.theme().font_family.clone(),
        ..Font::default()
    };
    window
        .text_system()
        .shape_line(
            SharedString::from(text.to_string()),
            font_size,
            &[TextRun {
                len: text.len(),
                font,
                color: black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        )
        .width
        + px(2.)
}

/// 思考滚动行的纵滚缓动：ZCode QueuedSummaryContent 的 CSS cubic-bezier(0.4, 0, 0.2, 1)。
/// gpui 无内置 cubic_bezier，这里按 CSS 语义实现：Newton-Raphson 解 x(t) = 输入进度，
/// 再取对应 y(t)
fn ticker_roll_easing(x: f32) -> f32 {
    const X1: f32 = 0.4;
    const Y1: f32 = 0.0;
    const X2: f32 = 0.2;
    const Y2: f32 = 1.0;
    fn curve(t: f32, a1: f32, a2: f32) -> f32 {
        let u = 1.0 - t;
        3.0 * u * u * t * a1 + 3.0 * u * t * t * a2 + t * t * t
    }
    let x = x.clamp(0.0, 1.0);
    let mut t = x;
    for _ in 0..8 {
        let err = curve(t, X1, X2) - x;
        if err.abs() < 1e-4 {
            break;
        }
        let d = 3.0 * (1.0 - t) * (1.0 - t) * X1
            + 6.0 * (1.0 - t) * t * (X2 - X1)
            + 3.0 * t * t * (1.0 - X2);
        if d.abs() < 1e-6 {
            break;
        }
        t = (t - err / d).clamp(0.0, 1.0);
    }
    curve(t, Y1, Y2)
}
