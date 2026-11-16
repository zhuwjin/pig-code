//! Step policy for the shared agent engine.
//!
//! `engine::run_step` owns the sequence (preflight, attempt, commit, tools).
//! These `Session` methods own the policy for both kinds of agent: a root
//! session persists through the rollout and emits root events; a child Session
//! persists through its JSONL and emits shared conversation items into the
//! parent conversation.

use super::*;

use pig_provider::ProviderEvent;

use super::engine::{AttemptAction, AttemptOutcome, CommitAction, ProviderUsage, StepRuntime};

impl Session {
    pub(crate) async fn prepare_step(
        &mut self,
        rt: &mut StepRuntime,
        step: usize,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) {
        if self.is_subagent() {
            self.prepare_child_step(rt, step, tx, cancel).await;
            return;
        }
        let (turn_id, config, _, _, _) = rt.root_parts();
        let turn_id = turn_id.to_string();
        let config = config.clone();
        self.ensure_mcp_connected().await;
        self.select_plugin_tools(&config, tx).await;
        self.agent.config = Some(config.clone());
        rt.schemas = self.root_schemas();
        self.agent.schemas = rt.schemas.clone();
        rt.tools_tokens = super::estimate::estimate_schemas_tokens(&rt.schemas);
        rt.media = tool::MediaCapabilities::new(config.input_image, config.input_pdf);
        rt.text_item = format!("{turn_id}-text-{step}");
        rt.reasoning_item = format!("{turn_id}-reason-{step}");
    }

