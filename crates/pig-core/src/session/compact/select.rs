//! Pure compaction selection and budgeting.
//!
//! Root compaction (this module's `Session::run_compact`) and child JSONL
//! compaction share this API; no history rewriting, summary request, rollout
//! append, or event belongs here.
//!
//! Reuse recipe for a child driver that has a rebuilt `Vec<ChatMsg>` history
//! (the compaction service uses `select_tail_plan`, `tail_budget`,
//! `compactable_tokens`, `CompactionSelection`, `TailPlan` and
//! `estimate_leading_system_tokens` for both callers):
//!
//! ```ignore
//! let leading = estimate_leading_system_tokens(&history);
//! let budget = tail_budget(context_window, max_output_tokens, leading, tools_tokens, note_tokens);
//! let target = budget.min(compactable_tokens(&history) / 2); // half for manual/watermark
//! let active = history.iter().rposition(|m| m.role == "user" && !is_compaction_handoff(m));
//! let plan = select_tail_plan(&history, CompactionSelection {
//!     target_tokens: target,
//!     pin: None,                  // Some(paused_assistant_ix) for a pause continuation
//!     keep_latest_segment: false, // true for a pause continuation
//!     active_user: active,
//!     active_run_start: active_run_start(&history, active),
//! });
//! // `plan.tail`, `plan.tail_start`, `plan.omitted`, `plan.preserved_user`
//! // are the same authority the root `Compact` record uses.
//! ```
//!
//! Invariants shared by every caller: cuts land on complete user-run or
//! complete assistant-round boundaries, or at the active request after a
//! promoted checkpoint; an active real user omitted by the cut is preserved
//! separately (index into the original history); a pinned paused
//! assistant/server round (and any later steer segment) is never dropped; and
//! the fixed prefix (frozen system + marked compaction handoffs) is never a
//! user segment. A cut that would omit only that fixed prefix is a no-op.

use super::*;

/// One complete-boundary compaction plan produced by [`select_tail_plan`].
///
/// `tail` and `tail_start` describe the raw retained suffix; `omitted` counts
/// the messages discarded between the frozen system (index 0) and that suffix,
/// excluding a separately preserved user; `preserved_user` is the index of the
/// active real user in the *original* history when the cut would otherwise
/// omit it. Callers install the same shape the canonical `Compact` record
/// replays: `[frozen system, handoff note] + preserved_user? + tail`.
#[derive(Clone, Debug)]
pub(crate) struct TailPlan {
    pub(crate) tail: Vec<ChatMsg>,
    /// Index of tail[0] in the original history (== history.len() for an
    /// empty raw tail).
    pub(crate) tail_start: usize,
    /// Messages actually discarded, between the frozen system and the tail,
    /// excluding the separately preserved active user. Together with
    /// `preserved_user` this is the Compact record's replay authority.
    pub(crate) omitted: usize,
    /// Index of the active real user message in the original history when the
    /// cut would otherwise omit it (assistant-starting tails and later
    /// hook-only segments). It is installed verbatim after the compact note so
    /// the request survives a zero-length raw tail.
    pub(crate) preserved_user: Option<usize>,
}

/// Pure inputs for [`select_tail_plan`]. This is the reusable API for root and
/// child (subagent JSONL) compaction; it owns no session state and performs no
/// I/O.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CompactionSelection {
    /// Maximum estimated tokens for everything retained after the note,
    /// including a separately preserved active user.
    pub(crate) target_tokens: u64,
    /// A message index that must survive the cut (the paused assistant of a
    /// continuation). The whole complete user segment containing it is
    /// retained; if that segment plus every later segment does not fit, the
    /// plan is `None` (skip) rather than dropping the paused server state.
    pub(crate) pin: Option<usize>,
    /// Keep the newest complete user segment(s) intact for a pause
    /// continuation, using the full target budget instead of the ordinary
    /// half/third content fraction.
    pub(crate) keep_latest_segment: bool,
    /// The active request's real user message (from the recall stack or the
    /// promoted post-note checkpoint). When the cut omits it, it is retained
    /// separately and its tokens count against `target_tokens`.
    pub(crate) active_user: Option<usize>,
    /// Earliest message of the active request's user run that must survive a
    /// split cut. It equals `active_user` for a normal request or when only a
    /// promoted checkpoint precedes it (the checkpoint may be summarized with
    /// the old note); it is earlier than `active_user` when a hook context
    /// leads the request. `None` falls back to the active user.
    pub(crate) active_run_start: Option<usize>,
}

