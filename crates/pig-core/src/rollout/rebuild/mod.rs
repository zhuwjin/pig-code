use super::*;
use crate::tool::MediaCapabilities;
use pig_provider::{ChatImage, ChatMsg, ToolCall};
use std::collections::HashMap;

/// Rebuild the exact continuable message shape represented by canonical
/// records, with legacy records retained as a best-effort compatibility path.
/// No media bytes are read: this is for callers that only need text, tool calls
/// and the recorded projection. Session replay must use
/// [`rebuild_history_with_media_root`] so every media read is contained.
pub fn rebuild_history(
    records: &[RolloutRecord],
    system: String,
) -> (Vec<ChatMsg>, Vec<Option<usize>>) {
    let rebuilt = rebuild_records(
        records,
        system,
        RebuildOptions {
            media: None,
            media_root: None,
            load_media: false,
            context_window: None,
            skip_media_before: 0,
        },
    );
    (rebuilt.history, rebuilt.user_ixs)
}

/// Rebuild with the owning session's media root. Rehydration validates bytes,
/// type and dimensions and refuses paths outside `media_root`; there is no
/// unrooted media path. PDF token estimates are reused from the persisted
/// reference, or computed once from `context_window` for old records. When a
/// canonical compact is present, a cheap structure-only pre-pass finds the
/// first record whose messages can survive that compact, and the real pass
/// skips media reads for the discarded prefix.
pub fn rebuild_history_with_media_root(
    records: &[RolloutRecord],
    system: String,
    media: Option<MediaCapabilities>,
    media_root: &Path,
    context_window: Option<u64>,
) -> (Vec<ChatMsg>, Vec<Option<usize>>) {
    let options = RebuildOptions {
        media,
        media_root: Some(media_root),
        load_media: true,
        skip_media_before: 0,
        context_window,
    };
    if records
        .iter()
        .any(|record| matches!(record, RolloutRecord::Compact { .. }))
    {
        let dry = rebuild_records(
            records,
            system.clone(),
            RebuildOptions {
                load_media: false,
                ..options
            },
        );
        if let Some(skip_before) = dry.skip_media_before {
            let rebuilt = rebuild_records(
                records,
                system.clone(),
                RebuildOptions {
                    skip_media_before: skip_before,
                    ..options
                },
            );
            return (rebuilt.history, rebuilt.user_ixs);
        }
    }
    let rebuilt = rebuild_records(records, system, options);
    (rebuilt.history, rebuilt.user_ixs)
}

#[derive(Clone, Copy)]
struct RebuildOptions<'a> {
    media: Option<MediaCapabilities>,
    /// Present exactly when `load_media` is true; there is no unrooted read.
    media_root: Option<&'a Path>,
    /// false = structure-only pass (no media reads, no missing-media notes).
    load_media: bool,
    /// Model window used to price old PDF references that predate the cached
    /// token cost. New references carry the cost and never re-scan.
    context_window: Option<u64>,
    /// Media for records below this record index is not read: those records
    /// can only have produced messages a canonical compact discarded.
    skip_media_before: usize,
}

/// Receipts are indexed by their live message position so Recall can revoke
/// only records it actually removed. Compaction keeps the receipt but forgets
/// its old position, preventing a later recall from revoking omitted history.
pub fn notification_delivery_receipts(
    records: &[RolloutRecord],
) -> HashMap<String, pig_protocol::DeliveryChannel> {
    let rebuilt = rebuild_records(
        records,
        String::new(),
        RebuildOptions {
            media: None,
            media_root: None,
            load_media: false,
            context_window: None,
            skip_media_before: 0,
        },
    );
    rebuilt
        .notification_receipts
        .into_iter()
        .map(|(id, (_, channel))| (id, channel))
        .collect()
}

struct RebuildOutput {
    history: Vec<ChatMsg>,
    user_ixs: Vec<Option<usize>>,
    notification_receipts: HashMap<String, (Option<usize>, pig_protocol::DeliveryChannel)>,
    /// Earliest record index that can still contribute a message to the final
    /// history when a canonical compact applied.
    skip_media_before: Option<usize>,
}

