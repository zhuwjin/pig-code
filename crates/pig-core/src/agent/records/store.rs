use super::*;

use crate::rollout::ConversationStore as _;
use std::collections::VecDeque;

use super::compact::{AgentCompact, apply_agent_compact};
use super::media::rehydrate_agent_media;
use crate::rollout::{ConversationKind, RolloutRecord};

/// First-line meta record of a subagent context JSONL.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentMeta {
    pub agent_id: String,
    pub parent_session_id: String,
    pub delegation_depth: u32,
    pub context_mode: crate::rollout::ChildContextMode,
    pub inherited_record_count: Option<usize>,
    pub profile: String,
    pub description: String,
    pub model: String,
    pub provider: String,
    pub created_at: u64,
}

fn required_meta<T>(value: Option<T>, field: &str, path: &Path) -> Result<T, String> {
    value.ok_or_else(|| {
        format!(
            "Subagent context {} is missing required meta field {field}",
            path.display()
        )
    })
}

/// Read a subagent context: first-line meta + canonical conversation records.
/// Compaction markers rewrite the history at complete-boundary indexes, exactly
/// like the root rollout rebuild. Media is rehydrated only for messages that
/// survive every marker, through the owning child media directory
/// (`{jsonl}.media`), so a tampered or corrupt ref cannot point the model at
/// another file. Bad lines error with the line number; a torn partial final
/// line (a crash during append) keeps the last valid prefix and is repaired
/// before the next append.
pub fn read_agent(path: &Path) -> Result<(AgentMeta, Vec<pig_provider::ChatMsg>), String> {
    let (meta, stored) = read_agent_stored(path)?;
    let media_root = AgentConversationStore::new(path).media_root();
    let mut history = Vec::with_capacity(stored.len());
    for mut item in stored {
        rehydrate_agent_media(&mut item, &media_root);
        history.push(item.msg);
    }
    reconcile_tool_results(&mut history);
    Ok((meta, history))
}

