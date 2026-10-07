use super::*;

impl ThreadView {
    pub fn reduce_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::SessionConfigured { .. } => {}
            // Mode chip and plan chip live in the composer (handled in main.rs/events.rs); the message flow need not respond
            Event::ExecModeChanged { .. } => {}
            Event::PlanModeChanged { .. } => {}
            Event::TurnStarted { turn_id, .. } => {
                self.replay_turn = turn_id.starts_with("replay-");
                self.messages.push(ChatMessage::assistant());
                self.item_index.clear();
                self.set_streaming(true, cx);
            }
            Event::ReasoningDelta { item_id, delta, .. } => {
                if !self.item_index.contains_key(&item_id) {
                    // A new thinking segment starting = the previous thinking segment ending
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
                    // The vertical-scroll state machine only feeds in-progress
                    // segments: replayed/settled segments show no scrolling line,
                    // and feeding them would only start timers for nothing
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
                // Search open: rerun once segment content settles (segments whose
                // revision did not change reuse the cache directly)
                if self.search_open {
                    self.run_search(cx);
                }
            }
            Event::ToolCallBegin {
                item_id,
                tool,
                input_summary,
                detail,
                ..
            } => {
                self.finish_thinking();
                if tool == "ExitPlanMode" {
                    // Plan card: detail = the full args JSON (same path for
                    // live/replay); extract the full plan text
                    let args: serde_json::Value = serde_json::from_str(&detail).unwrap_or_default();
                    let plan = args["plan"].as_str().unwrap_or("").to_string();
                    self.find_or_create(&item_id, || Segment::Plan {
                        state: cx.new(|cx| TextViewState::markdown(&plan, cx)),
                        done: false,
                        approved: false,
                        is_error: false,
                        open: false,
                        expand_anim: ExpandAnim::default(),
                        body_scroll: ScrollHandle::new(),
                    });
                    self.auto_scroll();
                    return cx.notify();
                }
                let six = self.find_or_create(&item_id, || Segment::ToolCall {
                    tool: tool.clone(),
                    summary: input_summary.clone(),
                    output: String::new(),
                    is_error: false,
                    done: false,
                    // The Swarm panel's collapsed state also uses this field
                    // (agent cards have no output expansion area); expanded by default
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
                if let Some(Segment::Plan {
                    done,
                    approved,
                    is_error: err,
                    ..
                }) = self.current_segment(six)
                {
                    // Plan card settles: the decision writes the tri-state (same path for replay)
                    *done = true;
                    *approved = output.contains("Plan approved");
                    *err = is_error;
                    self.auto_scroll();
                    return cx.notify();
                }
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
                    // Settling clears the live progress line (the card returns to its static summary)
                    *live_note = None;
                    // Failed calls expand the output directly, sparing the user an extra click
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
                // Foreground subagent live progress goes to the separate
                // live_note field (rendered under the summary line), leaving
                // summary untouched — the original summary is kept while running
                // and not lost after settling; ignored when the card is absent
                // (replay/out-of-order) or already settled
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
                // Agent card: arrives before ToolCallEnd (on replay it is re-sent
                // right after Begin); one tool card can hold several (one per
                // AgentSwarm subagent, deduplicated by agent_id — the
                // out-of-order/duplicate defense allows patching already-done
                // cards, keeping the running-state fields)
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
                // The agent card's running state is driven by the subagent's real
                // lifecycle: find the most recent matching agent card — finished
                // lands the card's terminal state (collected for both foreground
                // and background: each foreground Swarm subagent completes
                // independently, a single card lands its terminal state on
                // arrival without waiting for the whole batch's tool call to
                // settle); activity-item progress lines are only written to
                // background cards (foreground progress goes through the
                // segment-level SubagentProgress). One tool card can hold
                // several (AgentSwarm), each independent
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
                // No card found (panel-exclusive / late event outside replay): ignored
                if let Some(card) = target {
                    if finished {
                        // Record the finish order (used by the Swarm panel's
                        // completion-first sort; replay has no such event, so it
                        // stays None and falls back to spawn order).
                        // agent_finish_seq and messages are disjoint fields, so
                        // it can be incremented directly while the card borrow lives
                        self.agent_finish_seq += 1;
                        card.finished_seq = Some(self.agent_finish_seq);
                        card.finished = true;
                        card.live_note = None;
                        self.auto_scroll();
                    } else if let Some(item) = item {
                        // Activity item → progress line text: tool → "tool name
                        // summary"; assistant → first line of the body; user
                        // ignored. Uniformly flattened to one line, truncated to
                        // 60 chars
                        let note = match item.role.as_str() {
                            "tool" => {
                                let tool_fallback = rust_i18n::t!("thread.tool_fallback");
                                Some(format!(
                                    "{} {}",
                                    item.tool.as_deref().unwrap_or(tool_fallback.as_ref()),
                                    item.text
                                ))
                            }
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
                // duration_ms=0 is the session replay's closing event: only leave
                // the streaming state, no duration footer; historical turns with
                // stats (TurnStats replay) still get the full stats footer
                let duration = stats
                    .as_ref()
                    .map(|s| s.duration_ms)
                    .filter(|ms| *ms > 0)
                    .unwrap_or(duration_ms);
                if duration_ms > 0 || stats.is_some() {
                    let stats_part = stats.as_ref().map(format_turn_stats).unwrap_or_default();
                    if let Some(message) = self.messages.last_mut() {
                        message.footer = Some(
                            rust_i18n::t!(
                                "thread.turn_end",
                                n = duration as f64 / 1000.0 : {:.1},
                                stats = stats_part
                            )
                            .to_string(),
                        );
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
                // An abort can happen during the compact summary request
                // (ContextCompacted never arrives); clear the flag as a fallback
                self.compacting = false;
                // Abort wrap-up: tool cards that never saw ToolCallEnd are stuck
                // spinning as "running" — settle them all (clear the live
                // progress line; with no output, set "stopped"). Only the last
                // assistant message can have unfinished segments; iterating all
                // messages is just out-of-order defense
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
                                *output = rust_i18n::t!("thread.stopped").to_string();
                            }
                        }
                    }
                }
                if let Some(message) = self.messages.last_mut() {
                    message.footer = Some(rust_i18n::t!("thread.stopped").to_string());
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
                            rows: files.into_iter().map(|edit| TurnFileRow { edit }).collect(),
                            open: false,
                            expand_anim: ExpandAnim::default(),
                        });
                }
                self.auto_scroll();
            }
            Event::FileChanged { .. } | Event::FileReverted { .. } | Event::ContextUsage { .. } => {
            }
            // Data for the right "Subagents" tab (AppView routes it straight to the panel; the message flow does not show it)
            Event::SubagentHistory { .. } => {}
            Event::UserMessage {
                text,
                files,
                image_nums,
                ..
            } => {
                // The event body is already clean text (attachment links are no
                // longer embedded), and queued holds the user's raw input too, so
                // match directly; thumbnails load by image_nums
                let trimmed = text.trim().to_string();
                self.append_user_message(text.clone(), files, image_nums, cx);
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
            // These events are intercepted by AppView::route_event and never reach here
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
            Event::Error { error, .. } => {
                self.finish_thinking();
                self.replay_turn = false;
                self.settle_work_rows(WorkState::Stopped);
                self.set_streaming(false, cx);
                // Core structured error → localized text (detail is the English original as a note)
                self.messages.push(ChatMessage::system(format!(
                    "⚠ {}",
                    crate::errors::core_error_text(&error)
                )));
            }
        }
        cx.notify();
    }

    /// Settle work rows when the turn ends (complete/abort/error): the turn's
    /// thinking blocks and tool cards collapse into one "worked N s ›" row. Marks
    /// backward from the last message up to the User message (skipping System —
    /// a compact divider can sit mid-turn); the is_none guard ensures the
    /// replayed TurnStats top-up TurnComplete (with the real duration) and the
    /// closing TurnComplete (duration_ms=0) do not overwrite each other
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

    /// Settle the still-ticking thinking segments in the current message, fixing
    /// their durations. Turns rebuilt by replay have no real clock (events arrive
    /// in an instant), so they keep None and show "lasted a few seconds".
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

    /// Find a segment by item_id in the current assistant message; append via `create` when not found.
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

    /// Scroll-interval timer for the thinking scrolling line (ZCode
    /// QueuedSummaryContent's promote timer): on expiry roll in the next queued
    /// line, re-arming while the queue is non-empty. Stale timers with
    /// mismatched generations (segment already reset or scrolled again) are
    /// discarded. The playing thinking segment is always in the last message, so
    /// current_segment suffices; not finding it means the segment settled or
    /// belongs to another turn, and the timer chain ends naturally.
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
