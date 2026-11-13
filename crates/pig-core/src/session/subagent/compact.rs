use super::*;

use crate::session::compact::{
    CompactOutcome, CompactTrigger, InstructionFlavor, SummaryCall, child_compaction_plan,
    compaction_instruction, compaction_note, install_compaction, summarize_history,
};

/// Reactive typed-overflow recoveries per child drive. Preflight compaction is
/// not counted here: it fires only when the projected request is over the
/// trigger and always omits completed work. A drive whose every retry still
/// overflows must surface the error instead of compacting forever.
pub(super) const MAX_CHILD_OVERFLOW_RECOVERIES: usize = 2;

/// One child compaction attempt's result. `pin_after` remaps a paused
/// assistant index into the rewritten history so a later reactive recovery in
/// the same step still pins the actual server state.
pub(super) struct ChildCompactReport {
    pub(super) outcome: CompactOutcome,
    pub(super) used_after: Option<u64>,
    pub(super) pin_after: Option<usize>,
}

impl ChildCompactReport {
    fn skipped(pin_after: Option<usize>) -> Self {
        Self {
            outcome: CompactOutcome::Skipped,
            used_after: None,
            pin_after,
        }
    }

    fn cancelled(pin_after: Option<usize>) -> Self {
        Self {
            outcome: CompactOutcome::Cancelled,
            used_after: None,
            pin_after,
        }
    }
}

impl Session {
    /// One child compaction attempt, with the paused-state pin remapped into
    /// the rewritten history.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn compact_child_context(
        &mut self,
        trigger: CompactTrigger,
        cancel: &CancellationToken,
        preserve_latest: bool,
        pause_pin: Option<usize>,
        background: bool,
        step: usize,
    ) -> ChildCompactReport {
        compact_child_context(
            self,
            trigger,
            cancel,
            preserve_latest,
            pause_pin,
            background,
            step,
        )
        .await
    }
}