    pub(crate) async fn preflight_step(
        &mut self,
        rt: &mut StepRuntime,
        _step: usize,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> bool {
        if self.is_subagent() {
            return self.preflight_child_step(rt, _step, tx, cancel).await;
        }
        let (_, config, _, preserve_latest, pause_pin) = rt.root_parts();
        let config = config.clone();
        let projected = self.projected_context_tokens(&config, rt.tools_tokens);
        if projected > compact_trigger(&config) && !rt.recovery_attempted {
            match self
                .run_compact_with_current_tools(
                    Some(&config),
                    CompactTrigger::Watermark,
                    None,
                    preserve_latest,
                    pause_pin,
                    tx,
                    cancel,
                )
                .await
            {
                CompactOutcome::Cancelled => return true,
                CompactOutcome::Completed | CompactOutcome::Skipped | CompactOutcome::Failed => {}
            }
        }
        if rt.step_started.is_none() {
            rt.step_started = Some(Instant::now());
        }
        false
    }

    pub(super) fn root_prompt_cache_key(&self) -> String {
        pig_provider::session_prompt_cache_key(&self.agent.id)
    }

    pub(crate) async fn begin_step_attempt(
        &mut self,
        rt: &mut StepRuntime,
        _step: usize,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<ProviderEvent>,
        tokio::task::JoinHandle<()>,
    ) {
        if self.is_subagent() {
            return self.begin_child_attempt(rt, cancel);
        }
        let (_, config, control, _, _) = rt.root_parts();
        let config = config.clone();
        let control = control.clone();
        let breakdown =
            super::estimate::context_breakdown_with_tools(&self.agent.history, rt.tools_tokens);
        self.emit(
            |session_id| Event::ContextBreakdownChanged {
                session_id,
                breakdown,
            },
            tx,
        );
        let mut request_config = config;
        if pig_provider::prompt_cache_key_supported(&request_config.base_url) {
            request_config.prompt_cache_key = Some(self.root_prompt_cache_key());
        }
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(pig_provider::stream_chat(
            request_config,
            self.agent.history.clone(),
            rt.schemas.clone(),
            event_tx,
            control,
        ));
        (event_rx, task)
    }

    pub(crate) async fn record_step_attempt(
        &mut self,
        rt: &mut StepRuntime,
        attempt: &mut AttemptOutcome,
        api_tracker: crate::api_usage::ApiCallTracker,
        api_started: Instant,
        api_started_ms: u64,
        step: usize,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> AttemptAction {
        if self.is_subagent() {
            return self
                .record_child_attempt(
                    rt,
                    attempt,
                    api_tracker,
                    api_started,
                    api_started_ms,
                    step,
                    tx,
                    cancel,
                )
                .await;
        }
        let (turn_id, config, _, preserve_latest, pause_pin) = rt.root_parts();
        let turn_id = turn_id.to_string();
        let config = config.clone();
        // Usage side effects: turn/session totals, the durable step record and
        // the live context figure.
        let mut step_usage = crate::model_io::ModelIoUsage::default();
        if let Some(usage) = attempt.usage {
            let ProviderUsage {
                input,
                cache_read,
                output,
                used,
                total,
                reasoning_output,
            } = usage;
            self.turn_input += input;
            self.turn_cache_read += cache_read;
            self.turn_output += output;
            self.turn_reasoning_output += reasoning_output;
            self.input_total += input;
            self.cache_read_total += cache_read;
            self.last_total_tokens = Some(used);
            step_usage = crate::model_io::ModelIoUsage {
                input,
                cache_read,
                output,
                used,
                total,
                reasoning_output,
            };
            self.record(&RolloutRecord::StepUsage {
                input,
                cache_read,
                output,
                used,
                turn_id: self.usage_turn_id.clone(),
            });
            let (input_total, cache_read_total) = (self.input_total, self.cache_read_total);
            self.emit(
                |session_id| Event::ContextUsage {
                    session_id,
                    used,
                    total,
                    cache_read_total,
                    input_total,
                },
                tx,
            );
        }
        let first_token_at = attempt.first_token_at;
        if let Some(at) = first_token_at {
            rt.step_first_token_at.get_or_insert(at);
        }
        let forwarded_content = attempt.forwarded_content;
        let failure_present = attempt.failure.is_some();
        let step_error = attempt.failure.as_ref().map(pig_provider::core_error_en);
        let api_elapsed = api_started.elapsed();
        self.turn_api_steps += 1;
        let request_ttft_ms = first_token_at
            .map(|at| at.duration_since(api_started).as_millis() as u64)
            .unwrap_or(0)
            .min(api_elapsed.as_millis() as u64);
        let incomplete = attempt.finish_reason.is_incomplete();
        let io_finish = if failure_present {
            "error"
        } else if cancel.is_cancelled() {
            "cancelled"
        } else {
            attempt.finish_reason.as_str()
        };
        let mut api_record = crate::api_usage::ApiUsageRecord::new(
            &self.agent.id,
            "main",
            &config,
            api_started_ms,
            api_elapsed.as_millis() as u64,
        );
        api_record.turn_id = Some(turn_id.clone());
        api_record.step = Some(step as u32);
        api_record.ttft_ms = first_token_at.is_some().then_some(request_ttft_ms);
        api_record.finish = io_finish.to_string();
        api_record.error = step_error.clone();
        api_tracker.apply_to(&mut api_record);
        self.store
            .lock()
            .expect("store lock")
            .record_api_usage(&api_record);

        if !failure_present && !cancel.is_cancelled() && !incomplete {
            rt.step_item_ids =
                self.claim_tool_item_ids(&turn_id, &attempt.tool_calls, &mut rt.live_tool_inputs);
        }

        // Model-io trace input projection: history is unchanged since the
        // request (the assistant is committed in the next hook), so the
        // snapshot is the one the provider saw.
        let io_full_retention = self
            .config_snapshot()
            .is_some_and(|c| c.model_io_full_retention);
        let io_input_full = crate::model_io::project_input(
            &self.agent.history,
            (!io_full_retention).then_some(crate::model_io::TOOL_RESULT_MAX_CHARS),
        );
        let io_offset = crate::model_io::common_prefix_len(&io_input_full, &self.io_last_input);
        let io_input: Vec<_> = io_input_full[io_offset..].to_vec();
        let mut io_record = crate::model_io::ModelIoRecord {
            ts_ms: crate::model_io::now_ms(),
            turn: format!("{turn_id}-s{step}"),
            source: "main".into(),
            provider: config.provider_name.clone(),
            model: config.model.clone(),
            duration_ms: api_elapsed.as_millis() as u64,
            ttft_ms: request_ttft_ms,
            usage: step_usage,
            finish: io_finish.into(),
            error: step_error,
            reasoning: attempt.reasoning.clone(),
            text: attempt.text.clone(),
            tool_calls: crate::model_io::project_tool_calls(&attempt.tool_calls),
            input_offset: io_offset,
            input: io_input,
            file_reset: false,
        };
        if let Err(e) = crate::model_io::append(
            &self.data_dir.join("sessions"),
            &self.agent.id,
            &mut io_record,
            &io_input_full,
            crate::model_io::MAX_SESSION_BYTES,
            io_full_retention,
        ) {
            tracing::warn!("Failed to write model io trace (ignored): {e}");
        }
        self.io_last_input = io_input_full;

        for (_, (id, item_id)) in rt.live_tool_inputs.drain() {
            let discard = failure_present
                || incomplete
                || cancel.is_cancelled()
                || !attempt.tool_calls.iter().any(|call| call.id == id);
            if discard {
                self.emit(
                    |session_id| Event::ToolInputDiscard {
                        session_id,
                        item_id,
                    },
                    tx,
                );
            }
        }
        if incomplete {
            attempt.tool_calls.clear();
            if let Some(replay) = attempt.replay.as_mut() {
                replay.remove_tool_calls();
            }
        }

        if let Some(error) = attempt.failure.as_ref() {
            let overflow = matches!(error, CoreError::ContextWindowExceeded { .. });
            if overflow && !forwarded_content && !self.overflow_recovered {
                tracing::warn!(
                    step,
                    "context window exceeded before any content was forwarded; compacting and retrying this step once"
                );
                let used_before =
                    super::estimate::estimate_history_tokens(&self.agent.history) + rt.tools_tokens;
                match self
                    .run_compact_with_current_tools(
                        Some(&config),
                        CompactTrigger::Overflow,
                        None,
                        preserve_latest,
                        pause_pin,
                        tx,
                        cancel,
                    )
                    .await
                {
                    CompactOutcome::Completed => {
                        let used_after = self.last_total_tokens.unwrap_or(u64::MAX);
                        if used_after < used_before {
                            self.overflow_recovered = true;
                            rt.recovery_attempted = true;
                            return AttemptAction::Retry;
                        }
                        tracing::warn!(
                            used_before,
                            used_after,
                            "overflow compaction did not shrink the context; surfacing the error"
                        );
                    }
                    CompactOutcome::Cancelled => {
                        self.charge_step_time(
                            rt.step_started.unwrap_or_else(Instant::now),
                            rt.step_first_token_at,
                        );
                        return AttemptAction::Terminal(engine::StepOutcome::Cancelled);
                    }
                    CompactOutcome::Skipped | CompactOutcome::Failed => {
                        tracing::warn!(
                            "context overflow cannot be compacted (no compactable prefix); surfacing the error"
                        );
                    }
                }
            }
            self.emit(
                |session_id| Event::Error {
                    session_id: Some(session_id),
                    error: error.clone(),
                    fatal: true,
                },
                tx,
            );
            self.charge_step_time(
                rt.step_started.unwrap_or_else(Instant::now),
                rt.step_first_token_at,
            );
            return AttemptAction::Terminal(engine::StepOutcome::Failed);
        }
        if cancel.is_cancelled() {
            self.emit_turn_aborted(tx);
            self.charge_step_time(
                rt.step_started.unwrap_or_else(Instant::now),
                rt.step_first_token_at,
            );
            return AttemptAction::Terminal(engine::StepOutcome::Cancelled);
        }
        AttemptAction::Continue
    }

    pub(crate) async fn commit_step_assistant(
        &mut self,
        rt: &mut StepRuntime,
        attempt: AttemptOutcome,
        step: usize,
        tx: &async_channel::Sender<Event>,
    ) -> CommitAction {
        if self.is_subagent() {
            return self.commit_child_assistant(rt, attempt, step, tx).await;
        }
        let (_, config, _, _, _) = rt.root_parts();
        let config = config.clone();
        let provider_usage = attempt.usage;
        let finish_reason = attempt.finish_reason;
        let incomplete = finish_reason.is_incomplete();
        let text = attempt.text;
        let reasoning = Some(attempt.reasoning).filter(|reasoning| !reasoning.is_empty());
        let replay = attempt.replay;
        let tool_calls = attempt.tool_calls;

        self.charge_step_time(
            rt.step_started.unwrap_or_else(Instant::now),
            rt.step_first_token_at,
        );

        // The durable assistant record is written before the TextDone boundary:
        // a crash after TextDone can never lose a reply the UI already showed.
        self.record(&RolloutRecord::Assistant {
            text: text.clone(),
            reasoning: reasoning.clone(),
            replay: replay.clone(),
            tool_calls: tool_calls.iter().map(ToolCall::to_wire).collect(),
            tool_execution_ids: rt.step_item_ids.clone(),
            ts_ms: 0,
            reasoning_ms: None,
        });
        if !text.is_empty() {
            self.emit(
                |session_id| Event::TextDone {
                    session_id,
                    item_id: rt.text_item.clone(),
                    full_text: text.clone(),
                },
                tx,
            );
        }
        let sources = replay
            .as_ref()
            .map(pig_provider::replay_sources)
            .unwrap_or_default();
        if !text.is_empty() && !sources.is_empty() {
            self.emit(
                |session_id| Event::Sources {
                    session_id,
                    item_id: rt.text_item.clone(),
                    sources,
                },
                tx,
            );
        }
        self.agent.history.push(ChatMsg::assistant_with_replay(
            text,
            tool_calls.clone(),
            reasoning,
            replay,
        ));
        let attempt_usage = provider_usage
            .map(|usage| usage.used)
            .filter(|used| *used > 0);
        self.usage_anchor = attempt_usage.map(|used| UsageAnchor {
            model_key: model_key(&config),
            history_len: self.agent.history.len(),
            tools_tokens: rt.tools_tokens,
            used,
            system_fingerprint: system_fingerprint(&self.agent.history),
        });
        let breakdown =
            super::estimate::context_breakdown_with_tools(&self.agent.history, rt.tools_tokens);
        self.emit(
            |session_id| Event::ContextBreakdownChanged {
                session_id,
                breakdown,
            },
            tx,
        );

        let paused = finish_reason == pig_protocol::FinishReason::Pause;
        if tool_calls.is_empty() {
            if paused {
                return CommitAction::Paused;
            }
            return CommitAction::TextOnly {
                finish_reason: incomplete.then_some(finish_reason),
            };
        }
        debug_assert_eq!(
            rt.step_item_ids.len(),
            tool_calls.len(),
            "every final call owns one internal item id"
        );
        rt.paused = paused;
        CommitAction::Tools(tool_calls)
    }

    pub(crate) async fn execute_step_tools(
        &mut self,
        rt: &mut StepRuntime,
        calls: Vec<ToolCall>,
        step: usize,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> engine::StepOutcome {
        if self.is_subagent() {
            return self.execute_child_tools(rt, calls, step, tx, cancel).await;
        }
        let (turn_id, config, _, _, _) = rt.root_parts();
        let turn_id = turn_id.to_string();
        let config = config.clone();
        let paused = rt.paused;
        self.execute_root_tools(
            &calls,
            &rt.step_item_ids,
            &turn_id,
            paused,
            rt.media,
            &config,
            tx,
            cancel,
        )
        .await
    }
}
