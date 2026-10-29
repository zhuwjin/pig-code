use super::*;

/// Canonical child compaction marker (one `"type":"compact"` line in the
/// child JSONL). It owns the same replay authority as the root
/// `RolloutRecord::Compact`: the note replaces the omitted work, `tail_len`
/// counts the retained messages at the cut, and `preserved_user` names the
/// active real user that was installed separately after the note. Message
/// content and media stay in their own `msg` lines (never base64 here), so a
/// restart rebuilds the exact context and the current model's capability
/// projection decides what media is sent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentCompact {
    pub note: String,
    pub omitted: usize,
    #[serde(default)]
    pub automatic: bool,
    #[serde(default)]
    pub summary: Option<String>,
    pub tail_len: usize,
    #[serde(default)]
    pub preserved_user: Option<usize>,
    /// "watermark" or "overflow" (diagnostics only)
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub used_before: Option<u64>,
    #[serde(default)]
    pub used_after: Option<u64>,
}

/// Apply one canonical compaction marker to the parsed history. Invalid
/// indexes (tampered/torn records) leave the history untouched with a warning
/// rather than discarding the child's context; the same conservative policy as
/// the root rollout rebuild.
pub(super) fn apply_agent_compact(
    history: &mut Vec<StoredAgentMsg>,
    record: &AgentCompact,
    line_no: usize,
) {
    let preserved_count = usize::from(record.preserved_user.is_some());
    let cut = 1 + record.omitted + preserved_count;
    let expected = cut + record.tail_len;
    if record.omitted == 0
        || history.len() != expected
        || !history
            .first()
            .is_some_and(|message| message.msg.role == "system")
        || !agent_compact_boundary_ok(history, cut)
    {
        tracing::warn!(
            line = line_no,
            history_len = history.len(),
            expected,
            cut,
            "subagent compact record does not match the rebuilt history; keeping the previous context"
        );
        return;
    }
    let preserved = match record.preserved_user {
        Some(ix) => {
            // The separately preserved message is the latest real user before
            // the cut (the child's active task/request) and no later user
            // exists inside or after the retained tail. A note prefix is a
            // synthetic boundary, so the first real user may follow it.
            let valid = ix >= 1
                && ix < cut
                && history[ix].msg.role == "user"
                && (stored_user_run_start(history, ix) || stored_checkpoint_run(history, ix))
                && history[ix + 1..cut]
                    .iter()
                    .all(|message| message.msg.role != "user")
                && history[cut..]
                    .iter()
                    .all(|message| message.msg.role != "user");
            if !valid {
                tracing::warn!(
                    line = line_no,
                    preserved_user = ix,
                    cut,
                    "subagent compact record has an invalid preserved user; keeping the previous context"
                );
                return;
            }
            Some(history[ix].clone())
        }
        None => {
            // A raw tail that starts at a user provides the request anchor;
            // otherwise the install would leave note-only assistant messages.
            if !history
                .get(cut)
                .is_some_and(|message| message.msg.role == "user")
            {
                tracing::warn!(
                    line = line_no,
                    cut,
                    "subagent compact record has no user anchor; keeping the previous context"
                );
                return;
            }
            None
        }
    };
    let tail = history.split_off(cut);
    history.truncate(1);
    history.push(StoredAgentMsg {
        msg: crate::rollout::compaction_note_message(record.note.clone()),
        images: Vec::new(),
        documents: Vec::new(),
    });
    if let Some(user) = preserved {
        history.push(user);
    }
    history.extend(tail);
}

fn stored_user_run_start(history: &[StoredAgentMsg], ix: usize) -> bool {
    let handoff = |message: &StoredAgentMsg| crate::rollout::is_compaction_handoff(&message.msg);
    history.get(ix).is_some_and(|message| {
        message.msg.role == "user"
            && !handoff(message)
            && (ix == 0 || history[ix - 1].msg.role != "user" || handoff(&history[ix - 1]))
    })
}

fn agent_compact_boundary_ok(history: &[StoredAgentMsg], cut: usize) -> bool {
    if cut == 0 || cut > history.len() {
        return false;
    }
    match history.get(cut) {
        None => true,
        Some(message) => match message.msg.role.as_str() {
            "assistant" => history[cut - 1].msg.role != "system",
            // A user run may start after the note (the first real user) or
            // start with the promoted checkpoint; a cut inside such a
            // checkpoint run preserves the later request verbatim.
            "user" => stored_user_run_start(history, cut) || checkpoint_run_cut(history, cut),
            _ => false,
        },
    }
}

fn checkpoint_run_cut(history: &[StoredAgentMsg], cut: usize) -> bool {
    stored_checkpoint_run(history, cut)
}

/// Whether `index` is a user message inside a run that starts with the
/// promoted checkpoint installed directly after a compaction note. Such a run
/// may be cut/preserved at the active request: the checkpoint is summarized
/// with the old note.
fn stored_checkpoint_run(history: &[StoredAgentMsg], index: usize) -> bool {
    if !history
        .get(index)
        .is_some_and(|message| message.msg.role == "user")
    {
        return false;
    }
    let mut run_start = index;
    while run_start > 0
        && history[run_start - 1].msg.role == "user"
        && !crate::rollout::is_compaction_handoff(&history[run_start - 1].msg)
    {
        run_start -= 1;
    }
    run_start < index
        && run_start == stored_fixed_prefix_len(history)
        && run_start > 0
        && crate::rollout::is_compaction_note(&history[run_start - 1].msg)
}

fn stored_fixed_prefix_len(history: &[StoredAgentMsg]) -> usize {
    let mut index = 0;
    while history.get(index).is_some_and(|message| {
        message.msg.role == "system" || crate::rollout::is_compaction_handoff(&message.msg)
    }) {
        index += 1;
    }
    index
}
