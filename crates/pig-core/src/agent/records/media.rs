use super::*;

use crate::rollout::RolloutRecord;

use super::jsonl::append_agent_conversation_record;

/// Append a deterministic degradation note to a message's model-facing text.
fn append_note(msg: &mut ChatMsg, note: &str) {
    msg.content.get_or_insert_with(String::new).push_str(note);
}

/// Append the exact model-facing message while keeping media bytes out of
/// JSONL. A media-write failure mutates `msg` in place (clearing the bytes and
/// appending the same note written to disk), so callers persist first and then
/// push the returned live message: the live context always equals the durable
/// record.
pub fn append_agent_message(
    path: &Path,
    msg: &mut ChatMsg,
    context_window: u64,
) -> Result<(), String> {
    let tool_execution_ids = msg.tool_calls.as_ref().map_or_else(Vec::new, |calls| {
        calls
            .iter()
            .map(|call| format!("execution-{}", call.id))
            .collect()
    });
    append_agent_message_meta(
        path,
        msg,
        context_window,
        super::AgentMsgMeta {
            tool_execution_ids,
            ..Default::default()
        },
    )
}

/// Persist a message's media bytes and return the references stored in the
/// canonical record. A media-write failure mutates `msg` in place, so callers
/// persist first and then push the returned live message.
fn persist_message_media(
    path: &Path,
    msg: &mut ChatMsg,
    context_window: u64,
) -> (
    Vec<crate::rollout::ImageRef>,
    Vec<crate::rollout::DocumentRef>,
) {
    let media_dir = path.with_extension("media");
    let images = match crate::rollout::persist_chat_images(&media_dir, &msg.images) {
        Ok(images) => images,
        Err(error) => {
            tracing::warn!(
                "failed to persist subagent images; storing the message without them: {error}"
            );
            let count = msg.images.len();
            msg.images.clear();
            append_note(
                msg,
                &format!(
                    "\n\n[{count} image(s) could not be saved and were removed from this tool result]"
                ),
            );
            Vec::new()
        }
    };
    let documents = match crate::rollout::persist_chat_documents(
        &media_dir,
        &mut msg.documents,
        context_window,
    ) {
        Ok(documents) => documents,
        Err(error) => {
            tracing::warn!(
                "failed to persist subagent documents; storing the message without them: {error}"
            );
            let count = msg.documents.len();
            msg.documents.clear();
            append_note(
                msg,
                &format!(
                    "\n\n[{count} PDF document(s) could not be saved and were removed from this tool result]"
                ),
            );
            Vec::new()
        }
    };
    (images, documents)
}

pub fn append_agent_mailbox_message(
    path: &Path,
    message_id: String,
    notification_id: Option<String>,
    text: String,
) -> Result<(), String> {
    super::append_agent_conversation_record(
        path,
        &RolloutRecord::MailboxMessage {
            message_id,
            notification_id,
            text,
            sent_at_ms: crate::model_io::now_ms(),
        },
    )?;
    super::sync_agent_context(path)
}

/// Append one message with its display metadata and, for an assistant, the
/// execution ids parallel to its tool calls.
pub fn append_agent_message_meta(
    path: &Path,
    msg: &mut ChatMsg,
    context_window: u64,
    meta: super::AgentMsgMeta,
) -> Result<(), String> {
    let ts_ms = crate::model_io::now_ms();
    let text = msg.content.clone().unwrap_or_default();
    let record = match msg.role.as_str() {
        "system" => RolloutRecord::System { text },
        "user" => {
            let (images, _) = persist_message_media(path, msg, context_window);
            RolloutRecord::User {
                text: text.clone(),
                files: Vec::new(),
                images: images.clone(),
                sent_at_ms: ts_ms,
                sent_text: Some(text),
                sent_images: images,
                model_text: None,
                sent_input_image: None,
            }
        }
        "assistant" => RolloutRecord::Assistant {
            text,
            reasoning: msg.reasoning.clone(),
            replay: msg.replay.clone(),
            tool_calls: msg.tool_calls.clone().unwrap_or_default(),
            tool_execution_ids: meta.tool_execution_ids,
            ts_ms,
            reasoning_ms: meta.reasoning_ms,
        },
        "tool" => {
            return Err(
                "a tool result needs its ToolCall; use append_agent_tool_result".to_string(),
            );
        }
        role => return Err(format!("unsupported subagent message role {role}")),
    };
    append_agent_conversation_record(path, &record)
}

