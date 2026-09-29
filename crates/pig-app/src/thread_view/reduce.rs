use super::*;

impl ThreadView {
    pub fn reduce_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::SessionConfigured { .. } => {}
            // 模式 chip 在 composer（main.rs 处理）；消息流无需响应
            Event::ExecModeChanged { .. } => {}
            Event::TurnStarted { turn_id, .. } => {
                self.plan_pending = false;
                self.replay_turn = turn_id.starts_with("replay-");
                self.messages.push(ChatMessage::assistant());
                self.item_index.clear();
                self.set_streaming(true, cx);
            }
            Event::ReasoningDelta { item_id, delta, .. } => {
                if !self.item_index.contains_key(&item_id) {
                    // 新一段思考开始 = 上一段思考结束
                    self.finish_thinking();
                }
                let six = self.find_or_create(&item_id, || Segment::Thinking {
                    text: String::new(),
                    open: false,
                    pinned: false,
                    started: std::time::Instant::now(),
                    duration: None,
                    body_scroll: ScrollHandle::new(),
                    ticker_scroll: ScrollHandle::new(),
                });
                if let Some(Segment::Thinking { text, .. }) = self.current_segment(six) {
                    text.push_str(&delta);
                }
                self.auto_scroll();
            }
            Event::TextDelta { item_id, delta, .. } => {
                self.finish_thinking();
                let state_holder = cx.new(|cx| TextViewState::markdown("", cx));
                let six = self.find_or_create(&item_id, || Segment::Markdown {
                    state: state_holder,
                    text: String::new(),
                });
                if let Some(Segment::Markdown { state, text }) = self.current_segment(six) {
                    text.push_str(&delta);
                    let delta = delta.clone();
                    state.update(cx, |state, cx| state.push_str(&delta, cx));
                }
                self.auto_scroll();
            }
            Event::TextDone {
                item_id, full_text, ..
            } => {
                self.finish_thinking();
                let state_holder = cx.new(|cx| TextViewState::markdown("", cx));
                let six = self.find_or_create(&item_id, || Segment::Markdown {
                    state: state_holder,
                    text: String::new(),
                });
                if let Some(Segment::Markdown { state, text }) = self.current_segment(six) {
                    *text = full_text.clone();
                    state.update(cx, |state, cx| state.set_text(&full_text, cx));
                }
                // 搜索开着：段内容落定后重跑（revision 没变的段会直接复用缓存）
                if self.search_open {
                    self.run_search(cx);
                }
            }
            Event::ToolCallBegin {
                item_id,
                tool,
                input_summary,
                ..
            } => {
                self.finish_thinking();
                let six = self.find_or_create(&item_id, || Segment::ToolCall {
                    tool: tool.clone(),
                    summary: input_summary.clone(),
                    output: String::new(),
                    is_error: false,
                    done: false,
                    expanded: false,
                    edit: None,
                    live_note: None,
                    agent_card: None,
                    agent_finished: false,
                    body_scroll: ScrollHandle::new(),
                });
                if let Some(Segment::ToolCall { tool, summary, .. }) = self.current_segment(six) {
                    *tool = tool.clone();
                    *summary = input_summary;
                }
                self.auto_scroll();
            }
            Event::ToolCallEnd {
                item_id,
                output,
                is_error,
                edit,
                ..
            } => {
                let six = self.find_or_create(&item_id, || Segment::ToolCall {
                    tool: String::new(),
                    summary: String::new(),
                    output: String::new(),
                    is_error: false,
                    done: false,
                    expanded: false,
                    edit: None,
                    live_note: None,
                    agent_card: None,
                    agent_finished: false,
                    body_scroll: ScrollHandle::new(),
                });
                if let Some(Segment::ToolCall {
                    output: out,
                    is_error: err,
                    done,
                    expanded,
                    edit: slot,
                    live_note,
                    ..
                }) = self.current_segment(six)
                {
                    *out = output;
                    *err = is_error;
                    *done = true;
                    *slot = edit;
                    // 收尾清掉实时进度行（卡片回到静态摘要）
                    *live_note = None;
                    // 失败的调用直接展开输出，省去用户多点一下
                    if is_error {
                        *expanded = true;
                    }
                }
                self.auto_scroll();
            }
            Event::ApprovalRequested { request_id, .. } => {
                self.finish_thinking();
                if self
                    .messages
                    .last()
                    .is_none_or(|m| m.role != Role::Assistant)
                {
                    self.messages.push(ChatMessage::assistant());
                }
                self.messages
                    .last_mut()
                    .expect("assistant message")
                    .segments
                    .push(Segment::Approval {
                        request_id,
                        decision: None,
                    });
                self.auto_scroll();
            }
            Event::SubagentProgress { item_id, note, .. } => {
                // 前台子代理的实时进度写独立字段 live_note（渲染在摘要行下方），
                // 不动 summary——原摘要在运行中保留，收尾后也不丢；
                // 卡片不存在（回放/乱序）或已收尾时忽略
                if let Some(&six) = self.item_index.get(&item_id)
                    && let Some(Segment::ToolCall {
                        live_note,
                        done: false,
                        ..
                    }) = self.current_segment(six)
                {
                    *live_note = Some(note);
                }
            }
            Event::SubagentCard {
                item_id,
                agent_id,
                profile,
                description,
                model,
                background,
                ..
            } => {
                // 代理卡元信息：先于 ToolCallEnd 到达（回放时紧挨 Begin 重发）；
                // 乱序防御允许补写已 done 的卡
                if let Some(&six) = self.item_index.get(&item_id)
                    && let Some(Segment::ToolCall { agent_card, .. }) = self.current_segment(six)
                {
                    *agent_card = Some(AgentCardMeta {
                        agent_id,
                        profile,
                        description,
                        model,
                        background,
                    });
                }
            }
            Event::SubagentActivity {
                agent_id,
                item,
                finished,
                ..
            } => {
                // 后台代理卡的运行态由子代理真实生命周期驱动：找最后一张匹配的
                // 后台代理卡（item 更新进度行；finished 落终态）。前台卡的进度走
                // SubagentProgress、运行态跟 done 走，这里一律不动它
                let target = self
                    .messages
                    .iter_mut()
                    .rev()
                    .flat_map(|m| m.segments.iter_mut().rev())
                    .find_map(|s| match s {
                        Segment::ToolCall {
                            agent_card: Some(card),
                            live_note,
                            agent_finished,
                            ..
                        } if card.agent_id == agent_id && card.background => {
                            Some((live_note, agent_finished))
                        }
                        _ => None,
                    });
                // 找不到卡（面板独占/回放外的迟到事件）忽略
                if let Some((live_note, finished_slot)) = target {
                    if finished {
                        *finished_slot = true;
                        *live_note = None;
                        self.auto_scroll();
                    } else if let Some(item) = item {
                        // 活动项 → 进度行文本：tool → "工具名 摘要"；assistant → 正文首行；
                        // user 忽略。统一压单行截 60 字符
                        let note = match item.role.as_str() {
                            "tool" => Some(format!(
                                "{} {}",
                                item.tool.as_deref().unwrap_or("工具"),
                                item.text
                            )),
                            "assistant" => item.text.lines().next().map(str::to_string),
                            _ => None,
                        }
                        .map(|n| n.split_whitespace().collect::<Vec<_>>().join(" "))
                        .filter(|n| !n.is_empty());
                        if let Some(note) = note {
                            let note: String = note.chars().take(60).collect();
                            *live_note = Some(note);
                            self.auto_scroll();
                        }
                    }
                }
            }
            Event::TurnComplete {
                duration_ms, stats, ..
            } => {
                self.finish_thinking();
                self.replay_turn = false;
                if let Some(message) = self.messages.last_mut() {
                    for segment in &mut message.segments {
                        if let Segment::Thinking { open, pinned, .. } = segment {
                            if !*pinned {
                                *open = false;
                            }
                        }
                    }
                }
                // duration_ms=0 是会话回放的收尾事件：只退出流式状态，不写用时脚注；
                // stats 有值的历史回合（TurnStats 回放）仍写完整统计脚注
                if duration_ms > 0 || stats.is_some() {
                    let stats_part = stats.as_ref().map(format_turn_stats).unwrap_or_default();
                    let duration = stats
                        .as_ref()
                        .map(|s| s.duration_ms)
                        .filter(|ms| *ms > 0)
                        .unwrap_or(duration_ms);
                    if let Some(message) = self.messages.last_mut() {
                        message.footer = Some(format!(
                            "回合结束 · 用时 {:.1}s{stats_part}",
                            duration as f64 / 1000.0
                        ));
                    }
                }
                self.set_streaming(false, cx);
            }
            Event::TurnAborted { .. } => {
                self.finish_thinking();
                self.replay_turn = false;
                // 中止收尾：没等到 ToolCallEnd 的工具卡停在「执行中」转圈——
                // 全部落定（清实时进度行，无输出置「已停止」）。只有最后一条
                // assistant 消息可能有未完成段，遍历全部消息只是防御乱序
                for message in &mut self.messages {
                    for segment in &mut message.segments {
                        if let Segment::ToolCall {
                            output,
                            done,
                            live_note,
                            ..
                        } = segment
                            && !*done
                        {
                            *done = true;
                            *live_note = None;
                            if output.is_empty() {
                                *output = "已停止".to_string();
                            }
                        }
                    }
                }
                if let Some(message) = self.messages.last_mut() {
                    message.footer = Some("已停止".to_string());
                }
                self.set_streaming(false, cx);
            }
            Event::TurnFileChanges { files, .. } => {
                self.finish_thinking();
                if !files.is_empty() {
                    if self
                        .messages
                        .last()
                        .is_none_or(|m| m.role != Role::Assistant)
                    {
                        self.messages.push(ChatMessage::assistant());
                    }
                    self.messages
                        .last_mut()
                        .expect("assistant message")
                        .segments
                        .push(Segment::TurnChanges {
                            rows: files
                                .into_iter()
                                .map(|edit| TurnFileRow {
                                    edit,
                                    expanded: false,
                                    scroll: ScrollHandle::new(),
                                })
                                .collect(),
                            open: false,
                        });
                }
                self.auto_scroll();
            }
            Event::FileChanged { .. } | Event::FileReverted { .. } | Event::ContextUsage { .. } => {
            }
            // 右侧「子代理」tab 的数据（AppView 直接路由给面板，消息流不展示）
            Event::SubagentHistory { .. } => {}
            Event::UserMessage { text, files, .. } => {
                // 链接尾巴不进队列匹配（queued 里是用户输入原文）
                let (body, _) = split_image_links(text.as_str());
                let trimmed = body.trim().to_string();
                self.append_user_message(text.clone(), files, cx);
                if let Some(pos) = self
                    .queued
                    .iter()
                    .position(|t| trimmed == *t || trimmed.starts_with(t.as_str()))
                {
                    self.queued.remove(pos);
                }
            }
            Event::MessageQueued { text, .. } => {
                self.queued.push(text);
            }
            // 这些事件由 AppView::route_event 拦截处理，不到这里
            Event::SessionList { .. }
            | Event::SessionTitleChanged { .. }
            | Event::FileSearchResults { .. }
            | Event::ContextCompacted { .. }
            | Event::TodoListChanged { .. }
            | Event::TaskListChanged { .. }
            | Event::QuestionRequested { .. }
            | Event::GitInfo { .. }
            | Event::BranchChanged { .. }
            | Event::GitStatus { .. }
            | Event::GitDiff { .. }
            | Event::ConfigSnapshot { .. }
            | Event::TestResult { .. }
            | Event::ModelInfo { .. }
            | Event::WorkspaceList { .. } => {}
            Event::Error { message, .. } => {
                self.finish_thinking();
                self.replay_turn = false;
                self.set_streaming(false, cx);
                self.messages
                    .push(ChatMessage::system(format!("⚠ {message}")));
            }
        }
        cx.notify();
    }


    /// 收尾当前消息里还在计时的思考段，定格用时。
    /// 回放重建的回合没有真实时钟（事件在一瞬间到达），保持 None 显示「持续了几秒」。
    pub(crate) fn finish_thinking(&mut self) {
        if self.replay_turn {
            return;
        }
        let Some(message) = self.messages.last_mut() else {
            return;
        };
        for segment in &mut message.segments {
            if let Segment::Thinking {
                started, duration, ..
            } = segment
            {
                duration.get_or_insert_with(|| started.elapsed());
            }
        }
    }


    /// 在当前助手消息里按 item_id 找 segment，找不到则用 `create` 追加。
    pub(crate) fn find_or_create(&mut self, item_id: &str, create: impl FnOnce() -> Segment) -> usize {
        if let Some(&six) = self.item_index.get(item_id) {
            return six;
        }
        if self
            .messages
            .last()
            .is_none_or(|m| m.role != Role::Assistant)
        {
            self.messages.push(ChatMessage::assistant());
        }
        let message = self.messages.last_mut().expect("assistant message");
        message.segments.push(create());
        let six = message.segments.len() - 1;
        self.item_index.insert(item_id.to_string(), six);
        six
    }


    pub(crate) fn current_segment(&mut self, six: usize) -> Option<&mut Segment> {
        self.messages.last_mut()?.segments.get_mut(six)
    }


}
