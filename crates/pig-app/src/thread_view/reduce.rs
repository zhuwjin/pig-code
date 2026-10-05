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
                    ticker: TickerRoll::default(),
                    expand_anim: ExpandAnim::default(),
                });
                let mut roll_promoted = false;
                let replay = self.replay_turn;
                if let Some(Segment::Thinking {
                    text,
                    duration,
                    ticker,
                    ..
                }) = self.current_segment(six)
                {
                    text.push_str(&delta);
                    // 纵滚状态机只喂进行中的段：回放/已收尾的段不显示滚动行，
                    // 喂了也只会白起定时器
                    if duration.is_none()
                        && !replay
                        && let Some(target) = ticker_target_line(text)
                    {
                        roll_promoted = ticker.feed(target);
                    }
                }
                if roll_promoted {
                    self.spawn_ticker_timer(six, cx);
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
                    // Swarm 面板的折叠态也用这个字段（代理卡无输出展开区），默认展开
                    expanded: tool == "AgentSwarm",
                    edit: None,
                    live_note: None,
                    agent_cards: vec![],
                    read_ui: None,
                    bash_ui: None,
                    expand_anim: ExpandAnim::default(),
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
                    agent_cards: vec![],
                    read_ui: None,
                    bash_ui: None,
                    expand_anim: ExpandAnim::default(),
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
                // 代理卡：先于 ToolCallEnd 到达（回放时紧挨 Begin 重发）；同一工具卡
                // 可有多张（AgentSwarm 每个子代理一张，同 agent_id 去重更新——
                // 乱序/重复防御允许补写已 done 的卡，运行态字段保留）
                if let Some(&six) = self.item_index.get(&item_id)
                    && let Some(Segment::ToolCall { agent_cards, .. }) = self.current_segment(six)
                {
                    match agent_cards.iter_mut().find(|c| c.agent_id == agent_id) {
                        Some(existing) => {
                            existing.profile = profile;
                            existing.description = description;
                            existing.model = model;
                            existing.background = background;
                        }
                        None => agent_cards.push(AgentCardMeta {
                            agent_id,
                            profile,
                            description,
                            model,
                            background,
                            finished: false,
                            finished_seq: None,
                            live_note: None,
                        }),
                    }
                }
            }
            Event::SubagentActivity {
                agent_id,
                item,
                finished,
                ..
            } => {
                // 代理卡的运行态由子代理真实生命周期驱动：找最近一张匹配的
                // 代理卡——finished 落卡终态（前台/后台都收：前台 Swarm 每个
                // 子代理独立完成，单卡到点即落终态，不等整批工具调用收尾）；
                // 活动项进度行只写后台卡（前台进度走段级 SubagentProgress）。
                // 同一工具卡可有多张（AgentSwarm），逐卡独立
                let target = self
                    .messages
                    .iter_mut()
                    .rev()
                    .flat_map(|m| m.segments.iter_mut().rev())
                    .find_map(|s| match s {
                        Segment::ToolCall { agent_cards, .. } => agent_cards
                            .iter_mut()
                            .rev()
                            .find(|c| c.agent_id == agent_id && (finished || c.background)),
                        _ => None,
                    });
                // 找不到卡（面板独占/回放外的迟到事件）忽略
                if let Some(card) = target {
                    if finished {
                        // 记录完成次序（Swarm 面板完成优先排序用；回放无此事件，
                        // 保持 None 落回发起序）。agent_finish_seq 与 messages 是
                        // 不相交字段，card 借用存活期间可直接自增
                        self.agent_finish_seq += 1;
                        card.finished_seq = Some(self.agent_finish_seq);
                        card.finished = true;
                        card.live_note = None;
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
                            card.live_note = Some(note);
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
                        if let Segment::Thinking { open, pinned, .. } = segment
                            && !*pinned
                        {
                            *open = false;
                        }
                    }
                }
                // duration_ms=0 是会话回放的收尾事件：只退出流式状态，不写用时脚注；
                // stats 有值的历史回合（TurnStats 回放）仍写完整统计脚注
                let duration = stats
                    .as_ref()
                    .map(|s| s.duration_ms)
                    .filter(|ms| *ms > 0)
                    .unwrap_or(duration_ms);
                if duration_ms > 0 || stats.is_some() {
                    let stats_part = stats.as_ref().map(format_turn_stats).unwrap_or_default();
                    if let Some(message) = self.messages.last_mut() {
                        message.footer = Some(format!(
                            "回合结束 · 用时 {:.1}s{stats_part}",
                            duration as f64 / 1000.0
                        ));
                    }
                }
                self.settle_work_rows(WorkState::Completed {
                    duration: (duration > 0).then_some(std::time::Duration::from_millis(duration)),
                });
                self.set_streaming(false, cx);
            }
            Event::TurnAborted { .. } => {
                self.finish_thinking();
                self.replay_turn = false;
                // 中止可能发生在压缩摘要请求期间（收不到 ContextCompacted），兜底清标记
                self.compacting = false;
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
                self.settle_work_rows(WorkState::Stopped);
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
                                    expand_anim: ExpandAnim::default(),
                                })
                                .collect(),
                            open: false,
                            expand_anim: ExpandAnim::default(),
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
            | Event::CompactStarted { .. }
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
            | Event::McpServerList { .. }
            | Event::ModelInfo { .. }
            | Event::WorkspaceList { .. } => {}
            Event::Error { message, .. } => {
                self.finish_thinking();
                self.replay_turn = false;
                self.settle_work_rows(WorkState::Stopped);
                self.set_streaming(false, cx);
                self.messages
                    .push(ChatMessage::system(format!("⚠ {message}")));
            }
        }
        cx.notify();
    }

    /// 回合结束（完成/中断/错误）时落定工作行：该轮的思考块与工具卡折叠成
    /// 「已工作 N 秒 ›」一行。从消息末尾向前标到 User 消息为止（跳过 System——
    /// 压缩分隔条可能插在回合中间）；is_none 守卫保证回放里 TurnStats 补发的
    /// TurnComplete（带真实耗时）与收尾 TurnComplete（duration_ms=0）不互相覆盖
    fn settle_work_rows(&mut self, state: WorkState) {
        for message in self.messages.iter_mut().rev() {
            if message.role == Role::User {
                break;
            }
            if message.role != Role::Assistant || message.work_state.is_some() {
                continue;
            }
            let has_work = message
                .segments
                .iter()
                .any(|s| matches!(s, Segment::Thinking { .. } | Segment::ToolCall { .. }));
            if has_work {
                message.work_state = Some(state);
            }
        }
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
    pub(crate) fn find_or_create(
        &mut self,
        item_id: &str,
        create: impl FnOnce() -> Segment,
    ) -> usize {
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

    /// 思考滚动行的滚动间隔定时器（ZCode QueuedSummaryContent 的 promote 定时器）：
    /// 到点滚入下一条排队行，还有排队则续期。代次不符（段已 reset/又滚过）的
    /// 旧定时器直接作废。在播思考段恒在最后一条消息里，current_segment 够用；
    /// 找不到说明段已收尾/不属于当前轮，定时器链自然终止。
    fn spawn_ticker_timer(&mut self, six: usize, cx: &mut Context<Self>) {
        let Some(Segment::Thinking { ticker, .. }) = self.current_segment(six) else {
            return;
        };
        let generation = ticker.generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TICKER_ROLL_INTERVAL).await;
            this.update(cx, |this, cx| {
                let again =
                    if let Some(Segment::Thinking { ticker, .. }) = this.current_segment(six) {
                        ticker.fire(generation, std::time::Instant::now())
                    } else {
                        false
                    };
                if again {
                    this.spawn_ticker_timer(six, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
