//! Child-agent turn policy and loop.
//!
//! A child is a `Session` in its own right (`Session::subagent` holds the
//! orchestration payload). The shared `session::engine::run_step` drives the
//! exact same sequence as a root turn; this module implements the child half
//! of the per-step policy and the outer loop (steering, step budget, pause and
//! settlement).

use super::*;
use std::time::Instant;

use std::collections::HashMap;

use pig_provider::ProviderEvent;

use super::compact::{MAX_CHILD_OVERFLOW_RECOVERIES, child_overflow_failure};
use super::persist::{
    child_agent_result_meta, child_persist_stop, persist_child_mailbox_message,
    persist_child_message_meta, persist_child_tool_result, persist_child_tool_starts,
};
use super::state::{ProgressSink, SubagentMode};
use crate::session::engine::{
    AttemptAction, AttemptOutcome, CommitAction, ProviderUsage, StepFailure, StepRuntime,
};

impl Session {
    /// Build the child Session from the shared preparation result. The child
    /// owns its agent context and a child-local tool state; approvals, policy
    /// rules, store and lifecycle handles are shared with the parent session.
    pub(super) fn new_subagent(
        prep: crate::agent::ChildPrep,
        parent: &Session,
        progress: ProgressSink,
        mode: SubagentMode,
    ) -> Session {
        let runtime_profile = prep.policy.profile.name.clone();
        let runtime_conversation_path = prep.jsonl.clone();
        let agent_id = prep.agent_id;
        let run = SubagentResult::child(
            crate::session::AgentRunStatus::Queued,
            "queued".to_string(),
            String::new(),
            0,
            (0, 0, 0),
        )
        .with_identity(&agent_id, prep.run_id.clone());
        let state = Session::child_local_state(&parent.state, &agent_id);
        state
            .agents
            .activate_child(crate::agent::AgentNode::child(
                agent_id.clone(),
                prep.parent_agent_id.clone(),
                prep.policy.delegation_depth,
                prep.run_id.clone(),
            ))
            .expect("a prepared child must not have an active conflicting run");
        state
            .agents
            .update_agent_context(&agent_id, runtime_profile, runtime_conversation_path);
        let delivery_receipts = state.delivery_receipts.clone();
        let tracker = match mode {
            SubagentMode::Foreground => parent.tracker.foreground_child(),
            SubagentMode::Background => parent.tracker.detached_child(),
        };
        let emitted_up_to = prep.history.len();
        Session {
            agent: crate::session::agent::Agent {
                id: agent_id,
                run: Some(run),
                cwd: prep.cwd,
                delegation_depth: prep.policy.delegation_depth,
                config: Some(prep.policy.model),
                history: prep.history,
                tools: prep.tools,
                schemas: prep.schemas,
            },
            subagent: Some(Box::new(super::state::SubagentState {
                parent_session_id: parent.event_session_id().to_string(),
                parent_agent_id: prep.parent_agent_id,
                profile: prep.policy.profile,
                jsonl: prep.jsonl,
                max_turns: prep.policy.max_turns,
                description: prep.description,
                output_schema: prep.policy.output_schema,
                plugin_tool_filter: Default::default(),
                mcp_inherits_all: prep.policy.mcp_inherits_all,
                trusted: prep.policy.trusted,
                persist_failure: None,
                writer: Some(prep.writer),
                progress,
                preserve_latest: false,
                pause_pin: None,
                overflow_recoveries: 0,
                emitted_up_to,
                tool_edits: HashMap::new(),
                tool_agents: HashMap::new(),
            })),
            user_ixs: Vec::new(),
            io_last_input: Vec::new(),
            tracker,
            state,
            always_allowed: parent.always_allowed.clone(),
            permission_rules: parent.permission_rules.clone(),
            plan_enabled: false,
            reviewer: parent.reviewer.clone(),
            pending: parent.pending.clone(),
            pending_questions: parent.pending_questions.clone(),
            permissions: prep.policy.permissions,
            model_override: None,
            rollout: None,
            rollout_failure: None,
            rollout_failure_reported: false,
            delivery_receipts,
            notification_delivery_failure: None,
            notification_reconcile_required: false,
            store: parent.store.clone(),
            data_dir: parent.data_dir.clone(),
            git_snapshot: parent.git_snapshot.clone(),
            agents_prompt: String::new(),
            skills_prompt: String::new(),
            memory_prompt: String::new(),
            memory_enabled: false,
            date_frozen: parent.date_frozen.clone(),
            date_reminded: String::new(),
            agents_reminded: String::new(),
            memory_reminded: String::new(),
            permissions_reminded: None,
            profiles_summary: prep.profiles_summary,
            prompt_text: None,
            prompt_memory_enabled: None,
            prompt_skills_state: None,
            projected_input_image: None,
            projected_input_pdf: None,
            recorded_prompt: None,
            run_state: Default::default(),
            agent_seq: 0,
            tool_item_seq: 0,
            repeat: Default::default(),
            streamed_calls: Default::default(),
            early_tools: Vec::new(),
            steer_signal: None,
            runtime: parent.runtime.clone(),
            trust: parent.trust.clone(),
            plugin_tools: prep.plugin_tools,
            plugin_tool_filter: Default::default(),
            mcp: prep.mcp,
            hooks: parent.hooks.clone(),
            hook_context_stash: Vec::new(),
            session_start_hook_ran: true,
            session_start_source: "subagent",
        }
    }

    pub(crate) fn record_child_partial(&self, text: &str) {
        let Some(run_id) = self.agent.run_id() else {
            return;
        };
        self.state
            .agents
            .record_partial(&self.agent.id, &run_id, text.to_string());
    }

    /// Subagent progress reporting (routed per sink).
    pub(super) fn report_child_progress(&self, tx: &async_channel::Sender<Event>, note: String) {
        let sub = self.subagent.as_ref().expect("child session");
        let run_id = self.agent.run_id();
        match &sub.progress {
            ProgressSink::Foreground { parent_item_id } => {
                let item_id = parent_item_id.clone();
                emit_bg(self.event_session_id(), tx, |session_id| {
                    Event::SubagentProgress {
                        session_id,
                        item_id,
                        note,
                        run_id,
                    }
                });
            }
            ProgressSink::Background { task_id } => {
                crate::task::note_output(&self.state.tasks, task_id, &format!("{note}\n"));
            }
        }
    }