fn rebuild_records(
    records: &[RolloutRecord],
    system: String,
    options: RebuildOptions<'_>,
) -> RebuildOutput {
    let media = options.media;
    let system = records
        .iter()
        .rev()
        .find_map(|record| match record {
            RolloutRecord::System { text } => Some(text.clone()),
            RolloutRecord::PromptSnapshot { snapshot } => Some(snapshot.text.clone()),
            _ => None,
        })
        .unwrap_or(system);
    let mut history = vec![ChatMsg::system(system)];
    let mut provenance: Vec<Option<usize>> = vec![None];
    let mut user_ixs = Vec::new();
    let mut pending_reasoning: Option<String> = None;
    let mut pending_replay: Option<pig_provider::AssistantReplay> = None;
    let mut active_replay: Option<ActiveReplay> = None;
    let mut skip_media_before: Option<usize> = None;
    let mut notification_receipts = HashMap::new();
    let meta_cwd = records.iter().find_map(|record| match record {
        RolloutRecord::Meta { cwd, .. } => Some(cwd.clone()),
        _ => None,
    });
    for (record_ix, record) in records.iter().enumerate() {
        let load_media = options.load_media && record_ix >= options.skip_media_before;
        match record {
            RolloutRecord::Meta { .. }
            | RolloutRecord::System { .. }
            | RolloutRecord::PromptSnapshot { .. }
            | RolloutRecord::ToolExecutionStart { .. }
            | RolloutRecord::TurnStats { .. }
            | RolloutRecord::StepUsage { .. }
            | RolloutRecord::TurnChanges { .. }
            | RolloutRecord::AgentRun { .. } => {}
            RolloutRecord::TaskNotificationBatch {
                notifications,
                model_text,
                ..
            } => {
                finalize_active_replay(&mut history, &mut active_replay);
                pending_reasoning = None;
                pending_replay = None;
                history.push(ChatMsg::user(model_text.clone()));
                provenance.push(None);
                let index = history.len() - 1;
                for notification in notifications {
                    notification_receipts
                        .entry(notification.id.clone())
                        .or_insert((Some(index), pig_protocol::DeliveryChannel::Automatic));
                }
            }
            RolloutRecord::MailboxMessage { text, .. } => {
                finalize_active_replay(&mut history, &mut active_replay);
                pending_reasoning = None;
                pending_replay = None;
                history.push(ChatMsg::user(text.clone()));
                provenance.push(None);
            }
            RolloutRecord::User {
                text,
                files,
                images,
                sent_text,
                sent_images,
                model_text,
                sent_input_image,
                ..
            } => {
                finalize_active_replay(&mut history, &mut active_replay);
                pending_reasoning = None;
                pending_replay = None;
                let input_image = media.map(|caps| caps.input_image);
                let reproject = match (sent_input_image, input_image, model_text) {
                    (Some(original), Some(current), Some(base)) if *original != current => {
                        Some((base, current))
                    }
                    _ => None,
                };
                let mut text = if let Some((base, _)) = reproject {
                    base.clone()
                } else if let Some(sent_text) = sent_text {
                    sent_text.clone()
                } else {
                    let mut text = text.clone();
                    if !files.is_empty() {
                        if let Some(cwd) = &meta_cwd {
                            text = crate::session::pointer_file_references(cwd, &text, files);
                        } else {
                            text.push_str("\n\nReferenced files: ");
                            text.push_str(&files.join(", "));
                        }
                    }
                    text
                };
                let image_refs = if reproject.is_some() {
                    images
                } else if sent_text.is_some() {
                    sent_images
                } else {
                    images
                };
                let media_root = load_media.then_some(options.media_root).flatten();
                let mut chat_images: Vec<ChatImage> = media_root.map_or_else(Vec::new, |root| {
                    image_refs
                        .iter()
                        .enumerate()
                        .filter_map(|(ix, image_ref)| {
                            let mut image = rehydrate_image(image_ref, root)?;
                            if image_ref.label.is_none() {
                                image.label = Some(format!("Image {}", ix + 1));
                            }
                            Some(image)
                        })
                        .collect()
                });
                let missing = if media_root.is_some() {
                    image_refs.len() - chat_images.len()
                } else {
                    0
                };
                if missing > 0 {
                    text.push_str(&format!("\n\n[{missing} image(s) no longer available]"));
                }
                if media_root.is_some()
                    && let Some((_, current)) = reproject
                {
                    let paths: Vec<PathBuf> =
                        image_refs.iter().map(|image| image.path.clone()).collect();
                    crate::session::project_images(&mut text, &mut chat_images, &paths, current);
                }
                let mut msg = ChatMsg::user(text);
                msg.images.append(&mut chat_images);
                history.push(msg);
                provenance.push(Some(record_ix));
                user_ixs.push(Some(history.len() - 1));
            }
            RolloutRecord::Assistant {
                text,
                reasoning,
                replay,
                tool_calls,
                ..
            } => {
                finalize_active_replay(&mut history, &mut active_replay);
                if pending_replay.is_some() {
                    materialize_pending_replay(
                        &mut history,
                        &mut provenance,
                        &mut pending_replay,
                        &mut pending_reasoning,
                        String::new(),
                        &mut active_replay,
                        record_ix,
                    );
                    finalize_active_replay(&mut history, &mut active_replay);
                }
                pending_reasoning = None;
                pending_replay = None;
                let calls: Vec<ToolCall> = tool_calls
                    .iter()
                    .map(|call| ToolCall {
                        id: call.id.clone(),
                        name: call.function.name.clone(),
                        arguments: call.function.arguments.clone(),
                    })
                    .collect();
                let call_ids = calls.iter().map(|call| call.id.clone()).collect();
                let assistant_ix = history.len();
                history.push(ChatMsg::assistant_with_replay(
                    text.clone(),
                    calls,
                    reasoning.clone(),
                    replay.clone(),
                ));
                provenance.push(Some(record_ix));
                active_replay = Some(ActiveReplay {
                    assistant_ix,
                    call_ids,
                    next_result: 0,
                });
            }
            RolloutRecord::Reasoning { text, replay } => {
                finalize_active_replay(&mut history, &mut active_replay);
                pending_reasoning = Some(text.clone());
                pending_replay = replay.clone();
            }
            RolloutRecord::Text { text } => {
                finalize_active_replay(&mut history, &mut active_replay);
                materialize_pending_replay(
                    &mut history,
                    &mut provenance,
                    &mut pending_replay,
                    &mut pending_reasoning,
                    text.clone(),
                    &mut active_replay,
                    record_ix,
                );
            }
            RolloutRecord::ToolCall {
                tool,
                arguments,
                output,
                is_error,
                stopped,
                images,
                documents,
                delivered_notification_ids,
                ..
            } => {
                if pending_replay.is_some() {
                    materialize_pending_replay(
                        &mut history,
                        &mut provenance,
                        &mut pending_replay,
                        &mut pending_reasoning,
                        String::new(),
                        &mut active_replay,
                        record_ix,
                    );
                }
                if let Some(call_id) = active_replay
                    .as_ref()
                    .and_then(|active| active.call_ids.get(active.next_result).cloned())
                {
                    if let Some(active) = active_replay.as_mut() {
                        active.next_result += 1;
                    }
                    history.push(restore_tool_result(
                        &call_id,
                        output,
                        images,
                        documents,
                        media,
                        options.media_root,
                        load_media,
                        options.context_window,
                        *is_error,
                        *stopped,
                    ));
                    provenance.push(Some(record_ix));
                    let index = history.len() - 1;
                    for id in delivered_notification_ids {
                        notification_receipts
                            .entry(id.clone())
                            .or_insert((Some(index), pig_protocol::DeliveryChannel::TaskWait));
                    }
                    continue;
                }
                finalize_active_replay(&mut history, &mut active_replay);

                let call_id = format!("replay-{}", history.len());
                let wire_call = ToolCall {
                    id: call_id.clone(),
                    name: tool.clone(),
                    arguments: arguments.clone(),
                };
                if let Some(ChatMsg {
                    role,
                    tool_calls: Some(calls),
                    replay: None,
                    ..
                }) = history.last_mut()
                    && role == "assistant"
                {
                    calls.push(wire_call.to_wire());
                } else {
                    history.push(ChatMsg::assistant(
                        String::new(),
                        vec![wire_call],
                        pending_reasoning.take(),
                    ));
                    provenance.push(Some(record_ix));
                }
                history.push(restore_tool_result(
                    &call_id,
                    output,
                    images,
                    documents,
                    media,
                    options.media_root,
                    load_media,
                    options.context_window,
                    *is_error,
                    *stopped,
                ));
                provenance.push(Some(record_ix));
                let index = history.len() - 1;
                for id in delivered_notification_ids {
                    notification_receipts
                        .entry(id.clone())
                        .or_insert((Some(index), pig_protocol::DeliveryChannel::TaskWait));
                }
            }
            RolloutRecord::Compact {
                note,
                omitted,
                tail_len,
                preserved_user,
                ..
            } => {
                finalize_active_replay(&mut history, &mut active_replay);
                pending_reasoning = None;
                pending_replay = None;
                if let Some(tail_len) = tail_len {
                    let omitted = *omitted;
                    let tail_len = *tail_len;
                    let preserved_count = usize::from(preserved_user.is_some());
                    let cut = 1 + omitted + preserved_count;
                    let expected = cut + tail_len;
                    // Every compacted history must keep the active user. A raw
                    // tail that starts at a real user provides it; an
                    // assistant-starting or empty tail needs the separately
                    // preserved user, otherwise the install would reduce the
                    // context to `[system, note]` (or a note plus assistants)
                    // and drop every request.
                    let last_real_user = user_ixs.last().copied().flatten();
                    let promoted_checkpoint = history.get(1).is_some_and(|message| {
                        message.role == "system" || crate::rollout::is_compaction_handoff(message)
                    }) && history.get(2).is_some_and(|message| {
                        message.role == "user" && !crate::rollout::is_compaction_handoff(message)
                    });
                    let tail_has_user_anchor = history.get(cut).is_some_and(|message| {
                        message.role == "user" && !crate::rollout::is_compaction_handoff(message)
                    });
                    let anchor_ok = preserved_user.is_some() || tail_has_user_anchor;
                    let active_kept = match last_real_user {
                        Some(index) => index >= cut || *preserved_user == Some(index),
                        None => !promoted_checkpoint || cut <= 2 || *preserved_user == Some(2),
                    };
                    if history.len() == expected
                        && omitted > 0
                        && compact_boundary_ok(&history, cut)
                        && anchor_ok
                        && active_kept
                    {
                        // The preserved user is re-read from the rebuilt User
                        // record above, so restart keeps the exact media and
                        // capability projection without storing base64 here.
                        // The index must be the live active user (the last
                        // tracked real User) or the promoted post-note
                        // checkpoint: a merely valid earlier user would
                        // silently install a different boundary.
                        let preserved = preserved_user.and_then(|index| {
                            let promoted_anchor = index == 2
                                && matches!(last_real_user, None | Some(2))
                                && promoted_checkpoint;
                            let valid = index >= 1
                                && index < cut
                                && history
                                    .get(index)
                                    .is_some_and(|message| message.role == "user")
                                && (crate::rollout::is_user_run_start(&history, index)
                                    || crate::rollout::is_compaction_checkpoint_run(
                                        &history, index,
                                    ))
                                && (last_real_user == Some(index) || promoted_anchor);
                            valid.then(|| history[index].clone())
                        });
                        if preserved_user.is_none() || preserved.is_some() {
                            let preserved_provenance = preserved_user
                                .and_then(|index| provenance.get(index).copied().flatten());
                            let tail_provenance = provenance.split_off(cut);
                            let tail = history.split_off(cut);
                            history.truncate(1);
                            history.push(crate::rollout::compaction_note_message(note.clone()));
                            provenance.truncate(1);
                            provenance.push(None);
                            if let Some(user) = preserved {
                                history.push(user);
                                provenance.push(preserved_provenance);
                            }
                            history.extend(tail);
                            provenance.extend(tail_provenance.iter().copied());
                            for (index, _) in notification_receipts.values_mut() {
                                *index = index.and_then(|index| {
                                    (index >= cut).then_some(2 + preserved_count + (index - cut))
                                });
                            }
                            // Everything before this record that produced no
                            // retained message (and is not the preserved user)
                            // can be rebuilt without reading its media.
                            let first_retained = tail_provenance
                                .iter()
                                .copied()
                                .flatten()
                                .chain(preserved_provenance)
                                .min()
                                .unwrap_or(record_ix + 1);
                            skip_media_before = Some(
                                skip_media_before
                                    .map_or(first_retained, |old| old.max(first_retained)),
                            );
                            for entry in &mut user_ixs {
                                *entry = match *entry {
                                    Some(ix) if ix >= cut => Some(ix - cut + 2 + preserved_count),
                                    Some(_) => None,
                                    None => None,
                                };
                            }
                        } else {
                            tracing::warn!(
                                preserved_user = ?preserved_user,
                                last_real_user = ?last_real_user,
                                cut,
                                "compact preserved-user index does not reference the active real user; using legacy replay"
                            );
                            history.push(crate::rollout::compaction_note_message(note.clone()));
                            provenance.push(None);
                        }
                    } else {
                        tracing::warn!(
                            expected,
                            actual = history.len(),
                            cut,
                            boundary_ok = compact_boundary_ok(&history, cut),
                            anchor_ok,
                            active_kept,
                            preserved_user = ?preserved_user,
                            "compact context boundary is invalid; using legacy replay"
                        );
                        history.push(crate::rollout::compaction_note_message(note.clone()));
                        provenance.push(None);
                    }
                } else {
                    history.push(crate::rollout::compaction_note_message(note.clone()));
                    provenance.push(None);
                }
            }
            RolloutRecord::HookContext { text } => {
                finalize_active_replay(&mut history, &mut active_replay);
                pending_reasoning = None;
                pending_replay = None;
                history.push(ChatMsg::user(text.clone()));
                provenance.push(Some(record_ix));
            }
            RolloutRecord::Recall => {
                finalize_active_replay(&mut history, &mut active_replay);
                pending_reasoning = None;
                pending_replay = None;
                match user_ixs.pop() {
                    Some(Some(ix)) => {
                        history.truncate(ix);
                        provenance.truncate(ix);
                        notification_receipts
                            .retain(|_, (index, _)| index.is_none_or(|index| index < ix));
                    }
                    Some(None) => {}
                    None => {
                        tracing::warn!("recall record without a preceding user message; ignored")
                    }
                }
            }
        }
    }
    if pending_replay.is_some() {
        materialize_pending_replay(
            &mut history,
            &mut provenance,
            &mut pending_replay,
            &mut pending_reasoning,
            String::new(),
            &mut active_replay,
            records.len(),
        );
    }
    finalize_active_replay(&mut history, &mut active_replay);
    debug_assert_eq!(history.len(), provenance.len());
    RebuildOutput {
        history,
        user_ixs,
        notification_receipts,
        skip_media_before,
    }
}

