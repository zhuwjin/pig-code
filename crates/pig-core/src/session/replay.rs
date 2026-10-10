use super::*;

impl Session {
    /// Resume replay: emit durable events in order; the UI rebuilds views with the same reduce logic.
    pub fn replay(&mut self, records: &[RolloutRecord], tx: &async_channel::Sender<Event>) {
        let mut in_assistant = false;
        let mut replay_turns = 0usize;
        // Background subagents encountered during replay (deduplicated): after replay ends, a unified finished pass settles their final state
        let mut replay_bg_agents: Vec<String> = vec![];
        for record in records {
            match record {
                RolloutRecord::Meta { .. } => {}
                RolloutRecord::User {
                    text,
                    files,
                    images,
                } => {
                    in_assistant = false;
                    // Clean body text + attachment numbers (same shape as live); the rollout's original text is the body
                    let nums = crate::rollout::image_nums(images);
                    let files = files.clone();
                    self.emit(
                        |session_id, seq| Event::UserMessage {
                            session_id,
                            seq,
                            text: text.clone(),
                            files,
                            image_nums: nums,
                        },
                        tx,
                    );
                }
                RolloutRecord::Reasoning { text } => {
                    if !in_assistant {
                        replay_turns += 1;
                        let turn_id = format!("replay-{replay_turns}");
                        self.emit(
                            |session_id, seq| Event::TurnStarted {
                                session_id,
                                seq,
                                turn_id,
                            },
                            tx,
                        );
                        in_assistant = true;
                    }
                    let item = format!("replay-{replay_turns}-reason");
                    let delta = text.clone();
                    self.emit(
                        |session_id, seq| Event::ReasoningDelta {
                            session_id,
                            seq,
                            item_id: item,
                            delta,
                        },
                        tx,
                    );
                }
                RolloutRecord::Text { text } => {
                    if !in_assistant {
                        replay_turns += 1;
                        let turn_id = format!("replay-{replay_turns}");
                        self.emit(
                            |session_id, seq| Event::TurnStarted {
                                session_id,
                                seq,
                                turn_id,
                            },
                            tx,
                        );
                        in_assistant = true;
                    }
                    let item = format!(
                        "replay-{replay_turns}-text-{}",
                        self.seq.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    let full = text.clone();
                    self.emit(
                        |session_id, seq| Event::TextDone {
                            session_id,
                            seq,
                            item_id: item,
                            full_text: full,
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
                    edit,
                    agent_card,
                    agent_cards,
                } => {
                    if !in_assistant {
                        replay_turns += 1;
                        let turn_id = format!("replay-{replay_turns}");
                        self.emit(
                            |session_id, seq| Event::TurnStarted {
                                session_id,
                                seq,
                                turn_id,
                            },
                            tx,
                        );
                        in_assistant = true;
                    }
                    let item = format!(
                        "replay-{replay_turns}-tool-{}",
                        self.seq.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    let detail = serde_json::from_str::<serde_json::Value>(arguments)
                        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                        .unwrap_or_else(|_| arguments.clone());
                    let (tool, output) = (tool.clone(), output.clone());
                    let is_error = *is_error;
                    // Replay always recomputes the summary from the full arguments, so replays stay consistent as summary logic evolves
                    let summary = tool::summarize(&pig_provider::ToolCall {
                        id: String::new(),
                        name: tool.clone(),
                        arguments: arguments.clone(),
                    });
                    let edit = edit.clone();
                    self.emit(
                        |session_id, seq| Event::ToolCallBegin {
                            session_id,
                            seq,
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
                            |session_id, seq| Event::SubagentCard {
                                session_id,
                                seq,
                                item_id: item.clone(),
                                agent_id: card.agent_id.clone(),
                                profile: card.profile.clone(),
                                description: card.description.clone(),
                                model: card.model.clone(),
                                background: card.background,
                            },
                            tx,
                        );
                    }
                    self.emit(
                        |session_id, seq| Event::ToolCallEnd {
                            session_id,
                            seq,
                            item_id: item,
                            output,
                            is_error,
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
                } => {
                    // Replay restores the compaction point's usage watermark (used_after is the estimate of the
                    // compacted history) and re-emits the event: the UI rebuilds the "context compacted" divider at the same spot in the message stream
                    if let Some(used) = used_after {
                        self.last_total_tokens = Some(*used);
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
                        |session_id, seq| Event::ContextCompacted {
                            session_id,
                            seq,
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
                    api_ms,
                    ttft_ms,
                    api_steps,
                } => {
                    // Replay restore: session totals + historical turns' footer stats (the usage watermark is restored by StepUsage)
                    self.input_total += input;
                    self.cache_read_total += cache_read;
                    let stats = pig_protocol::TurnUsageStats {
                        input: *input,
                        cache_read: *cache_read,
                        output: *output,
                        duration_ms: *duration_ms,
                        api_ms: *api_ms,
                        ttft_ms: *ttft_ms,
                        api_steps: *api_steps,
                    };
                    // duration_ms=0 preserves the "replay wrap-up event" semantics (does not trigger new-turn
                    // logic such as plan-mode pending steps); the footer's real duration comes from stats
                    self.emit(
                        |session_id, seq| Event::TurnComplete {
                            session_id,
                            seq,
                            duration_ms: 0,
                            stats: Some(stats),
                        },
                        tx,
                    );
                }
                RolloutRecord::TurnChanges { files } => {
                    let files = files.clone();
                    self.emit(
                        |session_id, seq| Event::TurnFileChanges {
                            session_id,
                            seq,
                            files,
                        },
                        tx,
                    );
                }
                RolloutRecord::StepUsage { used, .. } => {
                    // Usage watermark = total tokens of a single request; overwritten per record, the last one wins
                    self.last_total_tokens = Some(*used);
                }
            }
        }
        // The panels' current state from SQLite is re-emitted after the JSONL event stream:
        // - todos table -> restore todos and push the snapshot
        // - file_changes table -> re-emit FileChanged per path (the UI upserts to rebuild the change list)
        let (todos_json, db_changes) = {
            let store = self.store.lock().expect("store lock");
            (store.get_todos(&self.id), store.file_changes(&self.id))
        };
        if let Some(json) = todos_json
            && let Ok(items) = serde_json::from_str::<Vec<pig_protocol::TodoItem>>(&json)
        {
            *self.state.todos.lock().expect("todos lock") = items.clone();
            self.emit(
                |session_id, seq| Event::TodoListChanged {
                    session_id,
                    seq,
                    items,
                },
                tx,
            );
        }
        for (path, unified_diff, additions, deletions) in db_changes {
            self.emit(
                |session_id, seq| Event::FileChanged {
                    session_id,
                    seq,
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
                |session_id, seq| Event::TurnComplete {
                    session_id,
                    seq,
                    duration_ms: 0,
                    stats: None,
                },
                tx,
            );
        }
        // Background subagents' agent cards settle to their final state: background tasks do not outlive the
        // process, so after reopening they are always considered terminated
        // (emitted after the closing TurnComplete, when the UI is already non-streaming)
        for agent_id in replay_bg_agents {
            self.emit(
                |session_id, seq| Event::SubagentActivity {
                    session_id,
                    seq,
                    agent_id,
                    item: None,
                    finished: true,
                },
                tx,
            );
        }
    }
}