    pub(super) async fn select_child_plugin_tools(&mut self, tx: &async_channel::Sender<Event>) {
        let filter = self
            .plugin_tools
            .select(self.agent.config(), Some(&self.agent.id), tx)
            .await;
        let sub = self.subagent.as_mut().expect("child session");
        sub.plugin_tool_filter = filter.clone();
        let inherits_all = sub.mcp_inherits_all;
        self.agent.tools.retain(|tool| !tool.is_plugin());
        self.agent.tools.extend(
            self.plugin_tools
                .tools_filtered(&filter)
                .into_iter()
                .filter(|tool| inherits_all || tool.read_only()),
        );
        self.agent.schemas = self
            .agent
            .tools
            .iter()
            .map(|tool| crate::tool::schema_with_strict(tool.as_ref()))
            .collect();
    }

    /// extra_tools for the gated execution segment: MCP/plugin tools narrowed
    /// by the inheritance rule + Skill (not in the `all()` static table,
    /// served via extra by name, under the same trust decision as the child
    /// prompt).
    pub(super) fn child_extra_tools(&self) -> Vec<Box<dyn tool::Tool>> {
        let sub = self.subagent.as_ref().expect("child session");
        child_extra_tools_for(
            &self.agent.cwd,
            self.mcp.as_ref(),
            &self.plugin_tools,
            &sub.plugin_tool_filter,
            sub.mcp_inherits_all,
            sub.trusted,
        )
    }

    pub(crate) async fn prepare_child_step(
        &mut self,
        rt: &mut StepRuntime,
        _step: usize,
        tx: &async_channel::Sender<Event>,
        _cancel: &CancellationToken,
    ) {
        self.select_child_plugin_tools(tx).await;
        rt.tools_tokens = crate::session::estimate::estimate_schemas_tokens(&self.agent.schemas);
        rt.assistant_row = self.agent.history.len();
        rt.ownership = self
            .subagent
            .as_ref()
            .expect("child session")
            .progress
            .ownership_prefix(&self.agent.id);
    }