/// Read a subagent context with its display metadata, before media rehydration.
pub(crate) fn read_agent_stored(path: &Path) -> Result<(AgentMeta, Vec<StoredAgentMsg>), String> {
    let store = AgentConversationStore::new(path);
    let records = store.load().map_err(|error| {
        format!(
            "Failed to read subagent context {}: {}",
            path.display(),
            pig_provider::core_error_en(&error)
        )
    })?;
    let mut records = records.into_iter();
    let Some(first) = records.next() else {
        return Err(format!(
            "Subagent context {} is empty: missing meta line",
            path.display()
        ));
    };
    let RolloutRecord::Meta {
        id,
        created_at,
        kind,
        parent_id,
        delegation_depth,
        context_mode,
        inherited_record_count,
        profile,
        description,
        provider,
        model,
        ..
    } = first
    else {
        return Err("Subagent context line 1 is not a meta record".to_string());
    };
    if kind != ConversationKind::Subagent {
        return Err(format!(
            "Subagent context {} is not a subagent conversation",
            path.display()
        ));
    }
    let meta = AgentMeta {
        agent_id: id,
        parent_session_id: required_meta(parent_id, "parent_id", path)?,
        delegation_depth: required_meta(delegation_depth, "delegation_depth", path)?,
        context_mode: required_meta(context_mode, "context_mode", path)?,
        inherited_record_count,
        profile: required_meta(profile, "profile", path)?,
        description: required_meta(description, "description", path)?,
        model: required_meta(model, "model", path)?,
        provider: required_meta(provider, "provider", path)?,
        created_at,
    };
    let mut stored: Vec<StoredAgentMsg> = Vec::new();
    let mut pending_calls: VecDeque<String> = VecDeque::new();
    for (index, value) in records.enumerate() {
        let line_no = index + 2;
        match value {
            RolloutRecord::Meta { .. } => {}
            RolloutRecord::System { text } => stored.push(message(
                ChatMsg::system(text),
                Vec::new(),
                Vec::new(),
                0,
                None,
                None,
            )),
            RolloutRecord::PromptSnapshot { snapshot } => stored.push(message(
                ChatMsg::system(snapshot.text),
                Vec::new(),
                Vec::new(),
                0,
                None,
                None,
            )),
            RolloutRecord::User {
                text,
                images,
                sent_at_ms,
                ..
            } => stored.push(message(
                ChatMsg::user(text),
                images,
                Vec::new(),
                sent_at_ms,
                None,
                None,
            )),
            RolloutRecord::TaskNotificationBatch {
                model_text,
                sent_at_ms,
                ..
            } => stored.push(message(
                ChatMsg::user(model_text),
                Vec::new(),
                Vec::new(),
                sent_at_ms,
                None,
                None,
            )),
            RolloutRecord::Assistant {
                text,
                reasoning,
                replay,
                tool_calls,
                ts_ms,
                reasoning_ms,
            } => {
                let calls: Vec<pig_provider::ToolCall> = tool_calls
                    .iter()
                    .map(|wire| pig_provider::ToolCall {
                        id: wire.id.clone(),
                        name: wire.function.name.clone(),
                        arguments: wire.function.arguments.clone(),
                    })
                    .collect();
                pending_calls.extend(calls.iter().map(|call| call.id.clone()));
                stored.push(message(
                    ChatMsg::assistant_with_replay(text, calls, reasoning, replay),
                    Vec::new(),
                    Vec::new(),
                    ts_ms,
                    reasoning_ms,
                    None,
                ));
            }
            RolloutRecord::ToolCall {
                tool: _,
                arguments: _,
                output,
                is_error,
                stopped,
                edit,
                images,
                documents,
                ..
            } => {
                let call_id = pending_calls
                    .pop_front()
                    .unwrap_or_else(|| format!("replay-{}", stored.len()));
                let mut msg = ChatMsg::tool_result(&call_id, output).with_error(is_error);
                msg.stopped = stopped;
                stored.push(message(msg, images, documents, 0, None, edit));
            }
            RolloutRecord::Compact {
                note,
                omitted,
                automatic,
                used_after,
                used_before,
                summary,
                tail_len,
                preserved_user,
                trigger,
            } => {
                let compact = AgentCompact {
                    note,
                    omitted,
                    automatic,
                    summary,
                    tail_len: tail_len.unwrap_or(0),
                    preserved_user,
                    trigger,
                    used_before,
                    used_after,
                };
                apply_agent_compact(&mut stored, &compact, line_no);
                pending_calls.clear();
            }
            RolloutRecord::Reasoning { text, replay } => stored.push(message(
                ChatMsg::assistant_with_replay(String::new(), Vec::new(), Some(text), replay),
                Vec::new(),
                Vec::new(),
                0,
                None,
                None,
            )),
            RolloutRecord::Text { text } => stored.push(message(
                ChatMsg::assistant(text, Vec::new(), None),
                Vec::new(),
                Vec::new(),
                0,
                None,
                None,
            )),
            RolloutRecord::TurnChanges { .. }
            | RolloutRecord::HookContext { .. }
            | RolloutRecord::TurnStats { .. }
            | RolloutRecord::StepUsage { .. }
            | RolloutRecord::Recall => {}
        }
    }
    Ok((meta, stored))
}

fn message(
    msg: pig_provider::ChatMsg,
    images: Vec<crate::rollout::ImageRef>,
    documents: Vec<crate::rollout::DocumentRef>,
    ts_ms: u64,
    reasoning_ms: Option<u64>,
    edit: Option<pig_protocol::EditDiff>,
) -> StoredAgentMsg {
    StoredAgentMsg {
        msg,
        images,
        documents,
        ts_ms,
        reasoning_ms,
        edit,
    }
}

pub(crate) fn reconcile_tool_results(history: &mut [pig_provider::ChatMsg]) {
    let mut index = 0;
    while index < history.len() {
        if history[index].role != "assistant" {
            index += 1;
            continue;
        }
        let mut result_index = index + 1;
        let mut answered = std::collections::HashSet::new();
        while result_index < history.len() && history[result_index].role == "tool" {
            if let Some(id) = &history[result_index].tool_call_id {
                answered.insert(id.clone());
            }
            result_index += 1;
        }
        let msg = &mut history[index];
        let clear_calls = if let Some(calls) = msg.tool_calls.as_mut() {
            calls.retain(|call| answered.contains(&call.id));
            calls.is_empty()
        } else {
            false
        };
        if clear_calls {
            msg.tool_calls = None;
        }
        if let Some(replay) = msg.replay.as_mut() {
            replay.items.retain(|item| match item {
                pig_provider::AssistantReplayItem::ToolCall { call } => answered.contains(&call.id),
                _ => true,
            });
        }
        index = result_index;
    }
}
