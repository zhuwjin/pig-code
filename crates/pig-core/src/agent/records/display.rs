use super::*;

use pig_protocol::{ConversationItem, ConversationToolStatus};

/// Subagent context -> projection for the shared conversation wire:
/// returns (title=meta.description, subtitle="{provider} · {model}", items).
/// user -> task line; assistant -> reasoning line (when present) + body line
/// + one tool line per tool call (summary from tool::summarize); tool results
/// backfill the matching tool line's output and edit by tool_call_id (output
/// truncated to 2000 chars at a char boundary); system is skipped.
/// The tool row's status comes from the result message's execution status.
pub fn display_items(
    meta: &AgentMeta,
    msgs: &[pig_provider::ChatMsg],
) -> (String, String, Vec<ConversationItem>) {
    (
        meta.description.clone(),
        subtitle(meta),
        project_display_items(msgs),
    )
}

/// The stored variant used by the history loader: it keeps the record's display
/// metadata (timestamps, reasoning duration, tool edits and nested-agent cards).
pub(crate) fn display_stored_items(
    meta: &AgentMeta,
    stored: &[StoredAgentMsg],
) -> (String, String, Vec<ConversationItem>) {
    (
        meta.description.clone(),
        subtitle(meta),
        project_stored_items(stored, &HashMap::new(), 0),
    )
}

fn subtitle(meta: &AgentMeta) -> String {
    format!("{} · {}", meta.provider, meta.model)
}

/// A batch of messages -> conversation items (shared by full loading and live
/// projection). Tool results backfill the matching tool line's output
/// by tool_call_id **within this batch** — in the incremental case a batch =
/// one step's assistant (with tool_calls) + the tool results that immediately
/// follow, so matching naturally lands inside the batch.
pub fn project_display_items(msgs: &[pig_provider::ChatMsg]) -> Vec<ConversationItem> {
    project_display_items_with_edits(msgs, &HashMap::new(), 0)
}

/// The live path's variant: tool edits and nested-agent cards are carried by
/// the drive's call-id maps until their durable result record is projected.
pub(crate) fn project_display_items_with_edits(
    msgs: &[pig_provider::ChatMsg],
    edits: &HashMap<String, pig_protocol::EditDiff>,
    base: usize,
) -> Vec<ConversationItem> {
    project_display_items_with_metadata(msgs, edits, &HashMap::new(), base)
}

pub(crate) fn project_display_items_with_metadata(
    msgs: &[pig_provider::ChatMsg],
    edits: &HashMap<String, pig_protocol::EditDiff>,
    agent_cards: &HashMap<String, Vec<crate::rollout::AgentCardRecord>>,
    base: usize,
) -> Vec<ConversationItem> {
    let stored: Vec<StoredAgentMsg> = msgs
        .iter()
        .map(|msg| {
            let agent_cards = msg
                .tool_call_id
                .as_ref()
                .and_then(|id| agent_cards.get(id))
                .cloned()
                .unwrap_or_default();
            StoredAgentMsg {
                msg: msg.clone(),
                images: Vec::new(),
                documents: Vec::new(),
                ts_ms: 0,
                reasoning_ms: None,
                edit: None,
                agent_card: None,
                agent_cards,
            }
        })
        .collect();
    project_stored_items(&stored, edits, base)
}

fn project_stored_items(
    stored: &[StoredAgentMsg],
    edits: &HashMap<String, pig_protocol::EditDiff>,
    base: usize,
) -> Vec<ConversationItem> {
    let mut items: Vec<ConversationItem> = Vec::new();
    // Tool-row indexes indexed by call id: for backfilling output from tool result messages
    let mut tool_row_by_call: HashMap<String, usize> = HashMap::new();
    for (index, entry) in stored.iter().enumerate() {
        let row = base + index;
        let msg = &entry.msg;
        match msg.role.as_str() {
            "user" => items.push(ConversationItem::User {
                id: format!("u{row}"),
                text: msg.content.clone().unwrap_or_default(),
                files: Vec::new(),
                image_nums: Vec::new(),
                sent_at_ms: entry.ts_ms,
            }),
            "assistant" => {
                if let Some(reasoning) = msg.reasoning.as_ref().filter(|text| !text.is_empty()) {
                    items.push(ConversationItem::Reasoning {
                        id: format!("r{row}"),
                        text: reasoning.clone(),
                        streaming: false,
                        turn_id: None,
                        started_at_ms: None,
                        duration_ms: entry.reasoning_ms,
                        sources: Vec::new(),
                    });
                }
                if let Some(text) = msg.content.as_ref().filter(|t| !t.is_empty()) {
                    let sources = msg
                        .replay
                        .as_ref()
                        .map(pig_provider::replay_sources)
                        .unwrap_or_default();
                    items.push(ConversationItem::Markdown {
                        id: format!("a{row}"),
                        text: text.clone(),
                        streaming: false,
                        turn_id: None,
                        started_at_ms: None,
                        duration_ms: None,
                        sources,
                    });
                }
                for (call_index, wire) in msg.tool_calls.iter().flatten().enumerate() {
                    let call = pig_provider::ToolCall {
                        id: wire.id.clone(),
                        name: wire.function.name.clone(),
                        arguments: wire.function.arguments.clone(),
                    };
                    let id = format!("t{row}-{call_index}");
                    tool_row_by_call.insert(call.id.clone(), items.len());
                    items.push(ConversationItem::Tool {
                        id,
                        tool: call.name.clone(),
                        summary: crate::tool::summarize(&call),
                        detail: call.arguments.clone(),
                        output: String::new(),
                        status: ConversationToolStatus::Done,
                        turn_id: None,
                        edit: edits.get(&call.id).cloned(),
                        live_note: None,
                        agents: Vec::new(),
                        approval_pending: false,
                    });
                }
            }
            "tool" => {
                let output: String = msg
                    .content
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(2000)
                    .collect();
                if let Some(ix) = msg
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| tool_row_by_call.get(id))
                {
                    if let ConversationItem::Tool {
                        output: current,
                        status,
                        edit,
                        agents,
                        ..
                    } = &mut items[*ix]
                    {
                        *current = output;
                        *status = if msg.stopped {
                            ConversationToolStatus::Stopped
                        } else if msg.is_error {
                            ConversationToolStatus::Error
                        } else {
                            ConversationToolStatus::Done
                        };
                        *edit = entry.edit.clone().or_else(|| {
                            msg.tool_call_id
                                .as_ref()
                                .and_then(|id| edits.get(id))
                                .cloned()
                        });
                        *agents = entry
                            .agent_card
                            .iter()
                            .chain(entry.agent_cards.iter())
                            .enumerate()
                            .map(|(index, card)| pig_protocol::ConversationAgentCard {
                                agent_id: card.agent_id.clone(),
                                profile: card.profile.clone(),
                                description: card.description.clone(),
                                model: card.model.clone(),
                                background: card.background,
                                finished: true,
                                finished_seq: Some(index as u64 + 1),
                                live_note: None,
                            })
                            .collect();
                    }
                }
                // No matching call (truncated/out-of-order history): drop the result
            }
            // system prompts are not displayed
            _ => {}
        }
    }
    items
}