#[allow(clippy::too_many_arguments)] // Tool result + media + projection controls stay explicit.
fn restore_tool_result(
    call_id: &str,
    output: &str,
    image_refs: &[ImageRef],
    document_refs: &[DocumentRef],
    media: Option<MediaCapabilities>,
    media_root: Option<&Path>,
    load_media: bool,
    context_window: Option<u64>,
    is_error: bool,
    stopped: bool,
) -> ChatMsg {
    let mut output = output.to_string();
    let media_root = load_media.then_some(media_root).flatten();
    let images_enabled = media_root.is_some() && media.is_none_or(|caps| caps.input_image);
    let images: Vec<ChatImage> =
        media_root
            .filter(|_| images_enabled)
            .map_or_else(Vec::new, |root| {
                image_refs
                    .iter()
                    .filter_map(|image| rehydrate_image(image, root))
                    .collect()
            });
    let missing = if media_root.is_some() {
        image_refs.len() - images.len()
    } else {
        0
    };
    if missing > 0 {
        if images_enabled {
            output.push_str(&format!("\n\n[{missing} image(s) no longer available]"));
        } else {
            let paths: Vec<String> = image_refs
                .iter()
                .map(|image| image.path.display().to_string())
                .collect();
            output.push_str(&format!(
                "\n\n[{missing} image(s) were not sent with this message: the current model does not support image input; the files are at {}]",
                paths.join(", ")
            ));
        }
    }
    let documents_enabled = media_root.is_some() && media.is_none_or(|caps| caps.input_pdf);
    let documents = media_root
        .filter(|_| documents_enabled)
        .map_or_else(Vec::new, |root| {
            rehydrate_documents(document_refs, root, context_window)
        });
    let missing_docs = if media_root.is_some() {
        document_refs.len() - documents.len()
    } else {
        0
    };
    if missing_docs > 0 {
        if documents_enabled {
            output.push_str(&format!(
                "\n\n[{missing_docs} PDF document(s) no longer available]"
            ));
        } else {
            let paths: Vec<String> = document_refs
                .iter()
                .map(|document| document.path.display().to_string())
                .collect();
            output.push_str(&format!(
                "\n\n[{missing_docs} PDF document(s) were not sent with this message: the current model does not support PDF input; the files are at {}]",
                paths.join(", ")
            ));
        }
    }
    let mut message =
        ChatMsg::tool_result_with_images(call_id, output, images).with_error(is_error);
    message.documents = documents;
    message.stopped = stopped;
    message
}

