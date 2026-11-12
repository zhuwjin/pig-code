use super::*;

use std::collections::HashMap;

use super::state::{ProgressSink, SubagentDrive};

use super::compact::{
    ChildAttemptOutput, MAX_CHILD_OVERFLOW_RECOVERIES, child_overflow_failure,
    compact_child_context,
};
use super::persist::{child_persist_stop, persist_child_message, persist_child_message_meta};

/// Subagent progress reporting (routed per sink, see ProgressSink)
fn report_progress(
    ctx: &GateCtx<'_>,
    sink: &ProgressSink,
    tx: &async_channel::Sender<Event>,
    note: String,
) {
    match sink {
        ProgressSink::Foreground { parent_item_id } => {
            let item_id = parent_item_id.clone();
            emit_bg(ctx.session_id, tx, |session_id| Event::SubagentProgress {
                session_id,
                item_id,
                note,
            });
        }
        ProgressSink::Background { task_id } => {
            crate::task::note_output(&ctx.state.tasks, task_id, &format!("{note}\n"));
        }
    }
}

/// The subagent exhausted its pause-turn continuation budget: return an
/// explicit English failure to the parent instead of silently treating the
/// pause as a stop.
fn subagent_pause_limit(usage: (u64, u64, u64), turns: usize) -> SubagentDriveResult {
    let note = format!("Subagent model request failed: {}", pause_limit_detail());
    SubagentDriveResult {
        status_line: format!("failed: {note}"),
        result_text: note,
        turns,
        completed: false,
        is_error: true,
        usage,
        cancelled: false,
    }
}

