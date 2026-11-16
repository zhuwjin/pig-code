use super::*;

#[cfg(test)]
const TEST_CONTEXT_WINDOW: u64 = 128_000;

#[cfg(test)]
pub(super) fn persist_agent_msg(jsonl: &Path, msg: &mut ChatMsg) {
    if let Err(error) = crate::agent::append_agent_message(jsonl, msg, TEST_CONTEXT_WINDOW) {
        tracing::error!("failed to persist subagent message: {error}");
    }
}

#[cfg(test)]
pub(super) fn persist_agent_meta(
    jsonl: &Path,
    agent_id: &str,
    parent_id: &str,
    profile: &str,
    description: &str,
) {
    let record = crate::rollout::RolloutRecord::subagent_meta(
        agent_id.to_string(),
        parent_id.to_string(),
        std::env::temp_dir(),
        description.to_string(),
        profile.to_string(),
        "p".to_string(),
        "m".to_string(),
        1,
        1,
    );
    if let Err(error) = crate::agent::append_agent_conversation_record(jsonl, &record) {
        tracing::error!("failed to persist subagent meta: {error}");
    }
}

#[cfg(test)]
pub(super) fn persist_agent_tool(jsonl: &Path, call: &pig_provider::ToolCall, msg: &mut ChatMsg) {
    if let Err(error) = crate::agent::append_agent_tool_result(
        jsonl,
        call,
        &format!("execution-{}", call.id),
        msg,
        TEST_CONTEXT_WINDOW,
        crate::agent::AgentMsgMeta::default(),
    ) {
        tracing::error!("failed to persist subagent tool result: {error}");
    }
}

/// Persist one child message with sticky failure semantics: the first append
/// failure is remembered and every later append fails fast, so the driver
/// never advances live history the JSONL cannot reproduce.
pub(super) fn persist_child_message(
    session: &mut Session,
    msg: &mut ChatMsg,
) -> Result<(), String> {
    persist_child_message_meta(session, msg, crate::agent::AgentMsgMeta::default())
}

/// Persist a child message with display metadata and assistant execution ids.
pub(super) fn persist_child_message_meta(
    session: &mut Session,
    msg: &mut ChatMsg,
    meta: crate::agent::AgentMsgMeta,
) -> Result<(), String> {
    let (jsonl, context_window, previous) = {
        let sub = session.subagent.as_ref().expect("child session");
        (
            sub.jsonl.clone(),
            session.agent.config().context_window,
            sub.persist_failure.clone(),
        )
    };
    if let Some(error) = previous {
        return Err(error);
    }
    match crate::agent::append_agent_message_meta(&jsonl, msg, context_window, meta) {
        Ok(()) => Ok(()),
        Err(error) => {
            session
                .subagent
                .as_mut()
                .expect("child session")
                .persist_failure = Some(error.clone());
            Err(error)
        }
    }
}

pub(super) fn persist_child_tool_starts(
    session: &mut Session,
    calls: &[pig_provider::ToolCall],
    execution_ids: &[String],
    start: usize,
    end: usize,
) -> Result<(), String> {
    let (jsonl, previous) = {
        let sub = session.subagent.as_ref().expect("child session");
        (sub.jsonl.clone(), sub.persist_failure.clone())
    };
    if let Some(error) = previous {
        return Err(error);
    }
    let result = (|| {
        for record in crate::rollout::tool_execution_start_records(calls, execution_ids, start, end)
        {
            crate::agent::append_agent_conversation_record(&jsonl, &record)?;
        }
        crate::agent::sync_agent_context(&jsonl)
    })();
    if let Err(error) = result {
        session
            .subagent
            .as_mut()
            .expect("child session")
            .persist_failure = Some(error.clone());
        return Err(error);
    }
    Ok(())
}

pub(super) fn child_agent_result_meta(
    session: &mut Session,
    call_id: &str,
    agent_card: Option<crate::rollout::AgentCardRecord>,
    agent_cards: Vec<crate::rollout::AgentCardRecord>,
) -> crate::agent::AgentMsgMeta {
    let cards: Vec<_> = agent_card
        .iter()
        .chain(agent_cards.iter())
        .cloned()
        .collect();
    if !cards.is_empty() {
        session
            .subagent
            .as_mut()
            .expect("child session")
            .tool_agents
            .insert(call_id.to_string(), cards);
    }
    crate::agent::AgentMsgMeta {
        agent_card,
        agent_cards,
        ..Default::default()
    }
}

/// Persist one tool result with the call that produced it.
pub(super) fn persist_child_tool_result(
    session: &mut Session,
    call: &pig_provider::ToolCall,
    execution_id: &str,
    msg: &mut ChatMsg,
    meta: crate::agent::AgentMsgMeta,
) -> Result<(), String> {
    let (jsonl, context_window, previous) = {
        let sub = session.subagent.as_ref().expect("child session");
        (
            sub.jsonl.clone(),
            session.agent.config().context_window,
            sub.persist_failure.clone(),
        )
    };
    if let Some(error) = previous {
        return Err(error);
    }
    match crate::agent::append_agent_tool_result(
        &jsonl,
        call,
        execution_id,
        msg,
        context_window,
        meta,
    ) {
        Ok(()) => Ok(()),
        Err(error) => {
            session
                .subagent
                .as_mut()
                .expect("child session")
                .persist_failure = Some(error.clone());
            Err(error)
        }
    }
}

/// Terminal child result for a sticky context-write failure.
pub(super) fn child_persist_stop(
    session: &Session,
    error: &str,
    turns: usize,
    usage: (u64, u64, u64),
) -> SubagentResult {
    SubagentResult::child(
        crate::session::AgentRunStatus::Failed,
        "subagent context write failed".to_string(),
        crate::agent::child_persist_failure_note(
            &session.agent.id,
            error,
            true,
            crate::agent::ChildPersistPhase::Stopped,
        ),
        turns,
        usage,
    )
}