/// Earliest message of the active request's run that a cut must retain.
///
/// A run that starts immediately after a compaction handoff starts with the
/// promoted checkpoint — a real request from before the handoff. Summarizing
/// that checkpoint with the old note is safe, so the mandatory suffix may
/// start at the next request instead of no-op'ing forever.
pub(crate) fn active_run_start(history: &[ChatMsg], active_user: Option<usize>) -> Option<usize> {
    let active = active_user?;
    let fixed = crate::rollout::fixed_prefix_len(history);
    let mut run_start = active;
    while run_start > fixed
        && history[run_start - 1].role == "user"
        && !crate::rollout::is_compaction_handoff(&history[run_start - 1])
    {
        run_start -= 1;
    }
    if run_start < active
        && run_start == fixed
        && run_start > 0
        && crate::rollout::is_compaction_note(&history[run_start - 1])
    {
        return Some(run_start + 1);
    }
    Some(run_start)
}

/// Complete user segments after the leading system messages: one user run
/// (one or more consecutive user-role messages, e.g. a real user message plus
/// its hook contexts) followed by every assistant/tool message up to the next
/// user run. Calls and their results are never split.
pub(crate) fn segments(history: &[ChatMsg]) -> Option<Vec<(usize, usize)>> {
    let leading = crate::rollout::fixed_prefix_len(history);
    let mut starts: Vec<usize> = Vec::new();
    for ix in leading..history.len() {
        if crate::rollout::is_user_run_start(history, ix) {
            starts.push(ix);
        }
    }
    if starts.is_empty() {
        return None;
    }
    Some(
        starts
            .iter()
            .enumerate()
            .map(|(ix, start)| {
                let end = starts.get(ix + 1).copied().unwrap_or(history.len());
                (*start, end)
            })
            .collect(),
    )
}

/// Tokens of all content a comparison can omit: every complete user segment
/// after the leading system prefix. The compaction trigger keeps a fraction of
/// this (half for manual/watermark, one third for overflow recovery).
pub(crate) fn compactable_tokens(history: &[ChatMsg]) -> u64 {
    let leading = crate::rollout::fixed_prefix_len(history);
    segments(history).map_or(0, |segments| {
        segments
            .iter()
            .map(|(start, end)| estimate_history_tokens(&history[(*start).max(leading)..*end]))
            .sum()
    })
}

/// Retained-tail budget for a compaction: the model window minus the next
/// response reservation, the frozen/leading system prefix, the request's tool
/// schemas and the note that will replace the omitted work. `required_tokens`
/// lets callers pass a content-derived note reserve (the summary cap plus the
/// actual changed-file sentence) instead of a fixed constant.
pub(crate) fn tail_budget(
    context_window: u64,
    max_output_tokens: u64,
    leading_tokens: u64,
    tools_tokens: u64,
    required_tokens: u64,
) -> u64 {
    context_window
        .saturating_sub(max_output_tokens + COMPACT_CONTEXT_BUFFER)
        .saturating_sub(leading_tokens + tools_tokens + required_tokens)
}

