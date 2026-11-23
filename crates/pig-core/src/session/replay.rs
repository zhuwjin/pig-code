use super::*;

impl Session {
    /// Resume replay: emit durable events in order; the UI rebuilds views with the same reduce logic.
    pub fn replay(&mut self, records: &[RolloutRecord], tx: &async_channel::Sender<Event>) {
        let mut in_assistant = false;
        let mut replay_turns = 0usize;
        // Background subagents encountered during replay (deduplicated): after replay ends, a unified finished pass settles their final state
        let mut replay_bg_agents: Vec<String> = vec![];
        for (record_ix, record) in records.iter().enumerate() {
            match record {
                RolloutRecord::Meta { .. }
                | RolloutRecord::System { .. }
                | RolloutRecord::PromptSnapshot { .. }
                | RolloutRecord::ToolExecutionStart { .. }
                | RolloutRecord::MailboxMessage { .. }
                | RolloutRecord::AgentRun { .. } => {}
                RolloutRecord::Assistant {
                    text,
                    reasoning,
                    replay,
                    ..
                } => {
                    if !in_assistant {
                        replay_turns += 1;
                        let turn_id = format!("replay-{replay_turns}");
                        self.emit(
                            |session_id| Event::TurnStarted {
                                session_id,
                                turn_id,
                                run: None,
                            },
                            tx,
                        );
                        in_assistant = true;
                    }
                    if let Some(reasoning) = reasoning.as_ref().filter(|text| !text.is_empty()) {
                        let item = format!("replay-{replay_turns}-reason");
                        self.emit(
                            |session_id| Event::ReasoningDelta {
                                session_id,
                                item_id: item,
                                delta: reasoning.clone(),
                            },
                            tx,
                        );
                    }
                    if !text.is_empty() {
                        let item = format!("replay-{replay_turns}-text-{record_ix}");
                        self.emit(
                            |session_id| Event::TextDone {
                                session_id,
                                item_id: item.clone(),
                                full_text: text.clone(),
                            },
                            tx,
                        );
                        // Same durable replay -> same normalized sources as
                        // the live path (including compatible endpoints whose
                        // native blocks are stored but replayed as text only).
                        let sources = replay
                            .as_ref()
                            .map(pig_provider::replay_sources)
                            .unwrap_or_default();
                        if !sources.is_empty() {
                            self.emit(
                                |session_id| Event::Sources {
                                    session_id,
                                    item_id: item,
                                    sources,
                                },
                                tx,
                            );
                        }
                    }
                }
                RolloutRecord::TaskNotificationBatch {
                    notifications,
                    model_text,
                    sent_at_ms,
                } => {
                    in_assistant = false;
                    let notifications = notifications.clone();
                    let model_text = model_text.clone();
                    let sent_at_ms = *sent_at_ms;
                    self.emit(
                        |session_id| Event::TaskNotificationBatch {
                            session_id,
                            notifications,
                            model_text,
                            sent_at_ms,
                        },
                        tx,
                    );
                }
                RolloutRecord::User {
                    text,
                    files,
                    images,
                    sent_at_ms,
                    ..
                } => {
                    in_assistant = false;
                    // Clean body text + attachment numbers (same shape as live); the rollout's original text is the body
                    let nums = crate::rollout::image_nums(images);
                    let files = files.clone();
                    let sent_at_ms = *sent_at_ms;
                    self.emit(
                        |session_id| Event::UserMessage {
                            session_id,
                            text: text.clone(),
                            files,
                            image_nums: nums,
                            sent_at_ms,
                        },
                        tx,
                    );
                }
                RolloutRecord::ToolCall {
                    tool,
                    summary: _,
                    arguments,
                    output,
                    is_error,
                    stopped,
                    edit,
                    agent_card,
                    agent_cards,
                    ..
                } => {
                    if !in_assistant {
                        replay_turns += 1;
                        let turn_id = format!("replay-{replay_turns}");
                        self.emit(
                            |session_id| Event::TurnStarted {
                                session_id,
                                turn_id,
                                run: None,
                            },
                            tx,
                        );
                        in_assistant = true;
                    }
                    let item = format!("replay-{replay_turns}-tool-{record_ix}");
                    let detail = serde_json::from_str::<serde_json::Value>(arguments)
                        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                        .unwrap_or_else(|_| arguments.clone());
                    let (tool, output) = (tool.clone(), output.clone());
                    let is_error = *is_error;
                    let stopped = *stopped;
                    // Replay always recomputes the summary from the full arguments, so replays stay consistent as summary logic evolves
                    let summary = tool::summarize(&pig_provider::ToolCall {
                        id: String::new(),
                        name: tool.clone(),
                        arguments: arguments.clone(),
                    });
                    let edit = edit.clone();
                    self.emit(
                        |session_id| Event::ToolCallBegin {
                            session_id,
                            item_id: item.clone(),
                            tool,
                            input_summary: summary,
                            detail,
                        },
                        tx,
                    );
                    // Agent-card metadata is rebuilt from the record during replay (right after ToolCallBegin,
                    // with the same synthesized replay item_id): agent_card is the single-card slot for Agent,
                    // agent_cards is the AgentSwarm batch cards (each rebuilt). Background agents' agent_ids are
                    // collected, and after replay ends a unified finished pass settles them — background tasks do
                    // not outlive the process, so after reopening they are always considered terminated; otherwise
                    // the replayed agent cards would spin forever
                    for card in agent_card.iter().chain(agent_cards.iter()) {
                        if card.background
                            && !replay_bg_agents.iter().any(|id| id == &card.agent_id)
                        {
                            replay_bg_agents.push(card.agent_id.clone());
                        }
                        let card = card.clone();
                        self.emit(
                            |session_id| Event::SubagentCard {
                                session_id,
                                item_id: item.clone(),
                                agent_id: card.agent_id.clone(),
                                run_id: Some(card.run_id.clone()),
                                profile: card.profile.clone(),
                                description: card.description.clone(),
                                model: card.model.clone(),
                                background: card.background,
                            },
                            tx,
                        );
                    }
                    self.emit(
                        |session_id| Event::ToolCallEnd {
                            session_id,
                            item_id: item,
                            output,
                            is_error,
                            stopped,
                            edit,
                        },
                        tx,
                    );
                }
                RolloutRecord::Compact {
                    note,
                    omitted,
                    automatic,
                    used_after,
                    used_before,
                    summary,
                    ..
                } => {
                    // Replay restores the compaction point's usage watermark (used_after is the estimate of the
                    // compacted history) and re-emits the event: the UI rebuilds the "context compacted" divider at the same spot in the message stream
                    if let Some(used) = used_after {
                        self.run_state.last_total_tokens = Some(*used);
                    }
                    let (note, omitted, automatic, used_before, used_after, summary) = (
                        note.clone(),
                        *omitted,
                        *automatic,
                        *used_before,
                        *used_after,
                        summary.clone(),
                    );
                    self.emit(
                        |session_id| Event::ContextCompacted {
                            session_id,
                            run: None,
                            omitted,
                            note,
                            automatic,
                            used_before,
                            used_after,
                            summary,
                        },
                        tx,
                    );
                }
                RolloutRecord::TurnStats {
                    input,
                    cache_read,
                    output,
                    duration_ms,
                    ended_at_ms,
                    api_ms,
                    ttft_ms,
                    ttft_steps,
                    api_steps,
                    tool_ms,
                    has_usage,
                    finish_reason,
                    turn_id,
                } => {
                    if !in_assistant {
                        replay_turns += 1;
                        let turn_id = format!("replay-{replay_turns}");
                        self.emit(
                            |session_id| Event::TurnStarted {
                                session_id,
                                turn_id,
                                run: None,
                            },
                            tx,
                        );
                        in_assistant = true;
                    }
                    // Replay restore: session totals + historical turns' footer stats (the usage watermark is restored by StepUsage)
                    self.run_state.input_total += input;
                    self.run_state.cache_read_total += cache_read;
                    let stats = has_usage.then_some(pig_protocol::TurnUsageStats {
                        input: *input,
                        cache_read: *cache_read,
                        output: *output,
                        duration_ms: *duration_ms,
                        api_ms: *api_ms,
                        ttft_ms: *ttft_ms,
                        ttft_steps: *ttft_steps,
                        api_steps: *api_steps,
                        tool_ms: *tool_ms,
                        ended_at_ms: *ended_at_ms,
                    });
                    self.run_state.history.push(pig_protocol::PluginTurnStats {
                        id: turn_id.clone(),
                        index: self.run_state.history.len() as u64,
                        duration_ms: *duration_ms,
                        stats: stats.clone(),
                    });
                    // duration_ms=0 preserves the "replay wrap-up event" semantics (does not trigger new-turn
                    // logic such as plan-mode pending steps); the footer's real duration comes from stats
                    self.emit(
                        |session_id| Event::TurnComplete {
                            session_id,
                            duration_ms: 0,
                            run: None,
                            stats,
                            finish_reason: *finish_reason,
                        },
                        tx,
                    );
                }
                RolloutRecord::TurnChanges { files } => {
                    let files = files.clone();
                    self.emit(
                        |session_id| Event::TurnFileChanges { session_id, files },
                        tx,
                    );
                }
                RolloutRecord::StepUsage { used, .. } => {
                    // Usage watermark = total tokens of a single request; overwritten per record, the last one wins
                    self.run_state.last_total_tokens = Some(*used);
                }
                // Stop-hook continuation context: model-visible only, no UI event
                RolloutRecord::HookContext { .. } => {}
                // Recall tombstone: the recalled turn's events were already
                // emitted above, so this arrives in order as the truncation
                // point — the UI drops its last user message and everything
                // after it. in_assistant is left alone: the trailing wrap-up
                // TurnComplete still fires and settles the streaming state
                RolloutRecord::Recall => {
                    self.emit(|session_id| Event::MessageRecalled { session_id }, tx);
                }
            }
        }
        // The panels' current state from SQLite is re-emitted after the JSONL event stream:
        // - todos table -> restore todos and push the snapshot
        // - file_changes table -> re-emit FileChanged per path (the UI upserts to rebuild the change list)
        let (todos_json, db_changes) = {
            let store = self.store.lock().expect("store lock");
            (
                store.get_todos(&self.agent.id),
                store.file_changes(&self.agent.id),
            )
        };
        if let Some(json) = todos_json
            && let Ok(items) = serde_json::from_str::<Vec<pig_protocol::TodoItem>>(&json)
        {
            *self.state.todos.lock().expect("todos lock") = items.clone();
            self.emit(
                |session_id| Event::TodoListChanged { session_id, items },
                tx,
            );
        }
        for (path, unified_diff, additions, deletions) in db_changes {
            self.emit(
                |session_id| Event::FileChanged {
                    session_id,
                    path,
                    unified_diff,
                    additions,
                    deletions,
                },
                tx,
            );
        }
        // Replayed historical turns start with TurnStarted but the records hold no closing event;
        // close them with a duration_ms=0 TurnComplete so the UI exits streaming state
        // (based on this the UI skips the "turn ended - elapsed" footer and the plan-mode pending marker)
        if in_assistant {
            self.emit(
                |session_id| Event::TurnComplete {
                    session_id,
                    duration_ms: 0,
                    run: None,
                    stats: None,
                    finish_reason: None,
                },
                tx,
            );
        }
        // Background subagents' agent cards settle to their final state: background tasks do not outlive the
        // process, so after reopening they are always considered terminated
        // (emitted after the closing TurnComplete, when the UI is already non-streaming)
        for agent_id in replay_bg_agents {
            self.emit(
                |session_id| Event::ConversationStatus {
                    session_id,
                    agent_id,
                    running: false,
                    run_id: None,
                },
                tx,
            );
        }
    }
}
