use super::*;

impl ThreadView {
    /// Thinking collapsed block (same as ZCode reasoning.tsx): a borderless one-line header (brain icon + label);
    /// in progress the label is a shimmering "Thinking", followed by `·` plus a rolling output line (provided by the vertical-roll state machine: on line change the old
    /// line rolls up and out while the new line rolls in from below, tail-pinned to the latest content with a leading-edge fade mask; the vertical wheel bubbles to the outer
    /// message list); the arrow shows only on hover/expand; expanded, the body is indented behind a left vertical line and scrolls internally past its height cap.
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
            Some(d) => rust_i18n::t!("thread.thinking_done", n = secs(d)).to_string(),
            // In progress shows only "Thinking" (ZCode: the seconds count appears only in the done state)
            None if self.streaming && !self.replay_turn => {
                rust_i18n::t!("thread.thinking").to_string()
            }
            // History segments rebuilt by replay have no real clock
            None => rust_i18n::t!("thread.thinking_unknown").to_string(),
        };
        // The rolling output line comes from the segment's vertical-roll state machine (TickerRoll, see model.rs); shown only while collapsed and in progress
        let ticker_line = if in_progress && !open {
            ticker.displayed.clone()
        } else {
            None
        };
        let muted = cx.theme().muted_foreground;
        let subtlest = muted.opacity(0.6);
        // ZCode: the rolling line is one step brighter than the label (subtle vs subtlest)
        let ticker_color = muted.opacity(0.85);
        let group_id = format!("thinking-row-{message_ix}-{segment_ix}");
        let ticker_key = message_ix * 1024 + segment_ix;
        // Rolling-line tail pin: offset goes negative when scrolled right, so tail pin = -max (0 before the first measure, converging quickly during streaming)
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
            // Vertical-roll container (same as ZCode QueuedSummaryContent): single-line height, vertical clipping; the exiting
            // line is absolute and takes no part in layout (same as popLayout). The construction is factored into a free function for reuse by layout regression tests;
            // see ticker_roll_content for semantics
            let roll = ticker_roll_content(
                ticker_key,
                line_ix,
                // Explicit width: without one the text width inside the scroll container is clamped into the available space,
                // the ScrollHandle never sees the overflow (max_offset stays 0), and horizontal tail pinning breaks
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
                // Wheel takeover: horizontal scrolling is consumed by this line; the vertical wheel bubbles to the outer message list
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
                            // Both expand and collapse reset the rolling line to the latest one (ZCode: the rolling line unmounts while expanded
                            // and remounts on the latest line when back to collapsed, without replaying the roll)
                            ticker.reset_to(ticker_target_line(text));
                        }
                        // Expand/collapse animation driver (gen replays the animation; on collapse it stays mounted to play the slide-shut)
                        this.drive_expand_anim(message_ix, segment_ix, open_now, cx);
                        cx.notify();
                    }))
                    .child(
                        Icon::new(AssetIconName::Brain)
                            .size_4()
                            .text_color(subtlest),
                    )
                    // Thinking in progress: shimmer sweep highlight; the id must be stable (the default animation id derives from the label,
                    // and any change would restart the sweep)
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
                    // In-progress rolling output line (same as the ZCode reasoning trigger)
                    .when(ticker.is_some(), |this| {
                        this.child(div().text_sm().text_color(subtlest).child("·"))
                    })
                    .children(ticker)
                    // Arrow hidden by default, shown on row hover or when expanded
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
                // Expand/collapse animation wrap (slide open/shut plus fade in/out)
                this.child(
                    self.expand_anim_wrap(
                        format!(
                            "thinking-expand-{message_ix}-{segment_ix}-{}",
                            expand_anim.generation
                        ),
                        expand_anim,
                        // The wrap layer carries the scroll-chain handling: when the body can scroll it swallows the wheel, so the outer message list does not chain along
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

    /// ExitPlanMode plan card (same as kimi "Plan pending/approved"): a collapsed one-line row with three states,
    /// the chevron expands to the full plan markdown (TextView, internal scrolling beyond 480px)
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_plan_row(
        &self,
        message_ix: usize,
        segment_ix: usize,
        state: &Entity<TextViewState>,
        done: bool,
        approved: bool,
        open: bool,
        expand_anim: &ExpandAnim,
        body_scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (status, status_color) = if !done {
            (rust_i18n::t!("thread.plan_pending"), cx.theme().warning)
        } else if approved {
            (rust_i18n::t!("thread.plan_approved"), cx.theme().success)
        } else {
            (rust_i18n::t!("thread.plan_rejected"), cx.theme().danger)
        };
        let muted = cx.theme().muted_foreground;
        let subtlest = muted.opacity(0.6);
        let key = message_ix * 1024 + segment_ix;
        let group_id = format!("plan-row-{message_ix}-{segment_ix}");
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("plan-row", key))
                    .test_support()
                    .group(group_id.clone())
                    .w_full()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let mut open_now = false;
                        if let Some(Segment::Plan { open, .. }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            *open = !*open;
                            open_now = *open;
                        }
                        this.drive_expand_anim(message_ix, segment_ix, open_now, cx);
                        cx.notify();
                    }))
                    .child(
                        Icon::new(AssetIconName::ClipboardList)
                            .size_4()
                            .text_color(subtlest),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(subtlest)
                            .child(rust_i18n::t!("thread.plan")),
                    )
                    .child(div().text_sm().text_color(status_color).child(status))
                    // Arrow hidden by default, shown on row hover or when expanded
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
                // Expand/collapse animation wrap (slide open/shut plus fade in/out)
                this.child(
                    self.expand_anim_wrap(
                        format!(
                            "plan-expand-{message_ix}-{segment_ix}-{}",
                            expand_anim.generation
                        ),
                        expand_anim,
                        // The wrap layer carries the scroll-chain handling: when the body can scroll it swallows the wheel, so the outer message list does not chain along
                        div()
                            .relative()
                            .on_scroll_wheel(consume_scroll(body_scroll))
                            .child(
                                div()
                                    .id(("plan-row-body", key))
                                    .test_support()
                                    .mt_1()
                                    .rounded(px(10.))
                                    .bg(cx.theme().background)
                                    .p_3()
                                    .max_h(px(480.))
                                    .overflow_y_scroll()
                                    .track_scroll(body_scroll)
                                    .child(TextView::new(state).selectable(true).text_sm()),
                            )
                            .into_any_element(),
                    ),
                )
            })
            .into_any_element()
    }

    /// Tool call (same as ZCode): a borderless summary row (icon + localized tool name + one-line summary + status word),
    /// the arrow shows only on hover/expand; expanded it becomes a rounded outlined card: full input (terminal tools get a `$` prefix) plus
    /// monospace output, with the output height-capped and internally scrollable. No spinner while running; the tool name shimmers instead (ZCode's tradeoff:
    /// many tools run during streaming, and a persistent animation would burn rendering resources).
    /// `approval_pending`: this tool is awaiting approval (a yellow "Waiting for approval" at the end of the row).
    /// `agent_cards`: agent card list written by SubagentCard (one for Agent, several for AgentSwarm).
    /// `read_ui`: UI state of the Read tool code card (wrap/copy/highlight caches; only set for Read)
    /// `bash_ui`: UI state of the Bash tool code card (command card plus output card; only set for Bash)
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
        // The AgentSwarm tool card upgrades to a Swarm panel (same as kimi-code): a collapsible summary row
        // (branch icon + "Swarm" + task title + completion count + arrow); expanded it shows the parent card
        // (bot icon tile + title + model subtitle + blue count) plus a subagent list (finished-first
        // ordering, row numbers, click any row to open the right-side subagent tab). With no cards (the transient state in live before the SubagentCard
        // event arrives) it falls back to the standard tool card rendering below
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
        // Agent cards (A3c, kimi-code style): Agent tool calls carrying SubagentCard
        // metadata upgrade to description cards (bot icon + task title + profile · model);
        // click to open the right-side subagent conversation tab; the card body no longer offers an expand area (full results and process live in the right-side
        // "subagent" tab).
        // With no cards (the transient state in live before the SubagentCard event arrives) it falls back to the standard tool card rendering below.
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
        // ZCode's three-tier text hierarchy: body > subtle(60%) > subtlest(30~40%), building information density through hierarchy rather than borders/color
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
            "Bash" => rust_i18n::t!("thread.tool_bash"),
            "Read" => rust_i18n::t!("thread.tool_read"),
            "Write" => rust_i18n::t!("thread.tool_write"),
            "Edit" => rust_i18n::t!("thread.tool_edit"),
            "Glob" => rust_i18n::t!("thread.tool_glob"),
            "Grep" => rust_i18n::t!("thread.tool_grep"),
            "TodoList" => rust_i18n::t!("thread.tool_todo"),
            "FetchURL" => rust_i18n::t!("thread.tool_fetch"),
            "TaskList" | "TaskOutput" | "TaskStop" => rust_i18n::t!("thread.tool_bg_task"),
            "AskUserQuestion" => rust_i18n::t!("thread.tool_ask"),
            _ => std::borrow::Cow::Borrowed(tool),
        };
        // Collapse the summary to one line: newlines in multi-line commands fold into spaces (otherwise the collapsed row would stretch into several lines)
        let summary_line = summary.split_whitespace().collect::<Vec<_>>().join(" ");

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("tool", message_ix * 1024 + segment_ix))
                    .test_support()
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
                        // Expand/collapse animation driver (gen replays the animation; on collapse it stays mounted to play the slide-shut)
                        this.drive_expand_anim(message_ix, segment_ix, expanded_now, cx);
                        cx.notify();
                    }))
                    .child(Icon::new(tool_icon).size_4().text_color(subtlest))
                    // Tool running: the tool name shimmers (back to static text while awaiting approval or finished).
                    // Both branches disable shrinking and wrapping: flex shrinking distributes by base width, so a long command line would
                    // squeeze the label narrower by a few px; Chinese wraps between any two characters (min-content is one character wide),
                    // so "Terminal" would be squeezed onto two lines (truncation should happen only on the summary)
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
                    // No marker on success; a cross at the row end on failure (hover shows the reason)
                    // Read: the path keeps its single full-text display (truncated when too long), clickable (view the
                    // full text in the right-side file panel), hover highlight plus underline; appends "N lines" when done.
                    // Edit tools: file name (one step brighter) plus directory path (darkest, truncated first);
                    // other tools get a one-line summary. The summary takes only content width (shrink-truncate when too long)
                    // so the stats/arrow follow the text instead of hugging the right edge
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
                                        Tooltip::new(
                                            rust_i18n::t!("thread.view_full_file").to_string(),
                                        )
                                        .build(window, cx)
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
                                        .child(
                                            rust_i18n::t!("thread.lines", n = line_count)
                                                .to_string(),
                                        ),
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
                    // Additions/deletions count (monospace; the zero side is not shown)
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
                                .child(rust_i18n::t!("thread.waiting_approval")),
                        )
                    })
                    // Arrow hidden by default, shown on row hover or when expanded (keeps the row clean)
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
                    // Failure: the row-end cross is always visible; hover shows the failure reason (output collapsed to one line and truncated)
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
            // Live progress row of a foreground subagent while running (below the summary row): small spinner plus single-line ellipsis,
            // left-indented to align with the summary row's icon column (icon 16px + gap 8px); absent when finished or in replay
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
                // Read finished with line-numbered file content → code card (read.rs);
                // running/error/non-content output (empty file, unchanged) falls back to the generic card.
                // Bash finished (failure included) → command card plus output card (bash.rs);
                // running or awaiting approval uses the generic card (live output)
                let read_card = tool == "Read"
                    && done
                    && !is_error
                    && read_ui.is_some()
                    && is_read_code_output(output);
                let bash_card = tool == "Bash" && done && bash_ui.is_some();
                // The expanded body uniformly goes into a viewport with a scrollbar (track_scroll keeps the scroll position plus a visible scrollbar);
                // the content as a whole is wrapped in the expand/collapse animation (slide open/shut plus fade in/out)
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
                                // Scroll chain: when the body can scroll it swallows the wheel, so the outer message list does not chain along
                                .on_scroll_wheel(consume_scroll(body_scroll))
                                // Edit tools expand into an inline diff code card; other tools get the generic input+output card
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
                                        // Full input: terminal tools get the `$` prefix; other tools show the full text (the part truncated in the collapsed row)
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
                                                    rust_i18n::t!("thread.no_output").to_string()
                                                } else {
                                                    output.to_string()
                                                }),
                                        )
                                        .into_any_element()
                                })
                                // The diff card and Read/Bash code cards have built-in scrollbars (corners trimmed by the rounded-corner patches); the generic card gets its scrollbar here
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

    /// Agent card: the upgraded style of the subagent Agent tool card (A3c, kimi-code style):
    /// rounded card + bot icon tile + task description title + `{profile} · {model}` subtitle;
    /// a foreground run gains one live progress row while running (segment-level live_note; background cards use the card-level live_note);
    /// right-side status: awaiting approval / Spinner / success check / failure word.
    /// Clicking the card body opens the right-side subagent conversation tab (full results and process are viewed there, so no expand area is offered).
    /// `card_ix`: which card within the same tool card (guards against multiple cards; Agent is always 0, and AgentSwarm
    /// goes through render_swarm_panel and never reaches here).
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
        // Run-state truth table: the background card's tool call ends immediately (running receipt), and the real run state is driven by the
        // subagent lifecycle (SubagentActivity finished sets the card-level finished; replay re-emits it from
        // core); the foreground card follows the tool call's lifecycle (!done; the spinner pauses while awaiting approval).
        // A single card of a foreground Swarm likewise lands its terminal state early via finished (a subagent that finishes first does not wait for the whole batch)
        let running = if card.background {
            !card.finished
        } else {
            !done && !card.finished && !approval_pending
        };
        // The background card's live progress lives in the card-level live_note (written by SubagentActivity keyed by agent_id);
        // the foreground card uses the segment-level live_note (written by SubagentProgress keyed by item_id)
        let progress_note = if card.background {
            card.live_note.as_deref()
        } else {
            live_note
        };
        let failed = done && is_error;
        let title = if card.description.is_empty() {
            rust_i18n::t!("thread.subagent").to_string()
        } else {
            card.description.clone()
        };
        let subtitle = if failed {
            rust_i18n::t!("thread.failed").to_string()
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
            // Bot icon tile (rounded, faint info background)
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
                    // Live progress row while running (moved into the card: spinner plus single-line ellipsis)
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
            // Right-side status: awaiting approval (foreground only) / running Spinner / failure word / success check.
            // Note: SubagentActivity's finished carries no success flag; a background card's terminal outcome reuses
            // the tool receipt's is_error (subagent failure is surfaced by a notification bubble; the check on the card only means "finished running")
            .child(if !card.background && approval_pending {
                div()
                    .text_xs()
                    .text_color(cx.theme().warning)
                    .child(rust_i18n::t!("thread.waiting_approval"))
                    .into_any_element()
            } else if running {
                Spinner::new().small().color(subtlest).into_any_element()
            } else if failed {
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(rust_i18n::t!("thread.failed"))
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

    /// Swarm panel: the upgraded style of the AgentSwarm tool card (same as kimi-code):
    /// a collapsible summary row (branch icon + "Swarm" + task title + `{finished}/{total}` + arrow);
    /// expanded (the collapsed state reuses the segment-level `expanded`; Swarm cards default to expanded) it is the parent card (bot icon
    /// tile + task title + model subtitle + blue finished count; clicking collapses it too) plus the subagent
    /// list (rounded outlined container, one row per "{subagent name} ({profile})" + status + two-digit row number +
    /// arrow; the #n inside names comes from the launcher, and the UI appends no index). Subagents are ordered by finish time
    /// (finished_seq; replay lacks this event and falls back to launch order), and row numbers follow the display order.
    /// Clicking a row opens the right-side subagent conversation tab.
    /// Title/subtitle come from the first subagent card (all subagents of one swarm share template/profile/model,
    /// so the first card represents them); while running the summary row label shimmers (same idiom as the tool row).
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
        // Row-level terminal state: background cards follow the subagent's real lifecycle (card.finished); foreground cards all
        // land their terminal state when the tool call ends (done; per-card finished is set early by SubagentActivity)
        let row_done = |card: &AgentCardMeta| card.finished || (done && !card.background);
        let finished_count = cards.iter().filter(|card| row_done(card)).count();
        let total = cards.len();
        let count_text = format!("{finished_count} / {total}");
        let running = finished_count < total;
        // Title/subtitle take the first card as the representative (a homogeneous swarm has identical values on every card); an empty description falls back to the tool summary
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
        // Display order: finished ones come first by finish order, unfinished ones keep launch order behind them;
        // replay has no finished_seq (all None) → stably keeps launch order
        let mut order: Vec<usize> = (0..total).collect();
        order.sort_by_key(|&ix| match cards[ix].finished_seq {
            Some(seq) => (0, seq),
            None => (1, ix as u64),
        });
        // The summary row and parent card share one collapse toggle (writing the segment-level expanded); listener return values are not
        // Clone, so each site gets its own copy
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
                this.drive_expand_anim(message_ix, segment_ix, open_now, cx);
                cx.notify();
            }))
            .child(
                Icon::new(AssetIconName::Share2)
                    .size_4()
                    .text_color(subtlest),
            )
            // While running: the label shimmers (same idiom as the tool row); back to static text when finished
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

        // Parent card: bot icon tile + title/model subtitle + blue finished count (click to collapse).
        // Outlined on top of the fill: in themes where group_box is close to the page background, a fill-only card would "vanish"
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
                this.drive_expand_anim(message_ix, segment_ix, open_now, cx);
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

        // Subagent list: rounded outlined container (row-hover backgrounds are trimmed at the corners by patches, same as the diff card),
        // one row per "{subagent name} ({profile})" + status + two-digit row number + arrow
        let border = cx.theme().border;
        // Behind the list = the page background (the message area itself is transparent, the same value as Root's tokens.background)
        let behind = cx.theme().background;
        let rows: Vec<AnyElement> = order
            .iter()
            .enumerate()
            .map(|(row_ix, &ix)| {
                let card = &cards[ix];
                let card_done = row_done(card);
                // Progress row while running: background cards use the card-level live_note (written by SubagentActivity),
                // foreground cards share the segment-level live_note (written by SubagentProgress keyed by item_id)
                let note = if card.background {
                    card.live_note.as_deref()
                } else {
                    live_note
                };
                let base_title = if card.description.is_empty() {
                    rust_i18n::t!("thread.subagent").to_string()
                } else {
                    card.description.clone()
                };
                // The title is the subagent name (whether #n is in the name is decided by the launcher; the UI appends no index)
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
                    // Right-side status: finished = green check + word; running = Spinner + progress row
                    // (falls back to "Running" without progress). SubagentActivity finished carries no success flag;
                    // the check only means "finished running" (subagent failure is surfaced by a notification bubble)
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
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(subtle)
                                    .child(rust_i18n::t!("thread.ended")),
                            )
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
                                    .child(note.unwrap_or_else(|| {
                                        rust_i18n::t!("thread.running").to_string()
                                    })),
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
            // The patches cover the corner strokes; re-stroke the rounded border once more
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
                // Expand/collapse animation wrap (slide open/shut plus fade in/out)
                this.child(
                    self.expand_anim_wrap(
                        format!("swarm-expand-{key}-{}", expand_anim.generation),
                        expand_anim,
                        // Indent with padding, not margin: a w_full child would not overflow via outer margins
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

    /// Patches the four corners of a rounded card: fills each corner's "R×R square minus the quarter circle of radius R" region
    /// (the rounded-corner notch). gpui's ContentMask only clips rectangles, so row backgrounds/color bars/scrollbars
    /// would all bleed past the rounded stroke; once the notches are filled with the color **behind** the card, the content is visually
    /// gathered by the rounded corners and stays inside the frame. The curve approximates the quarter circle with a quadratic Bezier (control point at the outer corner,
    /// deviation <0.5px). Callers must re-stroke the rounded border once more after this (the patches cover the corner strokes).
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
        // Top-left
        path.move_to(point(px(0.), px(0.)));
        path.line_to(point(r, px(0.)));
        path.curve_to(point(px(0.), r), point(px(0.), px(0.)));
        path.close();
        // Top-right
        path.move_to(point(w, px(0.)));
        path.line_to(point(w, r));
        path.curve_to(point(w - r, px(0.)), point(w, px(0.)));
        path.close();
        // Bottom-left
        path.move_to(point(px(0.), h));
        path.line_to(point(px(0.), h - r));
        path.curve_to(point(r, h), point(px(0.), h));
        path.close();
        // Bottom-right
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

/// The vertical-roll container of the thinking rolling line (extracted from render_thinking; layout regression tests reuse the same construction).
/// Single-line height, vertical clipping: the entering line starts at +0.8em from below and the exiting line stops at -0.8em upward; anything taller is
/// clipped away by the outer viewport's scroll mask (overflow on any non-visible axis clips both axes by bounds,
/// see gpui style::overflow_mask; the container itself sets no overflow and does not rely on it for clipping).
/// The exiting line is absolute and takes no part in layout (same as ZCode popLayout).
/// The animation id includes the line number: only a changed number replays; appending to the same line (same id) refreshes in place without restarting the animation.
///
/// `width` must be the natural text width measured by the caller (measure_ticker_width): without an explicit width,
/// the content inside the scroll container is clamped by layout to the viewport width, the ScrollHandle never sees the overflow (max_offset
/// stays 0), and horizontal tail pinning breaks (the same pitfall as the sidebar marquee, re-hit on the rolling line on 2026-09-30).
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

/// Measures the natural single-line width of the thinking rolling line with the text system (same as the sidebar marquee's `measure_title_width`):
/// without an explicit measurement, the text width inside the scroll container is clamped into the available space, the ScrollHandle
/// never sees the overflow (max_offset stays 0), and horizontal tail pinning breaks. text_sm = 0.875rem; plus 2px to guard against
/// font-width rounding error
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

/// Vertical-roll easing of the thinking rolling line: the CSS cubic-bezier(0.4, 0, 0.2, 1) of ZCode's QueuedSummaryContent.
/// gpui has no built-in cubic_bezier, so this follows the CSS semantics: Newton-Raphson solves x(t) = input progress,
/// then the corresponding y(t) is taken
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