/// Subagent result file path: {agents_dir}/{agent_id}.result.md (same
/// directory as the context jsonl)
pub(super) fn agent_result_path(jsonl: &Path, agent_id: &str) -> PathBuf {
    jsonl.with_file_name(format!("{agent_id}.result.md"))
}
/// Subagent drive entry: run the drive loop; the full result text is first
/// persisted to {agents_dir}/{agent_id}.result.md (written on success and
/// failure alike — failure writes the error explanation; cancel/kill has no
/// artifact and is skipped; a write failure only degrades — the truncation
/// hint has no file to point at — and is not fatal), then result_text is
/// truncated to the 32K budget.
pub(super) async fn drive_subagent(
    ctx: &mut GateCtx<'_>,
    run: &mut SubagentDrive,
    progress: &ProgressSink,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> SubagentDriveResult {
    run.plugin_tools.dispatcher().set_event_sender(tx.clone());
    let result_path = agent_result_path(&run.jsonl, &run.agent_id);
    let mut result = drive_subagent_loop(ctx, run, progress, tx, cancel).await;
    if !result.cancelled {
        let written = std::fs::write(&result_path, &result.result_text).is_ok();
        result.result_text = truncate_agent_result(
            written.then_some(result_path.as_path()),
            std::mem::take(&mut result.result_text),
        );
    }
    result
}
/// Subagent live display-item reporting (live-only, sent in both foreground
/// and background): project the new messages in history[emitted..] into
/// display items and emit them one by one (tool results within a batch get
/// their output backfilled by tool_call_id), advancing the watermark. The
/// panel claims them by agent_id.
fn emit_activity_since(
    ctx: &GateCtx<'_>,
    tx: &async_channel::Sender<Event>,
    agent_id: &str,
    history: &[ChatMsg],
    edits: &HashMap<String, pig_protocol::EditDiff>,
    reasoning_ms: Option<u64>,
    emitted: &mut usize,
) {
    let mut items = crate::agent::project_display_items_with_edits(&history[*emitted..], edits);
    if let Some(ms) = reasoning_ms {
        if let Some(item) = items
            .iter_mut()
            .rev()
            .find(|item| item.role == "reasoning" && item.duration_ms.is_none())
        {
            item.duration_ms = Some(ms);
        }
    }
    for item in items {
        let agent_id = agent_id.to_string();
        emit_bg(ctx.session_id, tx, move |session_id| {
            Event::SubagentActivity {
                session_id,
                agent_id,
                item: Some(item),
                finished: false,
            }
        });
    }
    *emitted = history.len();
}

pub(super) fn unexpected_drive_result() -> SubagentDriveResult {
    SubagentDriveResult {
        status_line: "failed: driver terminated unexpectedly".to_string(),
        result_text: "The subagent driver terminated unexpectedly before reporting a result"
            .to_string(),
        turns: 0,
        completed: false,
        is_error: true,
        usage: (0, 0, 0),
        cancelled: false,
    }
}

/// Subagent drive loop (shared by foreground/background): sampling with an
/// independent context + gated execution with the narrowed tool set; the
/// parent timeline has only one Agent tool card (foreground progress goes
/// through SubagentProgress, child tools emit no top-level events); messages
/// appended at each step are reported live via SubagentActivity (incremental
/// display in the "subagents" tab on the right).
async fn drive_subagent_loop(
    ctx: &mut GateCtx<'_>,
    run: &mut SubagentDrive,
    progress: &ProgressSink,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> SubagentDriveResult {
    // Steer registration: presence in the registry = this agent is running
    // (SendMessage checks it); the guard removes the key on every exit path
    // (finish, failure, cancel) so late senders get an error, not a lost
    // message
    struct SteerUnregister<'a>(&'a crate::task::SessionToolState, String);
    impl Drop for SteerUnregister<'_> {
        fn drop(&mut self) {
            self.0
                .agent_steers
                .lock()
                .expect("agent steers lock")
                .remove(&self.1);
        }
    }
    ctx.state
        .agent_steers
        .lock()
        .expect("agent steers lock")
        .insert(run.agent_id.clone(), Default::default());
    let _steer_guard = SteerUnregister(ctx.state, run.agent_id.clone());
    let mut usage = (0u64, 0u64, 0u64);
    let mut last_text = String::new();
    let mut steps_run = 0usize;
    let mut completed = false;
    let mut incomplete_reason = None;
    let mut pause_continuations = 0usize;
    // Live-reporting watermark: length of the history prefix already projected
    // as SubagentActivity (the initial system/user/resume history does not
    // count — LoadSubagent covers it with a full load)
    let mut emitted_up_to = run.history.len();
    let mut tool_edits: HashMap<String, pig_protocol::EditDiff> = HashMap::new();
    // Unique per child: foreground swarm children share the parent tool card,
    // so approval/hook ids need the agent_id too.
    let ownership = progress.ownership_prefix(&run.agent_id);
    // Child context preflight state: the provider's real usage (when it
    // reports one) anchors the next projection exactly like the root session.
    let mut usage_anchor: Option<super::UsageAnchor> = None;
    let mut last_total_tokens: Option<u64> = None;
    // A pause continuation keeps the paused assistant/server round and any
    // steer that arrived after it; a pinned range that cannot fit skips
    // compaction instead of dropping that state.
    let mut preserve_latest = false;
    let mut pause_pin: Option<usize> = None;
    let mut overflow_recoveries = 0usize;
    for step in 1..=run.max_turns {
        steps_run = step;
        // A context write failed in an earlier step (including the compaction
        // marker): stop before touching the live history again.
        if let Some(error) = run.persist_failure.clone() {
            return child_persist_stop(run, &error, steps_run, usage);
        }
        // Coordinator steering: messages queued via SendMessage enter the
        // conversation between steps — never mid-tool, never mid-stream
        let steered: Vec<String> = {
            let mut registry = ctx.state.agent_steers.lock().expect("agent steers lock");
            registry
                .get_mut(&run.agent_id)
                .map(|queue| queue.drain(..).collect())
                .unwrap_or_default()
        };
        for message in steered {
            let wrapped = format!(
                "New message from the coordinating agent (delivered while you were working):\n\n{message}"
            );
            let mut message = ChatMsg::user(wrapped);
            if let Err(error) = persist_child_message(run, &mut message) {
                return child_persist_stop(run, &error, steps_run, usage);
            }
            run.history.push(message);
            emit_activity_since(ctx, tx, &run.agent_id, &run.history, &tool_edits, None, &mut emitted_up_to);
        }
        report_progress(ctx, progress, tx, format!("Step {step} · thinking…"));
        run.select_plugin_tools(tx).await;
        let tools_tokens = crate::session::estimate::estimate_schemas_tokens(&run.schemas);
        // One step may spend at most one reactive overflow recovery; the drive
        // as a whole is bounded by MAX_CHILD_OVERFLOW_RECOVERIES.
        let mut attempt_recovered = false;
        let mut attempt_used: Option<u64>;
        // The successful attempt's outputs cross the retry-loop boundary as
        // one value; a failed attempt's partial values never leak into the
        // retry or into history.
        let attempt_output: Option<ChildAttemptOutput>;
        'attempt: loop {
            // Only this attempt's real usage may anchor the next preflight.
            attempt_used = None;
            // ---- preflight: project the next request with the child's own
            // window/capabilities and compact before sampling. Runs once per
            // step; a reactive retry below already compacted this request. ----
            if !attempt_recovered {
                let projected = crate::session::compact::project_context_tokens(
                    usage_anchor.as_ref(),
                    &run.history,
                    last_total_tokens,
                    &run.child_config,
                    tools_tokens,
                );
                if projected > super::compact_trigger(&run.child_config) {
                    report_progress(
                        ctx,
                        progress,
                        tx,
                        format!("Step {step} · context compaction…"),
                    );
                    let report = compact_child_context(
                        ctx.session_id,
                        ctx.store,
                        CompactTrigger::Watermark,
                        run,
                        cancel,
                        preserve_latest,
                        pause_pin,
                        matches!(progress, ProgressSink::Background { .. }),
                        step,
                    )
                    .await;
                    match report.outcome {
                        CompactOutcome::Cancelled => {
                            emit_activity_since(
                                ctx,
                                tx,
                                &run.agent_id,
                                &run.history,
                                &tool_edits,
                                None,
                                &mut emitted_up_to,
                            );
                            return SubagentDriveResult {
                                status_line: "cancelled".to_string(),
                                result_text: String::new(),
                                turns: steps_run,
                                completed: false,
                                is_error: false,
                                usage,
                                cancelled: true,
                            };
                        }
                        CompactOutcome::Completed => {
                            // The post-step anchor below describes the rewritten
                            // history; it replaces any anchor from before the
                            // compaction.
                            last_total_tokens = report.used_after;
                            pause_pin = report.pin_after;
                            emitted_up_to = emitted_up_to.min(run.history.len());
                        }
                        CompactOutcome::Skipped | CompactOutcome::Failed => {}
                    }
                }
            }
            let api_started = Instant::now();
            let api_started_ms = crate::model_io::now_ms();
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut replay = None;
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut finish_reason = pig_protocol::FinishReason::Stop;
            let (child_tx, mut child_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut request_config = run.child_config.clone();
            if pig_provider::prompt_cache_key_supported(&run.child_config.base_url) {
                request_config.prompt_cache_key = Some(pig_provider::subagent_prompt_cache_key(
                    ctx.session_id,
                    &run.agent_id,
                ));
            }
            let provider_task = tokio::spawn(pig_provider::stream_chat(
                request_config,
                run.history.clone(),
                run.schemas.clone(),
                child_tx,
                pig_provider::CallControl::new(cancel.clone()),
            ));
            let mut failed: Option<CoreError> = None;
            let mut first_token_at: Option<Instant> = None;
            let mut reasoning_started: Option<Instant> = None;
            let mut reasoning_ms: Option<u64> = None;
            // Provider tool-call id per streaming index (start events carry
            // the id; delta events only carry the index)
            let mut tool_input_ids: HashMap<usize, String> = HashMap::new();
            // Any model-authored content makes a transparent retry unsafe.
            let mut forwarded_content = false;
            let mut api_tracker = crate::api_usage::ApiCallTracker::default();
            loop {
                let event = tokio::select! {
                    event = child_rx.recv() => event,
                    _ = cancel.cancelled() => None,
                };
                if let Some(event) = &event {
                    api_tracker.observe(event);
                }
                match event {
                    // Subagent reasoning/text deltas stream to its own panel;
                    // the parent timeline only gets progress status lines
                    Some(ProviderEvent::Reasoning(delta)) => {
                        first_token_at.get_or_insert_with(Instant::now);
                        reasoning_started.get_or_insert_with(Instant::now);
                        forwarded_content = true;
                        reasoning.push_str(&delta);
                        let agent_id = run.agent_id.clone();
                        emit_bg(ctx.session_id, tx, move |session_id| Event::SubagentDelta {
                            session_id,
                            agent_id,
                            role: "reasoning".to_string(),
                            delta,
                        });
                    }
                    Some(ProviderEvent::Text(delta)) => {
                        first_token_at.get_or_insert_with(Instant::now);
                        if let Some(started) = reasoning_started.take() {
                            reasoning_ms = Some(started.elapsed().as_millis() as u64);
                        }
                        forwarded_content = true;
                        text.push_str(&delta);
                        let agent_id = run.agent_id.clone();
                        emit_bg(ctx.session_id, tx, move |session_id| Event::SubagentDelta {
                            session_id,
                            agent_id,
                            role: "assistant".to_string(),
                            delta,
                        });
                    }
                    Some(ProviderEvent::AssistantReplay(snapshot)) => {
                        forwarded_content = true;
                        replay = Some(snapshot);
                    }
                    Some(ProviderEvent::ToolCalls(calls)) => {
                        if let Some(started) = reasoning_started.take() {
                            reasoning_ms = Some(started.elapsed().as_millis() as u64);
                        }
                        forwarded_content |= !calls.is_empty();
                        tool_calls = calls;
                    }
                    // Live tool-input streaming goes to the subagent panel as
                    // pending tool cards, not to the parent timeline
                    Some(ProviderEvent::ToolCallStart { index, id, name }) => {
                        if let Some(started) = reasoning_started.take() {
                            reasoning_ms = Some(started.elapsed().as_millis() as u64);
                        }
                        forwarded_content = true;
                        tool_input_ids.insert(index, id.clone());
                        let agent_id = run.agent_id.clone();
                        emit_bg(ctx.session_id, tx, move |session_id| {
                            Event::SubagentToolInputStart {
                                session_id,
                                agent_id,
                                item_id: id,
                                tool: name,
                            }
                        });
                    }
                    Some(ProviderEvent::ToolCallDelta { index, delta }) => {
                        if let Some(item_id) = tool_input_ids.get(&index).cloned() {
                            let agent_id = run.agent_id.clone();
                            emit_bg(ctx.session_id, tx, move |session_id| {
                                Event::SubagentToolInputDelta {
                                    session_id,
                                    agent_id,
                                    item_id,
                                    delta,
                                }
                            });
                        }
                    }
                    Some(ProviderEvent::ResponseMetadata(_)) => {}
                    // Subagent retries stay out of the parent timeline; the
                    // tracker handles their reporting bookkeeping.
                    Some(ProviderEvent::Retrying(_)) => {}
                    Some(ProviderEvent::Usage {
                        input,
                        cache_read,
                        output,
                        used,
                        ..
                    }) => {
                        // Token usage is carried out via the return value: the
                        // foreground adds it into the parent turn stats, the
                        // background discards it; no StepUsage recorded /
                        // watermark untouched (the watermark tracks the parent
                        // session's own context)
                        usage.0 += input;
                        usage.1 += cache_read;
                        usage.2 += output;
                        if used > 0 {
                            attempt_used = Some(used);
                        }
                        last_total_tokens = Some(used);
                    }
                    Some(ProviderEvent::Finished(reason)) => {
                        finish_reason = reason;
                        break;
                    }
                    None => {
                        if !cancel.is_cancelled() {
                            failed = Some(CoreError::Internal {
                                detail: "Provider event stream ended without a finish reason"
                                    .to_string(),
                            });
                        }
                        break;
                    }
                    Some(ProviderEvent::Failed(error)) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            let api_duration_ms = api_started.elapsed().as_millis() as u64;
            let mut api_record = crate::api_usage::ApiUsageRecord::new(
                ctx.session_id,
                "subagent",
                &run.child_config,
                api_started_ms,
                api_duration_ms,
            );
            api_record.step = Some(step as u32);
            api_record.agent_id = Some(run.agent_id.clone());
            api_record.background = matches!(progress, ProgressSink::Background { .. });
            api_record.ttft_ms = first_token_at
                .map(|at| at.duration_since(api_started).as_millis() as u64)
                .map(|ttft| ttft.min(api_duration_ms));
            api_record.finish = if failed.is_some() {
                "error"
            } else if cancel.is_cancelled() {
                "cancelled"
            } else {
                finish_reason.as_str()
            }
            .to_string();
            api_record.error = failed.as_ref().map(pig_provider::core_error_en);
            api_tracker.apply_to(&mut api_record);
            ctx.store
                .lock()
                .expect("store lock")
                .record_api_usage(&api_record);

            if let Some(error) = failed {
                provider_task.abort();
                // Before wrapping up, report this step's already-recorded
                // messages to the live panel (same below)
                emit_activity_since(ctx, tx, &run.agent_id, &run.history, &tool_edits, reasoning_ms, &mut emitted_up_to);
                // A stop that raced the error wins: the child is cancelled,
                // not reported as a failure, and no recovery/summary runs.
                if cancel.is_cancelled() {
                    return SubagentDriveResult {
                        status_line: "cancelled".to_string(),
                        result_text: String::new(),
                        turns: steps_run,
                        completed: false,
                        is_error: false,
                        usage,
                        cancelled: true,
                    };
                }
                let overflow = matches!(error, CoreError::ContextWindowExceeded { .. });
                if overflow
                    && !forwarded_content
                    && !cancel.is_cancelled()
                    && !attempt_recovered
                    && overflow_recoveries < MAX_CHILD_OVERFLOW_RECOVERIES
                {
                    tracing::warn!(
                        agent_id = %run.agent_id,
                        step,
                        "subagent context window exceeded before any content was forwarded; compacting and retrying this step once"
                    );
                    // Compare the same estimate before and after: the provider's
                    // reported usage can be lower than the conservative
                    // estimate, so it cannot prove the rewrite freed room.
                    let used_before =
                        crate::session::estimate::estimate_history_tokens(&run.history)
                            + tools_tokens;
                    report_progress(
                        ctx,
                        progress,
                        tx,
                        format!("Step {step} · context compaction…"),
                    );
                    let report = compact_child_context(
                        ctx.session_id,
                        ctx.store,
                        CompactTrigger::Overflow,
                        run,
                        cancel,
                        preserve_latest,
                        pause_pin,
                        matches!(progress, ProgressSink::Background { .. }),
                        step,
                    )
                    .await;
                    match report.outcome {
                        CompactOutcome::Completed => {
                            if let Some(used_after) = report.used_after
                                && used_after < used_before
                            {
                                overflow_recoveries += 1;
                                attempt_recovered = true;
                                usage_anchor = None;
                                last_total_tokens = Some(used_after);
                                pause_pin = report.pin_after;
                                emitted_up_to = emitted_up_to.min(run.history.len());
                                report_progress(
                                    ctx,
                                    progress,
                                    tx,
                                    format!("Step {step} · retrying after compaction…"),
                                );
                                continue 'attempt;
                            }
                            tracing::warn!(
                                agent_id = %run.agent_id,
                                used_before,
                                used_after = ?report.used_after,
                                "child overflow compaction did not shrink the context; surfacing the error"
                            );
                        }
                        CompactOutcome::Cancelled => {
                            emit_activity_since(
                                ctx,
                                tx,
                                &run.agent_id,
                                &run.history,
                                &tool_edits,
                                None,
                                &mut emitted_up_to,
                            );
                            return SubagentDriveResult {
                                status_line: "cancelled".to_string(),
                                result_text: String::new(),
                                turns: steps_run,
                                completed: false,
                                is_error: false,
                                usage,
                                cancelled: true,
                            };
                        }
                        CompactOutcome::Skipped | CompactOutcome::Failed => {
                            tracing::warn!(
                                agent_id = %run.agent_id,
                                step,
                                "child context overflow cannot be compacted (no compactable prefix or the paused state does not fit); surfacing the error"
                            );
                        }
                    }
                }
                if overflow {
                    return child_overflow_failure(
                        &run.agent_id,
                        run.child_config.context_window,
                        &pig_provider::core_error_en(&error),
                        usage,
                        steps_run,
                    );
                }
                let note = format!(
                    "Subagent model request failed: {}",
                    pig_provider::core_error_en(&error)
                );
                return SubagentDriveResult {
                    status_line: format!("failed: {note}"),
                    result_text: note,
                    turns: steps_run,
                    completed: false,
                    is_error: true,
                    usage,
                    cancelled: false,
                };
            }
            if cancel.is_cancelled() {
                provider_task.abort();
                emit_activity_since(ctx, tx, &run.agent_id, &run.history, &tool_edits, reasoning_ms, &mut emitted_up_to);
                return SubagentDriveResult {
                    status_line: "cancelled".to_string(),
                    result_text: String::new(),
                    turns: steps_run,
                    completed: false,
                    is_error: false,
                    usage,
                    cancelled: true,
                };
            }
            let _ = provider_task.await;
            if reasoning_ms.is_none()
                && let Some(started) = reasoning_started.take()
            {
                reasoning_ms = Some(started.elapsed().as_millis() as u64);
            }
            attempt_output = Some((text, reasoning, replay, tool_calls, finish_reason, reasoning_ms));
            break 'attempt;
        }
        let (mut text, reasoning, mut replay, mut tool_calls, finish_reason, reasoning_ms) =
            attempt_output.expect("a successful child attempt always crosses the retry loop");
        let paused = finish_reason == pig_protocol::FinishReason::Pause;
        if finish_reason.is_incomplete() {
            incomplete_reason = Some(finish_reason);
            tool_calls.clear();
            if let Some(replay) = replay.as_mut() {
                replay.remove_tool_calls();
            }
        }
        // Responses can deliver the message only through `output_item.done`;
        // the durable replay carries the authoritative text.
        if text.is_empty()
            && let Some(replay) = replay.as_ref()
        {
            text = replay
                .items
                .iter()
                .filter_map(|item| match item {
                    pig_provider::AssistantReplayItem::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
        }
        let mut assistant = ChatMsg::assistant_with_replay(
            text,
            tool_calls.clone(),
            Some(reasoning).filter(|r| !r.is_empty()),
            replay,
        );
        last_text = assistant.content.clone().unwrap_or_default();
        if let Err(error) = persist_child_message_meta(
            run,
            &mut assistant,
            crate::agent::AgentMsgMeta {
                reasoning_ms,
                edit: None,
            },
        ) {
            return child_persist_stop(run, &error, steps_run, usage);
        }
        run.history.push(assistant);
        // The next request's preflight baseline is this persisted reply step
        // (the tool results below are added as history growth).
        usage_anchor = attempt_used.map(|used| super::UsageAnchor {
            model_key: super::model_key(&run.child_config),
            history_len: run.history.len(),
            tools_tokens,
            used,
            system_fingerprint: super::system_fingerprint(&run.history),
        });
        if tool_calls.is_empty() {
            // This step has only the assistant message: report before wrap-up
            emit_activity_since(ctx, tx, &run.agent_id, &run.history, &tool_edits, reasoning_ms, &mut emitted_up_to);
            if paused {
                if pause_continuations >= MAX_PAUSE_CONTINUATIONS {
                    return subagent_pause_limit(usage, steps_run);
                }
                pause_continuations += 1;
                preserve_latest = true;
                pause_pin = Some(run.history.len().saturating_sub(1));
                continue;
            }
            completed = true;
            break;
        }
        for (call_ix, child_call) in tool_calls.iter().enumerate() {
            report_progress(
                ctx,
                progress,
                tx,
                format!("Step {step} · {}", tool::summarize(child_call)),
            );
            // The provider call id is preserved on the wire and in the child
            // context; the internal item/approval id also carries the call
            // index so duplicate provider ids cannot alias each other.
            let child_item_id = format!("{ownership}-c{step}-{call_ix}-{}", child_call.id);
            let Some(child_tool) = run
                .tools
                .iter()
                .find(|t| t.name() == child_call.name.as_str())
                .map(|t| t.as_ref())
            else {
                // The narrowed tool set has no such name: record an error
                // result and continue (the subagent is not interrupted)
                let note = format!(
                    "Unknown tool {} (the subagent's tool set is narrowed)",
                    child_call.name
                );
                let mut message = ChatMsg::tool_result(&child_call.id, note).with_error(true);
                if let Err(error) = persist_child_message(run, &mut message) {
                    return child_persist_stop(run, &error, steps_run, usage);
                }
                run.history.push(message);
                continue;
            };
            match exec_tool_gated_ctx(
                ctx,
                child_call,
                Some(child_tool),
                &child_item_id,
                &ownership,
                tx,
                cancel,
            )
            .await
            {
                GatedToolOutcome::Cancelled => {
                    for stopped_call in &tool_calls[call_ix..] {
                        let mut message = Session::stopped_tool_result(&stopped_call.id);
                        if let Err(error) = persist_child_message(run, &mut message) {
                            return child_persist_stop(run, &error, steps_run, usage);
                        }
                        run.history.push(message);
                    }
                    emit_activity_since(ctx, tx, &run.agent_id, &run.history, &tool_edits, reasoning_ms, &mut emitted_up_to);
                    return SubagentDriveResult {
                        status_line: "cancelled".to_string(),
                        result_text: String::new(),
                        turns: steps_run,
                        completed: false,
                        is_error: false,
                        usage,
                        cancelled: true,
                    };
                }
                GatedToolOutcome::Rejected { note } => {
                    let mut message = ChatMsg::tool_result(&child_call.id, note).with_error(true);
                    if let Err(error) = persist_child_message(run, &mut message) {
                        return child_persist_stop(run, &error, steps_run, usage);
                    }
                    run.history.push(message);
                }
                GatedToolOutcome::Executed {
                    output,
                    is_error,
                    edit,
                    images,
                    documents,
                    ..
                } => {
                    let mut message =
                        ChatMsg::tool_result_with_images(&child_call.id, output, images)
                            .with_error(is_error);
                    message.documents = documents;
                    if let Some(edit) = edit.clone() {
                        tool_edits.insert(child_call.id.clone(), edit);
                    }
                    if let Err(error) = persist_child_message_meta(
                        run,
                        &mut message,
                        crate::agent::AgentMsgMeta {
                            reasoning_ms: None,
                            edit,
                        },
                    ) {
                        return child_persist_stop(run, &error, steps_run, usage);
                    }
                    run.history.push(message);
                }
            }
        }
        // End-of-step reporting: this step's assistant (with tool_calls) + all
        // tool results are projected in one batch, with output backfilled by
        // tool_call_id within the batch
        emit_activity_since(ctx, tx, &run.agent_id, &run.history, &tool_edits, reasoning_ms, &mut emitted_up_to);
        if paused {
            if pause_continuations >= MAX_PAUSE_CONTINUATIONS {
                return subagent_pause_limit(usage, steps_run);
            }
            pause_continuations += 1;
            // Keep the paused assistant/server state (and its completed tool
            // round) through the next request's compaction.
            preserve_latest = true;
            pause_pin = Some(run.history.len().saturating_sub(1));
        } else {
            preserve_latest = false;
            pause_pin = None;
        }
    }
    // ---- wrap-up: return the untruncated full text (result.md persistence
    // and the 32K budget truncation are finalized in the outer
    // drive_subagent) ----
    if completed {
        if last_text.is_empty() {
            SubagentDriveResult {
                status_line: "failed: subagent produced no final text".to_string(),
                result_text: "The subagent produced no final text".to_string(),
                turns: steps_run,
                completed: true,
                is_error: true,
                usage,
                cancelled: false,
            }
        } else if let Some(reason) = incomplete_reason {
            SubagentDriveResult {
                status_line: format!(
                    "completed with incomplete model response ({})",
                    reason.as_str()
                ),
                result_text: format!(
                    "[Model response incomplete: {}]\n\n{last_text}",
                    reason.as_str()
                ),
                turns: steps_run,
                completed: true,
                is_error: false,
                usage,
                cancelled: false,
            }
        } else {
            SubagentDriveResult {
                status_line: format!("completed ({steps_run} steps)"),
                result_text: last_text,
                turns: steps_run,
                completed: true,
                is_error: false,
                usage,
                cancelled: false,
            }
        }
    } else if last_text.is_empty() {
        SubagentDriveResult {
            status_line: format!(
                "failed: reached the maximum turn count ({}), subagent produced no conclusion",
                run.max_turns
            ),
            result_text: format!(
                "Reached the maximum turn count ({}). The subagent produced no conclusion",
                run.max_turns
            ),
            turns: steps_run,
            completed: false,
            is_error: true,
            usage,
            cancelled: false,
        }
    } else {
        SubagentDriveResult {
            status_line: format!("failed: reached the maximum turn count {}", run.max_turns),
            result_text: format!(
                "Reached the maximum turn count ({}). {last_text}",
                run.max_turns
            ),
            turns: steps_run,
            completed: false,
            is_error: true,
            usage,
            cancelled: false,
        }
    }
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