    pub(crate) async fn preflight_child_step(
        &mut self,
        rt: &mut StepRuntime,
        step: usize,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> bool {
        let (preserve_latest, pause_pin, background) = {
            let sub = self.subagent.as_ref().expect("child session");
            (
                sub.preserve_latest,
                sub.pause_pin,
                matches!(sub.progress, ProgressSink::Background { .. }),
            )
        };
        let projected = crate::session::compact::project_context_tokens(
            self.run_state.usage_anchor.as_ref(),
            &self.agent.history,
            self.run_state.last_total_tokens,
            self.agent.config(),
            rt.tools_tokens,
        );
        if projected > super::compact_trigger(self.agent.config()) && !rt.recovery_attempted {
            self.report_child_progress(tx, format!("Step {step} · context compaction…"));
            let report = self
                .compact_child_context(
                    CompactTrigger::Watermark,
                    cancel,
                    preserve_latest,
                    pause_pin,
                    background,
                    step,
                )
                .await;
            match report.outcome {
                CompactOutcome::Cancelled => {
                    self.emit_child_conversation(rt, tx, None);
                    return true;
                }
                CompactOutcome::Completed => {
                    self.run_state.last_total_tokens = report.used_after;
                    let sub = self.subagent.as_mut().expect("child session");
                    sub.pause_pin = report.pin_after;
                    sub.emitted_up_to = sub.emitted_up_to.min(self.agent.history.len());
                    rt.assistant_row = self.agent.history.len();
                }
                CompactOutcome::Skipped | CompactOutcome::Failed => {}
            }
        }
        if rt.step_started.is_none() {
            rt.step_started = Some(Instant::now());
        }
        false
    }

    pub(crate) fn begin_child_attempt(
        &mut self,
        _rt: &mut StepRuntime,
        cancel: &CancellationToken,
    ) -> (
        tokio::sync::mpsc::Receiver<ProviderEvent>,
        tokio::task::JoinHandle<()>,
    ) {
        let mut request_config = self.agent.config().clone();
        if pig_provider::prompt_cache_key_supported(&self.agent.config().base_url) {
            request_config.prompt_cache_key = Some(pig_provider::subagent_prompt_cache_key(
                self.event_session_id(),
                &self.agent.id,
            ));
        }
        let mut control = pig_provider::CallControl::new(cancel.clone());
        if let Some(schema) = self
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.output_schema.clone())
        {
            control = control.with_structured_output(schema);
        }
        let (child_tx, child_rx) = tokio::sync::mpsc::channel(256);
        let task = tokio::spawn(pig_provider::stream_chat(
            request_config,
            self.agent.history.clone(),
            self.agent.schemas.clone(),
            child_tx,
            control,
        ));
        (child_rx, task)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_child_attempt(
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
        let api_duration_ms = api_started.elapsed().as_millis() as u64;
        let background = matches!(
            self.subagent.as_ref().expect("child session").progress,
            ProgressSink::Background { .. }
        );
        let mut api_record = crate::api_usage::ApiUsageRecord::new(
            self.event_session_id(),
            "subagent",
            self.agent.config(),
            api_started_ms,
            api_duration_ms,
        );
        api_record.step = Some(step as u32);
        api_record.agent_id = Some(self.agent.id.clone());
        api_record.background = background;
        api_record.ttft_ms = attempt
            .first_token_at
            .map(|at| at.duration_since(api_started).as_millis() as u64)
            .map(|ttft| ttft.min(api_duration_ms));
        api_record.finish = if attempt.failure.is_some() {
            "error"
        } else if cancel.is_cancelled() {
            "cancelled"
        } else {
            attempt.finish_reason.as_str()
        }
        .to_string();
        api_record.error = attempt.failure.as_ref().map(pig_provider::core_error_en);
        api_tracker.apply_to(&mut api_record);
        self.store
            .lock()
            .expect("store lock")
            .record_api_usage(&api_record);

        if let Some(reported) = attempt.usage {
            let ProviderUsage {
                input,
                cache_read,
                output,
                used,
                ..
            } = reported;
            self.run_state.input += input;
            self.run_state.cache_read += cache_read;
            self.run_state.output += output;
            self.run_state.last_total_tokens = Some(used);
        }
        if attempt.finish_reason.is_incomplete() {
            attempt.tool_calls.clear();
            if let Some(replay) = attempt.replay.as_mut() {
                replay.remove_tool_calls();
            }
        }

        if let Some(error) = attempt.failure.as_ref() {
            let overflow = matches!(error, CoreError::ContextWindowExceeded { .. });
            let overflow_recoveries = self
                .subagent
                .as_ref()
                .expect("child session")
                .overflow_recoveries;
            if overflow
                && !attempt.forwarded_content
                && !rt.recovery_attempted
                && overflow_recoveries < MAX_CHILD_OVERFLOW_RECOVERIES
            {
                tracing::warn!(
                    agent_id = %self.agent.id,
                    step,
                    "subagent context window exceeded before any content was forwarded; compacting and retrying this step once"
                );
                let used_before =
                    crate::session::estimate::estimate_history_tokens(&self.agent.history)
                        + rt.tools_tokens;
                self.report_child_progress(tx, format!("Step {step} · context compaction…"));
                let (preserve_latest, pause_pin) = {
                    let sub = self.subagent.as_ref().expect("child session");
                    (sub.preserve_latest, sub.pause_pin)
                };
                let report = self
                    .compact_child_context(
                        CompactTrigger::Overflow,
                        cancel,
                        preserve_latest,
                        pause_pin,
                        background,
                        step,
                    )
                    .await;
                match report.outcome {
                    CompactOutcome::Completed => {
                        rt.assistant_row = self.agent.history.len();
                        if let Some(used_after) = report.used_after
                            && used_after < used_before
                        {
                            let sub = self.subagent.as_mut().expect("child session");
                            sub.overflow_recoveries += 1;
                            rt.recovery_attempted = true;
                            self.run_state.usage_anchor = None;
                            self.run_state.last_total_tokens = Some(used_after);
                            sub.pause_pin = report.pin_after;
                            sub.emitted_up_to = sub.emitted_up_to.min(self.agent.history.len());
                            self.emit_child_conversation(rt, tx, attempt.reasoning_ms);
                            self.report_child_progress(
                                tx,
                                format!("Step {step} · retrying after compaction…"),
                            );
                            return AttemptAction::Retry;
                        }
                        tracing::warn!(
                            agent_id = %self.agent.id,
                            used_before,
                            used_after = ?report.used_after,
                            "child overflow compaction did not shrink the context; surfacing the error"
                        );
                    }
                    CompactOutcome::Cancelled => {
                        self.emit_child_conversation(rt, tx, attempt.reasoning_ms);
                        return AttemptAction::Terminal(
                            crate::session::engine::StepOutcome::Cancelled,
                        );
                    }
                    CompactOutcome::Skipped | CompactOutcome::Failed => {
                        tracing::warn!(
                            agent_id = %self.agent.id,
                            step,
                            "child context overflow cannot be compacted (no compactable prefix or the paused state does not fit); surfacing the error"
                        );
                    }
                }
            }
            self.emit_child_conversation(rt, tx, attempt.reasoning_ms);
            if overflow {
                rt.failure = Some(StepFailure::Overflow(error.clone()));
            } else {
                rt.failure = Some(StepFailure::Message(format!(
                    "Subagent model request failed: {}",
                    pig_provider::core_error_en(error)
                )));
            }
            return AttemptAction::Terminal(crate::session::engine::StepOutcome::Failed);
        }
        if cancel.is_cancelled() {
            self.emit_child_conversation(rt, tx, attempt.reasoning_ms);
            return AttemptAction::Terminal(crate::session::engine::StepOutcome::Cancelled);
        }
        AttemptAction::Continue
    }

    pub(crate) async fn commit_child_assistant(
        &mut self,
        rt: &mut StepRuntime,
        attempt: AttemptOutcome,
        _step: usize,
        tx: &async_channel::Sender<Event>,
    ) -> CommitAction {
        let provider_usage = attempt.usage;
        let finish_reason = attempt.finish_reason;
        let incomplete = finish_reason.is_incomplete();
        let reasoning_ms = attempt.reasoning_ms;
        let tool_calls = attempt.tool_calls;
        let mut assistant = ChatMsg::assistant_with_replay(
            attempt.text,
            tool_calls.clone(),
            Some(attempt.reasoning).filter(|reasoning| !reasoning.is_empty()),
            attempt.replay,
        );
        let run_id = self.agent.run_id().expect("child run identity");
        rt.step_item_ids = tool_calls
            .iter()
            .enumerate()
            .map(|(call_index, call)| child_tool_execution_id(&run_id, _step, call_index, &call.id))
            .collect();
        rt.last_text = assistant.content.clone().unwrap_or_default();
        if let Err(error) = persist_child_message_meta(
            self,
            &mut assistant,
            crate::agent::AgentMsgMeta {
                reasoning_ms,
                edit: None,
                tool_execution_ids: rt.step_item_ids.clone(),
                ..Default::default()
            },
        ) {
            tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
            return CommitAction::Terminal(crate::session::engine::StepOutcome::Failed);
        }
        self.agent.history.push(assistant);
        self.run_state.usage_anchor = provider_usage.map(|usage| UsageAnchor {
            model_key: model_key(self.agent.config()),
            history_len: self.agent.history.len(),
            tools_tokens: rt.tools_tokens,
            used: usage.used,
            system_fingerprint: system_fingerprint(&self.agent.history),
        });
        rt.incomplete_reason = incomplete.then_some(finish_reason);
        rt.reasoning_ms = reasoning_ms;
        let paused = finish_reason == pig_protocol::FinishReason::Pause;
        if tool_calls.is_empty() {
            self.emit_child_conversation(rt, tx, reasoning_ms);
            if paused {
                return CommitAction::Paused;
            }
            return CommitAction::TextOnly {
                finish_reason: rt.incomplete_reason,
            };
        }
        rt.paused = paused;
        CommitAction::Tools(tool_calls)
    }

    async fn execute_child_parallel_group(
        &mut self,
        rt: &mut StepRuntime,
        calls: &[ToolCall],
        start: usize,
        end: usize,
        step: usize,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> crate::session::engine::StepOutcome {
        let sub = self.subagent.as_ref().expect("child session");
        let context = ChildBatchContext {
            cwd: self.agent.cwd.clone(),
            agent_id: self.agent.id.clone(),
            run_id: self.agent.run_id(),
            config: self.agent.config().clone(),
            permissions: self.permissions,
            state: self.state.clone(),
            reviewer: self.reviewer.clone(),
            permission_rules: self.permission_rules.clone(),
            always_allowed: self.always_allowed.clone(),
            store: self.store.clone(),
            mcp: self.mcp.clone(),
            plugin_tools: self.plugin_tools.clone(),
            plugin_filter: sub.plugin_tool_filter.clone(),
            hooks: self.hooks.clone(),
            history: Arc::new(self.agent.history.clone()),
            session_id: self.event_session_id().to_string(),
            ownership: rt.ownership.clone(),
            mcp_inherits_all: sub.mcp_inherits_all,
            trusted: sub.trusted,
        };
        for (offset, call) in calls[start..end].iter().enumerate() {
            let call_ix = start + offset;
            self.report_child_progress(tx, format!("Step {step} · {}", tool::summarize(call)));
            self.emit_child_item(
                tx,
                pig_protocol::ConversationItem::Tool {
                    id: format!("t{}-{call_ix}", rt.assistant_row),
                    tool: call.name.clone(),
                    summary: tool::summarize(call),
                    detail: call.arguments.clone(),
                    output: String::new(),
                    status: pig_protocol::ConversationToolStatus::Running,
                    turn_id: None,
                    edit: None,
                    live_note: None,
                    agents: Vec::new(),
                    approval_pending: false,
                },
            );
        }
        let mut slots: Vec<Option<GatedToolOutcome>> = (0..end - start).map(|_| None).collect();
        let cancelled = super::super::batch::run_tool_batch(
            end - start,
            cancel,
            |offset| {
                let call_ix = start + offset;
                let call = calls[call_ix].clone();
                let item_id = rt.step_item_ids[call_ix].clone();
                let context = context.clone();
                let tx = tx.clone();
                let cancel = cancel.clone();
                async move { execute_child_batch_tool(call, item_id, context, tx, cancel).await }
            },
            |offset, outcome| slots[offset] = Some(outcome),
        )
        .await;
        let cancelled = cancelled
            || slots
                .iter()
                .any(|outcome| matches!(outcome, Some(GatedToolOutcome::Cancelled)));
        for offset in 0..end - start {
            let call = &calls[start + offset];
            // Model order: the wrap-up visits results in call order, so the
            // streak never depends on which member of the wave finished first.
            let repeat_streak = self.repeat.observe(call);
            let (mut message, edit) = match slots[offset].take() {
                Some(GatedToolOutcome::Executed {
                    mut output,
                    is_error,
                    edit,
                    images,
                    documents,
                    ..
                }) => {
                    crate::session::repeat::append_reminder(repeat_streak, &mut output);
                    let mut message = ChatMsg::tool_result_with_images(&call.id, output, images)
                        .with_error(is_error);
                    message.documents = documents;
                    (message, edit)
                }
                Some(GatedToolOutcome::Rejected { mut note }) => {
                    crate::session::repeat::append_reminder(repeat_streak, &mut note);
                    (ChatMsg::tool_result(&call.id, note).with_error(true), None)
                }
                Some(GatedToolOutcome::Cancelled) => (Session::stopped_tool_result(&call.id), None),
                None if cancelled => (Session::stopped_tool_result(&call.id), None),
                None => (
                    ChatMsg::tool_result(
                        &call.id,
                        "Internal tool execution error (the task terminated unexpectedly)"
                            .to_string(),
                    )
                    .with_error(true),
                    None,
                ),
            };
            if let Some(edit) = edit.clone() {
                self.subagent
                    .as_mut()
                    .expect("child session")
                    .tool_edits
                    .insert(call.id.clone(), edit);
            }
            if let Err(error) = persist_child_tool_result(
                self,
                call,
                &rt.step_item_ids[start + offset],
                &mut message,
                crate::agent::AgentMsgMeta {
                    reasoning_ms: None,
                    edit,
                    ..Default::default()
                },
            ) {
                tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                return crate::session::engine::StepOutcome::Failed;
            }
            self.agent.history.push(message);
        }
        if cancelled {
            for (call_index, call) in calls.iter().enumerate().skip(end) {
                let mut message = Session::stopped_tool_result(&call.id);
                if let Err(error) = persist_child_tool_result(
                    self,
                    call,
                    &rt.step_item_ids[call_index],
                    &mut message,
                    crate::agent::AgentMsgMeta::default(),
                ) {
                    tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                    return crate::session::engine::StepOutcome::Failed;
                }
                self.agent.history.push(message);
            }
            self.emit_child_conversation(rt, tx, rt.reasoning_ms);
            return crate::session::engine::StepOutcome::Cancelled;
        }
        crate::session::engine::StepOutcome::Continue
    }

    pub(crate) async fn execute_child_tools(
        &mut self,
        rt: &mut StepRuntime,
        calls: Vec<ToolCall>,
        step: usize,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> crate::session::engine::StepOutcome {
        let mask = {
            let rules = self
                .permission_rules
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            crate::session::turn::parallel_mask(
                &calls,
                &self.agent.tools,
                self.permissions,
                tool::MediaCapabilities::new(
                    self.agent.config().input_image,
                    self.agent.config().input_pdf,
                ),
                &rules,
                &self.state,
                &self.agent.cwd,
                &self.hooks,
            )
        };
        let mut call_ix = 0usize;
        while call_ix < calls.len() {
            if mask[call_ix] {
                let mut end = call_ix + 1;
                while end < calls.len() && mask[end] {
                    end += 1;
                }
                if let Err(error) =
                    persist_child_tool_starts(self, &calls, &rt.step_item_ids, call_ix, end)
                {
                    tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                    return crate::session::engine::StepOutcome::Failed;
                }
                match self
                    .execute_child_parallel_group(rt, &calls, call_ix, end, step, tx, cancel)
                    .await
                {
                    crate::session::engine::StepOutcome::Continue => {
                        call_ix = end;
                        continue;
                    }
                    outcome => return outcome,
                }
            }
            let child_call = &calls[call_ix];
            // Repeat breaker: every call counts in model order (a different
            // call resets the streak); only a gated result carries the reminder.
            let repeat_streak = self.repeat.observe(child_call);
            if let Err(error) =
                persist_child_tool_starts(self, &calls, &rt.step_item_ids, call_ix, call_ix + 1)
            {
                tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                return crate::session::engine::StepOutcome::Failed;
            }
            let summary = tool::summarize(child_call);
            self.report_child_progress(tx, format!("Step {step} · {summary}"));
            self.emit_child_item(
                tx,
                pig_protocol::ConversationItem::Tool {
                    id: format!("t{}-{call_ix}", rt.assistant_row),
                    tool: child_call.name.clone(),
                    summary,
                    detail: child_call.arguments.clone(),
                    output: String::new(),
                    status: pig_protocol::ConversationToolStatus::Running,
                    turn_id: None,
                    edit: None,
                    live_note: None,
                    agents: Vec::new(),
                    approval_pending: false,
                },
            );
            let child_item_id = rt.step_item_ids[call_ix].clone();
            let Some(child_tool) = self
                .agent
                .tools
                .iter()
                .find(|t| t.name() == child_call.name.as_str())
                .map(|t| t.as_ref())
            else {
                let note = format!(
                    "Unknown tool {} (the subagent's tool set is narrowed)",
                    child_call.name
                );
                let mut message = ChatMsg::tool_result(&child_call.id, note).with_error(true);
                if let Err(error) = persist_child_tool_result(
                    self,
                    child_call,
                    &rt.step_item_ids[call_ix],
                    &mut message,
                    crate::agent::AgentMsgMeta::default(),
                ) {
                    tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                    return crate::session::engine::StepOutcome::Failed;
                }
                self.agent.history.push(message);
                call_ix += 1;
                continue;
            };
            if matches!(child_call.name.as_str(), "Agent" | "AgentSwarm") {
                let parent_config = self.agent.config().clone();
                let outcome = if child_call.name == "Agent" {
                    self.run_subagent(
                        child_call,
                        &rt.ownership,
                        &child_item_id,
                        &parent_config,
                        tx,
                        cancel,
                    )
                    .await
                } else {
                    self.run_swarm(child_call, &child_item_id, &parent_config, tx, cancel)
                        .await
                };
                match outcome {
                    SubagentOutcome::Finished {
                        note,
                        is_error,
                        card,
                        cards,
                    } => {
                        let meta = child_agent_result_meta(self, &child_call.id, card, cards);
                        let mut message =
                            ChatMsg::tool_result(&child_call.id, note).with_error(is_error);
                        if let Err(error) = persist_child_tool_result(
                            self,
                            child_call,
                            &child_item_id,
                            &mut message,
                            meta,
                        ) {
                            tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                            return crate::session::engine::StepOutcome::Failed;
                        }
                        self.agent.history.push(message);
                    }
                    SubagentOutcome::Cancelled { card, cards } => {
                        let meta = child_agent_result_meta(self, &child_call.id, card, cards);
                        for (stopped_index, stopped_call) in calls.iter().enumerate().skip(call_ix)
                        {
                            let mut message = Session::stopped_tool_result(&stopped_call.id);
                            let result_meta = if stopped_index == call_ix {
                                meta.clone()
                            } else {
                                crate::agent::AgentMsgMeta::default()
                            };
                            if let Err(error) = persist_child_tool_result(
                                self,
                                stopped_call,
                                &rt.step_item_ids[stopped_index],
                                &mut message,
                                result_meta,
                            ) {
                                tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                                return crate::session::engine::StepOutcome::Failed;
                            }
                            self.agent.history.push(message);
                        }
                        self.emit_child_conversation(rt, tx, rt.reasoning_ms);
                        return crate::session::engine::StepOutcome::Cancelled;
                    }
                }
                call_ix += 1;
                continue;
            }
            let extra_tools = self.child_extra_tools();
            let media = tool::MediaCapabilities::new(
                self.agent.config().input_image,
                self.agent.config().input_pdf,
            );
            let event_session_id = self.event_session_id().to_string();
            let mut gate = GateCtx {
                cwd: &self.agent.cwd,
                agent_id: &self.agent.id,
                run_id: self.agent.run_id(),
                permissions: self.permissions,
                allow_scope_widening: false,
                media,
                tracker: &mut self.tracker,
                state: &self.state,
                owns_todo_state: false,
                reviewer: self.reviewer.as_ref(),
                permission_rules: &self.permission_rules,
                always_allowed: &self.always_allowed,
                session_id: &event_session_id,
                plan_enabled: false,
                store: &self.store,
                extra_tools: &extra_tools,
                hooks: &self.hooks,
                history: &self.agent.history,
            };
            let outcome = exec_tool_gated_ctx(
                &mut gate,
                child_call,
                Some(child_tool),
                &child_item_id,
                &rt.ownership,
                tx,
                cancel,
            )
            .await;
            match outcome {
                GatedToolOutcome::Cancelled => {
                    for (stopped_index, stopped_call) in calls.iter().enumerate().skip(call_ix) {
                        let mut message = Session::stopped_tool_result(&stopped_call.id);
                        if let Err(error) = persist_child_tool_result(
                            self,
                            stopped_call,
                            &rt.step_item_ids[stopped_index],
                            &mut message,
                            crate::agent::AgentMsgMeta::default(),
                        ) {
                            tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                            return crate::session::engine::StepOutcome::Failed;
                        }
                        self.agent.history.push(message);
                    }
                    let reasoning_ms = rt.reasoning_ms;
                    self.emit_child_conversation(rt, tx, reasoning_ms);
                    return crate::session::engine::StepOutcome::Cancelled;
                }
                GatedToolOutcome::Rejected { mut note } => {
                    crate::session::repeat::append_reminder(repeat_streak, &mut note);
                    let mut message = ChatMsg::tool_result(&child_call.id, note).with_error(true);
                    if let Err(error) = persist_child_tool_result(
                        self,
                        child_call,
                        &rt.step_item_ids[call_ix],
                        &mut message,
                        crate::agent::AgentMsgMeta::default(),
                    ) {
                        tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                        return crate::session::engine::StepOutcome::Failed;
                    }
                    self.agent.history.push(message);
                }
                GatedToolOutcome::Executed {
                    mut output,
                    is_error,
                    edit,
                    images,
                    documents,
                    ..
                } => {
                    crate::session::repeat::append_reminder(repeat_streak, &mut output);
                    let mut message =
                        ChatMsg::tool_result_with_images(&child_call.id, output, images)
                            .with_error(is_error);
                    message.documents = documents;
                    if let Some(edit) = edit.clone() {
                        self.subagent
                            .as_mut()
                            .expect("child session")
                            .tool_edits
                            .insert(child_call.id.clone(), edit);
                    }
                    if let Err(error) = persist_child_tool_result(
                        self,
                        child_call,
                        &rt.step_item_ids[call_ix],
                        &mut message,
                        crate::agent::AgentMsgMeta {
                            reasoning_ms: None,
                            edit,
                            ..Default::default()
                        },
                    ) {
                        tracing::warn!(agent_id = %self.agent.id, error = ?error, "subagent context persistence failed");
                        return crate::session::engine::StepOutcome::Failed;
                    }
                    self.agent.history.push(message);
                }
            }
            call_ix += 1;
        }
        let reasoning_ms = rt.reasoning_ms;
        self.emit_child_conversation(rt, tx, reasoning_ms);
        if rt.paused {
            crate::session::engine::StepOutcome::Paused
        } else {
            crate::session::engine::StepOutcome::Continue
        }
    }

    /// One child conversation item upsert.
    pub(super) fn emit_child_item(
        &self,
        tx: &async_channel::Sender<Event>,
        item: pig_protocol::ConversationItem,
    ) {
        let session_id = self.event_session_id().to_string();
        let agent_id = self.agent.id.clone();
        let run_id = self.agent.run_id();
        emit_bg(&session_id, tx, move |session_id| {
            Event::ConversationUpdate {
                session_id,
                agent_id: Some(agent_id),
                run_id,
                item,
            }
        });
    }

    /// One child conversation item removal.
    fn emit_child_remove(&self, tx: &async_channel::Sender<Event>, item_ids: Vec<String>) {
        if item_ids.is_empty() {
            return;
        }
        let session_id = self.event_session_id().to_string();
        let agent_id = self.agent.id.clone();
        let run_id = self.agent.run_id();
        emit_bg(&session_id, tx, move |session_id| {
            Event::ConversationRemove {
                session_id,
                agent_id: Some(agent_id),
                run_id,
                item_ids,
            }
        });
    }

    /// A streamed live row: tracked once so the step's final projection can
    /// remove it, and upserted by its stable id on every delta.
    pub(crate) fn emit_child_live(
        &self,
        rt: &mut StepRuntime,
        tx: &async_channel::Sender<Event>,
        item: pig_protocol::ConversationItem,
    ) {
        if !rt.live_items.iter().any(|id| id == item.id()) {
            rt.live_items.push(item.id().to_string());
        }
        self.emit_child_item(tx, item);
    }

    /// Project the new child history since the watermark into shared
    /// conversation items, removing streamed rows the final projection
    /// replaces.
    pub(super) fn emit_child_conversation(
        &mut self,
        rt: &mut StepRuntime,
        tx: &async_channel::Sender<Event>,
        reasoning_ms: Option<u64>,
    ) {
        let history_len = self.agent.history.len();
        let mut stale = Vec::new();
        let items = {
            let sub = self.subagent.as_mut().expect("child session");
            let emitted = sub.emitted_up_to;
            let mut items = crate::agent::project_display_items_with_metadata(
                &self.agent.history[emitted..],
                &sub.tool_edits,
                &sub.tool_agents,
                emitted,
            );
            if let Some(ms) = reasoning_ms
                && let Some(pig_protocol::ConversationItem::Reasoning { duration_ms, .. }) =
                    items.iter_mut().rev().find(|item| {
                        matches!(
                            item,
                            pig_protocol::ConversationItem::Reasoning {
                                duration_ms: None,
                                ..
                            }
                        )
                    })
            {
                *duration_ms = Some(ms);
            }
            if !rt.live_items.is_empty() {
                stale = rt
                    .live_items
                    .drain(..)
                    .filter(|id| !items.iter().any(|item| item.id() == id))
                    .collect();
            }
            sub.emitted_up_to = history_len;
            items
        };
        self.emit_child_remove(tx, stale);
        for item in items {
            self.emit_child_item(tx, item);
        }
    }

    /// Project history since the watermark without touching streamed rows
    /// (used between steps, where no live row exists).
    pub(super) fn emit_child_history_since(
        &mut self,
        tx: &async_channel::Sender<Event>,
        reasoning_ms: Option<u64>,
    ) {
        let mut rt = StepRuntime::child();
        self.emit_child_conversation(&mut rt, tx, reasoning_ms);
    }
}

fn child_tool_execution_id(run_id: &str, step: usize, call_index: usize, call_id: &str) -> String {
    format!("{run_id}-c{step}-{call_index}-{call_id}")
}

/// Subagent result file path: {agents_dir}/{agent_id}.result.md (same
/// directory as the context jsonl)
pub(super) fn agent_result_path(jsonl: &Path, agent_id: &str) -> PathBuf {
    jsonl.with_file_name(format!("{agent_id}.result.md"))
}

/// Subagent result budget: returned verbatim within 32K chars; beyond that,
/// return the first 32K + truncation guidance. The full text is persisted
/// first by the caller to {agents_dir}/{agent_id}.result.md (same as kimi's
/// output.log), which result_path points to; None = persist failure.
fn truncate_agent_result(result_path: Option<&Path>, result: String) -> String {
    const MAX_RESULT_CHARS: usize = 32_000;
    if result.chars().count() <= MAX_RESULT_CHARS {
        return result;
    }
    let hint = match result_path {
        Some(path) => format!(
            "\n\n[Result too long; truncated. Full text: {}]",
            path.display()
        ),
        None => "\n\n[Result too long; truncated. Failed to save the full text]".to_string(),
    };
    let head: String = result.chars().take(MAX_RESULT_CHARS).collect();
    format!("{head}{hint}")
}

fn subagent_pause_limit(usage: (u64, u64, u64), turns: usize) -> SubagentResult {
    let note = format!("Subagent model request failed: {}", pause_limit_detail());
    SubagentResult::child(
        crate::session::AgentRunStatus::Failed,
        note.clone(),
        note,
        turns,
        usage,
    )
}

/// The child's tool loop outlived the repeat breaker's final reminder. The
/// batch is durable by then, so the drive ends at a clean boundary.
fn subagent_repeat_stop(
    usage: (u64, u64, u64),
    turns: usize,
    stopped: &crate::session::repeat::StoppedRepeat,
) -> SubagentResult {
    let note = format!(
        "Subagent stopped: {}",
        crate::session::repeat::repeat_stop_detail(stopped)
    );
    SubagentResult::child(
        crate::session::AgentRunStatus::Failed,
        note.clone(),
        note,
        turns,
        usage,
    )
}

pub(super) fn unexpected_subagent_result() -> SubagentResult {
    SubagentResult::child(
        crate::session::AgentRunStatus::Failed,
        "driver terminated unexpectedly".to_string(),
        "The subagent driver terminated unexpectedly before reporting a result".to_string(),
        0,
        (0, 0, 0),
    )
}

impl Session {
    /// Child drive entry: run the loop, persist the full result text to
    /// {agents_dir}/{agent_id}.result.md (success and failure alike), then
    /// truncate `result_text` to the 32K budget. Cancel/kill writes no
    /// artifact.
    pub(crate) async fn run_subagent_task(
        &mut self,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> SubagentResult {
        self.plugin_tools.dispatcher().set_event_sender(tx.clone());
        // The child conversation is running from this point; the lifecycle
        // emits the matching running=false status when it settles (also on
        // cancel/panic).
        let agent_id = self.agent.id.clone();
        let run_id = self.agent.run_id().expect("subagent run identity");
        self.state.agents.begin_run(&agent_id, run_id.clone());
        self.record_child_partial("");
        if self
            .subagent
            .as_ref()
            .is_some_and(|subagent| subagent.output_schema.is_some())
            && !self.agent.config().cap_structured
        {
            let result = SubagentResult::child(
                crate::session::AgentRunStatus::Failed,
                "structured output is unsupported by the selected model".to_string(),
                "The selected subagent model does not support structured output".to_string(),
                0,
                (0, 0, 0),
            )
            .with_identity(&agent_id, run_id);
            self.agent.run = Some(result.clone());
            return result;
        }
        let session_id = self.event_session_id().to_string();
        let event_agent_id = agent_id.clone();
        let event_run_id = Some(run_id.clone());
        emit_bg(&session_id, tx, move |session_id| {
            Event::ConversationStatus {
                session_id,
                agent_id: event_agent_id,
                running: true,
                run_id: event_run_id,
            }
        });
        if let Some(run) = self.agent.run.as_mut() {
            run.status = crate::session::AgentRunStatus::Running;
        }
        let result_path = agent_result_path(
            &self.subagent.as_ref().expect("child session").jsonl,
            &self.agent.id,
        );
        let mut result = self.run_subagent_loop(tx, cancel).await;
        if result.is_completed()
            && self
                .subagent
                .as_ref()
                .is_some_and(|subagent| subagent.output_schema.is_some())
        {
            match serde_json::from_str::<serde_json::Value>(result.text().trim()) {
                Ok(value) if value.is_object() => {
                    if let Some(artifact) = result.artifact.as_mut() {
                        artifact.structured = Some(value);
                    }
                }
                _ => {
                    let note = format!(
                        "Subagent returned invalid structured output; expected a JSON object matching output_schema. Raw result: {}",
                        result.text()
                    );
                    if let Some(artifact) = result.artifact.as_mut() {
                        artifact.text = note;
                    }
                    result.status = crate::session::AgentRunStatus::Failed;
                    result.stop_reason = "invalid structured output".to_string();
                }
            }
        }
        if !result.is_cancelled() {
            let written = std::fs::write(&result_path, result.text()).is_ok();
            let text = truncate_agent_result(
                written.then_some(result_path.as_path()),
                result.text().to_string(),
            );
            if let Some(artifact) = result.artifact.as_mut() {
                artifact.path = written.then_some(result_path);
                artifact.text = text;
                artifact.cwd = Some(self.agent.cwd.clone());
            }
        }
        let result = result.with_identity(&agent_id, run_id);
        self.agent.run = Some(result.clone());
        result
    }

    /// Outer child loop: steering, step budget, pause continuation and
    /// settlement. The step sequence itself is the shared `engine::run_step`.
    async fn run_subagent_loop(
        &mut self,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> SubagentResult {
        let mut steps_run = 0usize;
        let mut completed = false;
        let mut incomplete_reason = None;
        let mut last_text = String::new();
        let mut pause_continuations = 0usize;
        let max_turns = self.subagent.as_ref().expect("child session").max_turns;
        self.repeat.reset();
        for step in 1..=max_turns {
            steps_run = step;
            if let Some(error) = self
                .subagent
                .as_ref()
                .expect("child session")
                .persist_failure
                .clone()
            {
                let usage = (
                    self.run_state.input,
                    self.run_state.cache_read,
                    self.run_state.output,
                );
                return child_persist_stop(self, &error, steps_run, usage);
            }
            let steered: Vec<crate::agent::AgentSteer> =
                self.state.agents.drain_mailbox(&self.agent.id);
            for steer in steered {
                let wrapped = format!(
                    "New message from the coordinating agent (delivered while you were working):\n\n{}",
                    steer.text
                );
                let message = ChatMsg::user(wrapped);
                if let Err(error) = persist_child_mailbox_message(self, &steer, &message) {
                    self.state.agents.prepend_steer(&self.agent.id, steer);
                    let usage = (
                        self.run_state.input,
                        self.run_state.cache_read,
                        self.run_state.output,
                    );
                    return child_persist_stop(self, &error, steps_run, usage);
                }
                self.agent.history.push(message);
                self.emit_child_history_since(tx, None);
                if let Err(error) = self.state.agents.consume_steer(&steer.id) {
                    tracing::warn!(agent_id = %self.agent.id, %error, "mailbox consumption was not persisted");
                }
                if let Some(notification) = steer.notification {
                    crate::task::mark_agent_notification_delivered(
                        &self.state,
                        &notification.task_id,
                        Some(&notification.id),
                    );
                }
            }
            self.report_child_progress(tx, format!("Step {step} · thinking…"));
            let mut rt = StepRuntime::child();
            let outcome = crate::session::engine::run_step(self, &mut rt, step, tx, cancel).await;
            let paused = rt.paused;
            let failure = rt.failure.take();
            last_text = std::mem::take(&mut rt.last_text);
            incomplete_reason = rt.incomplete_reason;
            match outcome {
                crate::session::engine::StepOutcome::Continue => {
                    if let Some(stopped) = self.repeat.tripped() {
                        let usage = (
                            self.run_state.input,
                            self.run_state.cache_read,
                            self.run_state.output,
                        );
                        return subagent_repeat_stop(usage, steps_run, stopped);
                    }
                    let sub = self.subagent.as_mut().expect("child session");
                    if paused {
                        if pause_continuations >= MAX_PAUSE_CONTINUATIONS {
                            let usage = (
                                self.run_state.input,
                                self.run_state.cache_read,
                                self.run_state.output,
                            );
                            return subagent_pause_limit(usage, steps_run);
                        }
                        pause_continuations += 1;
                        sub.preserve_latest = true;
                        sub.pause_pin = Some(self.agent.history.len().saturating_sub(1));
                    } else {
                        sub.preserve_latest = false;
                        sub.pause_pin = None;
                    }
                }
                crate::session::engine::StepOutcome::TextOnly { finish_reason } => {
                    incomplete_reason = finish_reason.or(incomplete_reason);
                    completed = true;
                    break;
                }
                // Children never subscribe to the steer signal; a preempted
                // step cannot reach this loop.
                crate::session::engine::StepOutcome::Preempted => {}
                crate::session::engine::StepOutcome::Paused => {
                    let sub = self.subagent.as_mut().expect("child session");
                    if pause_continuations >= MAX_PAUSE_CONTINUATIONS {
                        let usage = (
                            self.run_state.input,
                            self.run_state.cache_read,
                            self.run_state.output,
                        );
                        return subagent_pause_limit(usage, steps_run);
                    }
                    pause_continuations += 1;
                    sub.preserve_latest = true;
                    sub.pause_pin = Some(self.agent.history.len().saturating_sub(1));
                }
                crate::session::engine::StepOutcome::Cancelled => {
                    let usage = (
                        self.run_state.input,
                        self.run_state.cache_read,
                        self.run_state.output,
                    );
                    return SubagentResult::child(
                        crate::session::AgentRunStatus::Cancelled,
                        "cancelled".to_string(),
                        String::new(),
                        steps_run,
                        usage,
                    );
                }
                crate::session::engine::StepOutcome::Failed => {
                    let usage = (
                        self.run_state.input,
                        self.run_state.cache_read,
                        self.run_state.output,
                    );
                    if let Some(error) = self
                        .subagent
                        .as_ref()
                        .expect("child session")
                        .persist_failure
                        .clone()
                    {
                        return child_persist_stop(self, &error, steps_run, usage);
                    }
                    match failure {
                        Some(StepFailure::Overflow(error)) => {
                            return child_overflow_failure(
                                &self.agent.id,
                                self.agent.config().context_window,
                                &pig_provider::core_error_en(&error),
                                usage,
                                steps_run,
                            );
                        }
                        Some(StepFailure::Message(note)) => {
                            return SubagentResult::child(
                                crate::session::AgentRunStatus::Failed,
                                note.clone(),
                                note,
                                steps_run,
                                usage,
                            );
                        }
                        None => {
                            return SubagentResult::child(
                                crate::session::AgentRunStatus::Failed,
                                "subagent drive failed".to_string(),
                                "The subagent drive failed".to_string(),
                                steps_run,
                                usage,
                            );
                        }
                    }
                }
            }
        }
        let usage = (
            self.run_state.input,
            self.run_state.cache_read,
            self.run_state.output,
        );
        if completed {
            if last_text.is_empty() {
                SubagentResult::child(
                    crate::session::AgentRunStatus::Failed,
                    "subagent produced no final text".to_string(),
                    "The subagent produced no final text".to_string(),
                    steps_run,
                    usage,
                )
            } else if let Some(reason) = incomplete_reason {
                SubagentResult::child(
                    crate::session::AgentRunStatus::Completed,
                    format!("with incomplete model response ({})", reason.as_str()),
                    format!(
                        "[Model response incomplete: {}]\n\n{last_text}",
                        reason.as_str()
                    ),
                    steps_run,
                    usage,
                )
            } else {
                SubagentResult::child(
                    crate::session::AgentRunStatus::Completed,
                    "completed".to_string(),
                    last_text,
                    steps_run,
                    usage,
                )
            }
        } else if last_text.is_empty() {
            SubagentResult::child(
                crate::session::AgentRunStatus::Failed,
                format!(
                    "reached the maximum turn count ({max_turns}), subagent produced no conclusion"
                ),
                format!(
                    "Reached the maximum turn count ({max_turns}). The subagent produced no conclusion"
                ),
                steps_run,
                usage,
            )
        } else {
            SubagentResult::child(
                crate::session::AgentRunStatus::Failed,
                format!("reached the maximum turn count {max_turns}"),
                format!("Reached the maximum turn count ({max_turns}). {last_text}"),
                steps_run,
                usage,
            )
        }
    }
}

#[derive(Clone)]
struct ChildBatchContext {
    cwd: PathBuf,
    agent_id: String,
    run_id: Option<String>,
    config: ResolvedModel,
    permissions: pig_protocol::PermissionSettings,
    state: crate::task::SessionToolState,
    reviewer: Arc<dyn ApprovalReviewer>,
    permission_rules: Arc<Mutex<crate::permissions::PermissionRules>>,
    always_allowed: Arc<Mutex<HashSet<(String, String)>>>,
    store: Arc<Mutex<crate::store::Store>>,
    mcp: Option<Arc<crate::mcp::McpManager>>,
    plugin_tools: crate::plugins::PluginToolset,
    plugin_filter: crate::plugins::PluginToolFilter,
    hooks: crate::hooks::HookSet,
    history: Arc<Vec<ChatMsg>>,
    session_id: String,
    ownership: String,
    mcp_inherits_all: bool,
    trusted: bool,
}

fn child_extra_tools_for(
    cwd: &Path,
    mcp: Option<&Arc<crate::mcp::McpManager>>,
    plugin_tools: &crate::plugins::PluginToolset,
    plugin_filter: &crate::plugins::PluginToolFilter,
    mcp_inherits_all: bool,
    trusted: bool,
) -> Vec<Box<dyn tool::Tool>> {
    let mut extra = mcp
        .map(|mcp| mcp.child_tools(mcp_inherits_all))
        .unwrap_or_default();
    extra.push(Box::new(tool::SkillTool::new_trusted(
        cwd,
        &plugin_tools.registry(),
        trusted,
    )));
    extra.extend(
        plugin_tools
            .tools_filtered(plugin_filter)
            .into_iter()
            .filter(|tool| mcp_inherits_all || tool.read_only()),
    );
    extra
}

async fn execute_child_batch_tool(
    call: ToolCall,
    item_id: String,
    context: ChildBatchContext,
    tx: async_channel::Sender<Event>,
    cancel: CancellationToken,
) -> GatedToolOutcome {
    let mut tracker = crate::tool::ChangeTracker::default();
    let extra_tools = child_extra_tools_for(
        &context.cwd,
        context.mcp.as_ref(),
        &context.plugin_tools,
        &context.plugin_filter,
        context.mcp_inherits_all,
        context.trusted,
    );
    let media = tool::MediaCapabilities::new(context.config.input_image, context.config.input_pdf);
    let mut gate = GateCtx {
        cwd: &context.cwd,
        agent_id: &context.agent_id,
        run_id: context.run_id.clone(),
        permissions: context.permissions,
        allow_scope_widening: false,
        media,
        tracker: &mut tracker,
        state: &context.state,
        owns_todo_state: false,
        reviewer: context.reviewer.as_ref(),
        permission_rules: &context.permission_rules,
        always_allowed: &context.always_allowed,
        session_id: &context.session_id,
        plan_enabled: false,
        store: &context.store,
        extra_tools: &extra_tools,
        hooks: &context.hooks,
        history: &context.history,
    };
    exec_tool_gated_ctx(
        &mut gate,
        &call,
        None,
        &item_id,
        &context.ownership,
        &tx,
        &cancel,
    )
    .await
}