/// Pure compaction tail selection. See [`CompactionSelection`] for the inputs
/// and [`TailPlan`] for the output contract. Returns `None` when nothing can be
/// omitted, when only leading system notes would be rewritten, or when a
/// pinned/latest segment cannot fit the target.
pub(crate) fn select_tail_plan(
    history: &[ChatMsg],
    selection: CompactionSelection,
) -> Option<TailPlan> {
    let leading = crate::rollout::fixed_prefix_len(history);
    let segments = segments(history)?;
    let active_user = selection.active_user.filter(|&ix| {
        history.get(ix).is_some_and(|message| {
            message.role == "user" && !crate::rollout::is_compaction_handoff(message)
        })
    });
    if selection.keep_latest_segment || selection.pin.is_some() {
        // Everything from the pinned segment (or the newest segment) through
        // the end is mandatory. A steer drained after a pause starts a later
        // segment, so this keeps both the paused assistant/server state and the
        // steer; retaining only the steer would silently restart the turn.
        let required_segment = match selection.pin {
            Some(pin) => segments
                .iter()
                .position(|(start, end)| *start <= pin && pin < *end)?,
            None => segments.len() - 1,
        };
        let required_start = segments[required_segment].0;
        let preserved = active_user.filter(|&ix| ix < required_start);
        let mut kept = estimate_history_tokens(&history[required_start..])
            + preserved.map_or(0, |ix| estimate_message_tokens(&history[ix]));
        if kept > selection.target_tokens {
            return None;
        }
        let mut keep_from = required_segment;
        for (ix, (start, end)) in segments.iter().enumerate().rev().skip(1) {
            if ix >= required_segment {
                continue;
            }
            let tokens = estimate_history_tokens(&history[*start..*end]);
            let preserved_now = active_user.filter(|&active| active < *start);
            let extra = preserved_now.map_or(0, |ix| estimate_message_tokens(&history[ix]));
            if kept + tokens + extra > selection.target_tokens {
                break;
            }
            kept += tokens;
            keep_from = ix;
        }
        return planned(history, segments[keep_from].0, selection);
    }

    // Whole newest user segments while they fit. A preserved active user that
    // sits before the candidate tail is still part of the retained context and
    // must count against the budget, or the rewritten history could exceed the
    // window it was supposed to fit.
    let preserved_tokens = active_user.map_or(0, |ix| estimate_message_tokens(&history[ix]));
    let mut keep_from = segments.len();
    let mut kept = 0u64;
    for (ix, (start, end)) in segments.iter().enumerate().rev() {
        let tokens = estimate_history_tokens(&history[*start..*end]);
        let preserved = if active_user.is_some_and(|active| active < *start) {
            preserved_tokens
        } else {
            0
        };
        if kept + tokens + preserved > selection.target_tokens {
            break;
        }
        kept += tokens;
        keep_from = ix;
    }
    if keep_from < segments.len() {
        return planned(history, segments[keep_from].0, selection);
    }

    // Even the newest segment does not fit. The active request may be in this
    // segment or in an earlier user run (a later Stop-hook continuation starts
    // its own segment); either way it must survive.
    let (segment_start, segment_end) = *segments.last().expect("segments is non-empty");
    let active = active_user.filter(|&ix| {
        ix >= leading
            && ix < segment_end
            && history
                .get(ix)
                .is_some_and(|message| message.role == "user")
    })?;
    let mandatory = selection.active_run_start.unwrap_or(active).min(active);
    if active < segment_start {
        // The active request is in an earlier run and a newer hook run is the
        // instruction that triggered the pending request. Keep the newest
        // segment (or the whole active run when a hook context leads it).
        let tail_start = if mandatory == active {
            segment_start
        } else {
            mandatory
        };
        return planned(history, tail_start, selection);
    }
    if mandatory < active {
        // A hook/reminder context leads the active request in its own run.
        // Dropping it would leave the model answering an older request, so the
        // whole run plus all newer work stays verbatim.
        return planned(history, mandatory, selection);
    }
    // The cut may start at the active request itself. A promoted checkpoint
    // before it in the same run is summarized with the old note; the active
    // request and every newer round stay. The newest complete rounds are kept
    // while they fit; zero raw tail is allowed when the newest round is
    // oversized.
    let mut kept = estimate_message_tokens(&history[active]);
    let round_starts: Vec<usize> = (active + 1..segment_end)
        .filter(|&ix| history[ix].role == "assistant")
        .collect();
    if round_starts.is_empty() {
        return planned(history, active, selection);
    }
    let mut tail_start = segment_end;
    for (round_ix, &start) in round_starts.iter().enumerate().rev() {
        let end = round_starts
            .get(round_ix + 1)
            .copied()
            .unwrap_or(segment_end);
        let tokens = estimate_history_tokens(&history[start..end]);
        if kept + tokens > selection.target_tokens {
            break;
        }
        kept += tokens;
        tail_start = start;
    }
    if tail_start <= active {
        return planned(history, active, selection);
    }
    planned(history, tail_start, selection)
}