/// Append a tool result as the canonical `tool_call` record, pairing it with
/// the assistant call that produced it.
pub fn append_agent_tool_result(
    path: &Path,
    call: &pig_provider::ToolCall,
    execution_id: &str,
    msg: &mut ChatMsg,
    context_window: u64,
    meta: super::AgentMsgMeta,
) -> Result<(), String> {
    let (images, documents) = persist_message_media(path, msg, context_window);
    let record = RolloutRecord::ToolCall {
        execution_id: execution_id.to_string(),
        tool: call.name.clone(),
        summary: crate::tool::summarize(call),
        arguments: call.arguments.clone(),
        output: msg.content.clone().unwrap_or_default(),
        is_error: msg.is_error,
        stopped: msg.stopped,
        edit: meta.edit,
        agent_card: meta.agent_card,
        agent_cards: meta.agent_cards,
        images,
        documents,
        delivered_notification_ids: Vec::new(),
        recovery: None,
    };
    append_agent_conversation_record(path, &record)
}

/// Apply the current child model's media capabilities to an already-rehydrated
/// resume history: unsupported images/documents are removed per message and
/// replaced with the same deterministic capability notes the root replay uses.
/// The durable JSONL is left untouched, so a later capable resume can rehydrate
/// the references again.
pub(crate) fn project_history_media(
    history: &mut [ChatMsg],
    media: crate::tool::MediaCapabilities,
) -> (usize, usize) {
    let mut images = 0;
    let mut documents = 0;
    for msg in history {
        let image_count = msg.images.len();
        if image_count > 0 && !media.input_image {
            msg.images.clear();
            append_note(
                msg,
                &format!(
                    "\n\n[{image_count} image(s) were not sent with this message: the current model does not support image input]"
                ),
            );
            images += image_count;
        }
        let document_count = msg.documents.len();
        if document_count > 0 && !media.input_pdf {
            msg.documents.clear();
            append_note(
                msg,
                &format!(
                    "\n\n[{document_count} PDF document(s) were not sent with this message: the current model does not support PDF input]"
                ),
            );
            documents += document_count;
        }
    }
    (images, documents)
}

/// Rehydrate one retained message's media through the child-owned media dir.
/// Missing, corrupt, oversized, type-mismatched or out-of-root refs degrade to
/// the deterministic "no longer available" note (the durable ref is untouched
/// for a later resume).
pub(super) fn rehydrate_agent_media(item: &mut StoredAgentMsg, media_root: &Path) {
    if !item.images.is_empty() {
        let refs = std::mem::take(&mut item.images);
        let rehydrated: Vec<_> = refs
            .iter()
            .filter_map(|image_ref| crate::rollout::rehydrate_image(image_ref, media_root))
            .collect();
        let missing = refs.len() - rehydrated.len();
        item.msg.images = rehydrated;
        if missing > 0 {
            append_note(
                &mut item.msg,
                &format!("\n\n[{missing} image(s) no longer available]"),
            );
        }
    }
    if !item.documents.is_empty() {
        let refs = std::mem::take(&mut item.documents);
        // The child model may still be unresolved at this point; new refs
        // carry their token cost, and the resume path reprices a window
        // fallback (or prices old refs once) when the current window is known.
        let rehydrated = crate::rollout::rehydrate_documents(&refs, media_root, None);
        let missing = refs.len() - rehydrated.len();
        item.msg.documents = rehydrated;
        if missing > 0 {
            append_note(
                &mut item.msg,
                &format!("\n\n[{missing} PDF document(s) no longer available]"),
            );
        }
    }
}

/// Align resumed PDF costs with the now-resolved child window. A measured
/// page-tree cost is window-independent and is never decoded or scanned again;
/// a window fallback is repriced from its tag; only a document without any
/// usable cache is decoded and scanned once. The result is installed on the
/// live message so later estimates never touch the payload.
pub(crate) fn fill_document_token_estimates(history: &mut [ChatMsg], context_window: u64) {
    for msg in history {
        for document in &mut msg.documents {
            match document.token_estimate {
                Some(cost) if cost.is_window_fallback() => {
                    document.token_estimate =
                        Some(pig_provider::DocumentTokenEstimate::window_fallback(
                            pig_utils::pdf::unknown_document_token_reserve(context_window),
                            context_window,
                        ));
                }
                Some(_) => {}
                None => {
                    document.token_estimate =
                        pig_utils::image::base64_decode(&document.data_base64)
                            .ok()
                            .map(|bytes| {
                                crate::rollout::document_token_estimate(&bytes, context_window)
                            });
                }
            }
        }
    }
}