struct ActiveReplay {
    assistant_ix: usize,
    call_ids: Vec<String>,
    next_result: usize,
}

fn materialize_pending_replay(
    history: &mut Vec<ChatMsg>,
    provenance: &mut Vec<Option<usize>>,
    pending_replay: &mut Option<pig_provider::AssistantReplay>,
    pending_reasoning: &mut Option<String>,
    text: String,
    active_replay: &mut Option<ActiveReplay>,
    record_ix: usize,
) {
    let Some(replay) = pending_replay.take() else {
        history.push(ChatMsg::assistant(text, vec![], pending_reasoning.take()));
        provenance.push(Some(record_ix));
        return;
    };
    let content = if text.is_empty() {
        replay
            .items
            .iter()
            .filter_map(|item| match item {
                pig_provider::AssistantReplayItem::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
    } else {
        text
    };
    let calls: Vec<ToolCall> = replay
        .items
        .iter()
        .filter_map(|item| match item {
            pig_provider::AssistantReplayItem::ToolCall { call } => Some(ToolCall {
                id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            }),
            _ => None,
        })
        .collect();
    let call_ids = calls.iter().map(|call| call.id.clone()).collect();
    let assistant_ix = history.len();
    history.push(ChatMsg::assistant_with_replay(
        content,
        calls,
        pending_reasoning.take(),
        Some(replay),
    ));
    provenance.push(Some(record_ix));
    *active_replay = Some(ActiveReplay {
        assistant_ix,
        call_ids,
        next_result: 0,
    });
}

fn finalize_active_replay(history: &mut [ChatMsg], active_replay: &mut Option<ActiveReplay>) {
    let Some(active) = active_replay.take() else {
        return;
    };
    let Some(msg) = history.get_mut(active.assistant_ix) else {
        return;
    };
    let answered = &active.call_ids[..active.next_result];
    let clear_calls = if let Some(calls) = msg.tool_calls.as_mut() {
        calls.truncate(active.next_result);
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
}

/// A compact tail may start at a complete assistant round, at the first
/// message of a user run, or after a promoted checkpoint in the same run;
/// anything else (a tool result, a hook-led mid-run user, a system note) would
/// orphan a call/result pair or split a user run without keeping its request.
/// A zero-length tail (`cut == history.len()`) has no message boundary to
/// check; the caller separately requires a user anchor for that shape.
fn compact_boundary_ok(history: &[ChatMsg], cut: usize) -> bool {
    if cut == 0 || cut > history.len() {
        return false;
    }
    match history.get(cut) {
        None => true,
        Some(message) => match message.role.as_str() {
            // An assistant round may follow a user, a tool result or another
            // assistant; a system note before it would mean the cut starts a
            // request with no user anchor.
            "assistant" => history[cut - 1].role != "system",
            "user" => {
                crate::rollout::is_user_run_start(history, cut)
                    || crate::rollout::is_compaction_checkpoint_run(history, cut)
            }
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests;