/// Persist the canonical child compaction marker and rewrite the live history.
/// The marker is written before the in-memory rewrite: a failed append leaves
/// both live and durable contexts on the pre-compaction shape instead of
/// silently diverging. Media never enters the marker (message lines keep their
/// refs), so the next resume re-reads the exact retained messages and applies
/// the current model's capability projection.
#[allow(clippy::too_many_arguments)]
pub(super) async fn compact_child_context(
    session: &mut Session,
    trigger: CompactTrigger,
    cancel: &CancellationToken,
    preserve_latest: bool,
    pause_pin: Option<usize>,
    background: bool,
    step: usize,
) -> ChildCompactReport {
    if cancel.is_cancelled() {
        return ChildCompactReport::cancelled(pause_pin);
    }
    let tools_tokens = crate::session::estimate::estimate_schemas_tokens(&session.agent.schemas);
    let used_before =
        crate::session::estimate::estimate_history_tokens(&session.agent.history) + tools_tokens;
    let Some(plan) = child_compaction_plan(
        &session.agent.history,
        session.agent.config(),
        tools_tokens,
        trigger,
        preserve_latest,
        pause_pin,
    ) else {
        if preserve_latest {
            tracing::warn!(
                agent_id = %session.agent.id,
                pause_pin = ?pause_pin,
                "child preserve-latest compaction cannot retain the paused assistant/server state and the steer within the budget; skipping"
            );
        }
        return ChildCompactReport::skipped(pause_pin);
    };

    let mut summary_config = session.agent.config().clone();
    if pig_provider::prompt_cache_key_supported(&session.agent.config().base_url) {
        summary_config.prompt_cache_key = Some(pig_provider::subagent_prompt_cache_key(
            session.event_session_id(),
            &session.agent.id,
        ));
    }
    let instruction = compaction_instruction(InstructionFlavor::Child, None);
    let summary_result = summarize_history(
        &SummaryCall {
            session_id: session.event_session_id(),
            store: &session.store,
            agent_id: Some(&session.agent.id),
            background,
            step: Some(step),
        },
        &summary_config,
        &session.agent.history,
        plan.tail_start,
        &instruction,
        &session.agent.schemas,
        cancel,
    )
    .await;
    if summary_result.cancelled || cancel.is_cancelled() {
        return ChildCompactReport::cancelled(pause_pin);
    }
    if let Some(error) = summary_result.error {
        // Automatic compaction deliberately falls back to the note-only
        // truncation after the provider's bounded transient retries and the
        // one typed-overflow retry. There is no manual child compaction path.
        tracing::warn!(
            agent_id = %session.agent.id,
            "child summary request failed; automatic compaction uses the note-only truncation fallback: {error:?}"
        );
    }
    let summary = summary_result.summary;

    let note = compaction_note(plan.omitted, true, summary.as_deref(), None);
    let installed = match install_compaction(&session.agent.history, plan, note, tools_tokens) {
        Ok(installed) => installed,
        Err(error) => {
            tracing::warn!(
                agent_id = %session.agent.id,
                "child compaction plan does not match the live history: {error}; skipping"
            );
            return ChildCompactReport::skipped(pause_pin);
        }
    };
    let record = crate::rollout::RolloutRecord::Compact {
        note: installed.note.clone(),
        omitted: installed.omitted,
        automatic: true,
        summary: summary.clone(),
        tail_len: Some(installed.tail_len),
        preserved_user: installed.preserved_user,
        trigger: Some(
            if trigger == CompactTrigger::Overflow {
                "overflow"
            } else {
                "watermark"
            }
            .to_string(),
        ),
        used_before: Some(used_before),
        used_after: Some(installed.used_after),
    };
    if let Err(error) = crate::agent::append_agent_conversation_record(
        &session.subagent.as_ref().expect("child session").jsonl,
        &record,
    ) {
        tracing::error!(
            agent_id = %session.agent.id,
            "failed to persist the subagent compaction marker; keeping the previous context: {error}"
        );
        // The failed append is sticky: the driver must not continue appending
        // messages to a context file that may hold a torn marker line. Live
        // and durable history are still identical here (no rewrite happened);
        // the next message persist stops the child with resume guidance.
        session
            .subagent
            .as_mut()
            .expect("child session")
            .persist_failure = Some(error.clone());
        return ChildCompactReport::skipped(pause_pin);
    }
    // The marker is the child's durable compaction boundary: flush it before
    // the in-memory rewrite can be followed by more messages.
    if let Err(error) = std::fs::OpenOptions::new()
        .write(true)
        .open(&session.subagent.as_ref().expect("child session").jsonl)
        .and_then(|file| file.sync_all())
    {
        tracing::warn!(
            agent_id = %session.agent.id,
            "failed to sync the subagent compaction marker: {error}"
        );
    }
    let pin_after = installed.pin_after(pause_pin);
    session.agent.history = installed.history;
    tracing::info!(
        agent_id = %session.agent.id,
        trigger = ?trigger,
        omitted = installed.omitted,
        tail_len = installed.tail_len,
        preserved_user = ?installed.preserved_user,
        summarized = summary.is_some(),
        "compacted the subagent context"
    );
    ChildCompactReport {
        outcome: CompactOutcome::Completed,
        used_after: Some(installed.used_after),
        pin_after,
    }
}

/// Unrecoverable child overflow: the model window is smaller than the retained
/// context even after bounded compaction. The child identity and a concrete
/// resume path stay in the model-facing failure text (the Agent receipt and
/// background notification add their own agent_id line too).
pub(super) fn child_overflow_failure(
    agent_id: &str,
    context_window: u64,
    detail: &str,
    usage: (u64, u64, u64),
    turns: usize,
) -> SubagentResult {
    let note = format!(
        "Subagent {agent_id} exceeded its model context window ({context_window} tokens) and automatic compaction could not free enough room: {detail}\n\
         The full child context is saved. Continue with Agent(resume=\"{agent_id}\", prompt=\"...\") using a shorter prompt after reducing the task scope, or switch this subagent's profile/model to a larger context window and resume it."
    );
    SubagentResult {
        status_line: "failed: child context overflow".to_string(),
        result_text: note,
        turns,
        completed: false,
        is_error: true,
        usage,
        cancelled: false,
    }
}