/// Compatibility wrapper for tests that only need the two ordinary modes; new
/// code (including child compaction) should build a [`CompactionSelection`].
#[cfg(test)]
pub(super) fn select_tail(
    history: &[ChatMsg],
    target: u64,
    preserve_latest: bool,
    active_user: Option<usize>,
) -> Option<TailPlan> {
    select_tail_plan(
        history,
        CompactionSelection {
            target_tokens: target,
            pin: None,
            keep_latest_segment: preserve_latest,
            active_user,
            active_run_start: active_run_start(history, active_user),
        },
    )
}

/// Finish a plan: the raw tail starts at `tail_start`, the active real user is
/// preserved separately whenever the cut would omit it, and `omitted` counts
/// only discarded messages (never the frozen system or the preserved user).
fn planned(
    history: &[ChatMsg],
    tail_start: usize,
    selection: CompactionSelection,
) -> Option<TailPlan> {
    let preserved_user = match selection.active_user {
        Some(ix) if ix < tail_start => {
            // A separately preserved request must be the run start, or the
            // active request itself when only a promoted checkpoint precedes
            // it. Preserving a later message of a hook-led run would drop the
            // instruction that triggered it.
            if selection.active_run_start != Some(ix) {
                return None;
            }
            let valid = ix >= 1
                && history
                    .get(ix)
                    .is_some_and(|message| message.role == "user")
                && !crate::rollout::is_compaction_handoff(&history[ix]);
            if !valid {
                return None;
            }
            Some(ix)
        }
        _ => None,
    };
    let preserved_count = usize::from(preserved_user.is_some());
    let omitted = tail_start.saturating_sub(1 + preserved_count);
    if omitted == 0 || tail_start <= 1 {
        return None;
    }
    // Rewriting only leading compact notes would discard the previous model
    // summary without omitting conversation work. The shape right after a
    // zero-tail compaction when the next user message arrives is
    // [frozen, note, preserved user, new user]; once new work exists after the
    // next user, the split path may summarize the old checkpoint instead.
    let omits_conversation = (1..tail_start).any(|ix| {
        Some(ix) != preserved_user
            && history[ix].role != "system"
            && !crate::rollout::is_compaction_handoff(&history[ix])
    });
    if !omits_conversation {
        return None;
    }
    Some(TailPlan {
        tail: history[tail_start..].to_vec(),
        tail_start,
        omitted,
        preserved_user,
    })
}

/// Index of the active real user message, from the session's recall stack.
///
/// `Some(ix)` entries are recallable real User records. The latest entry being
/// `None` means that real user was promoted into a compaction checkpoint: it is
/// installed directly after the compact note and is deliberately not
/// recallable (its stack entry is a tombstone), so its current index is 2.
/// Hook/reminder contexts are not tracked, which is what keeps a Stop-hook
/// continuation's user run from masquerading as the active request.
pub(crate) fn active_real_user_ix(
    history: &[ChatMsg],
    user_ixs: &[Option<usize>],
) -> Option<usize> {
    if let Some(Some(ix)) = user_ixs.last()
        && history
            .get(*ix)
            .is_some_and(|message| message.role == "user")
    {
        return Some(*ix);
    }
    let promoted = history.len() > 2
        && history[0].role == "system"
        && (history[1].role == "system" || crate::rollout::is_compaction_handoff(&history[1]))
        && history[2].role == "user"
        && !crate::rollout::is_compaction_handoff(&history[2]);
    promoted.then_some(2)
}

/// Pure projection used by the preflight. An anchor applies only when it was
/// produced by the same model and the same rendered system prefix, and it
/// only covers the request up to `history_len`; anything else falls back to
/// the conservative current estimate and last real watermark.
pub(crate) fn project_context_tokens(
    anchor: Option<&UsageAnchor>,
    history: &[ChatMsg],
    last_total_tokens: Option<u64>,
    config: &ResolvedModel,
    tools_tokens: u64,
) -> u64 {
    let current = estimate_history_tokens(history) + tools_tokens;
    match anchor {
        Some(anchor)
            if anchor.model_key == model_key(config)
                && anchor.system_fingerprint == system_fingerprint(history)
                && anchor.history_len <= history.len() =>
        {
            let added = estimate_history_tokens(&history[anchor.history_len..]);
            anchor.used + added + tools_tokens.saturating_sub(anchor.tools_tokens)
        }
        _ => current.max(last_total_tokens.unwrap_or(0)),
    }
}
