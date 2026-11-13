use super::*;

/// Lowercase access verb for an out-of-workspace rejection note
/// ("read"/"write").
pub(super) fn fs_access_verb(access: tool::FsAccess) -> String {
    match access {
        tool::FsAccess::Read => "read".to_string(),
        tool::FsAccess::Write => "write".to_string(),
    }
}

/// Persist an `ApprovedForProject` decision: append the conservative rule(s)
/// for this tool+subject to the workspace's `.pigcode/permissions.toml` and
/// apply them to this session's live rules immediately (no reload). Returns
/// the note prepended to the tool output; the call itself was approved either
/// way, so a rule that cannot be derived or saved never blocks it.
pub(super) fn remember_project_rule(
    ctx: &mut GateCtx,
    call: &ToolCall,
    subject: &str,
) -> Option<String> {
    if !ctx
        .permission_rules
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .project_allows_are_trusted()
    {
        return Some(
            "Note: this workspace is not trusted, so no project rule was saved; this call was \
             approved once. Trust the workspace to persist approval rules."
                .to_string(),
        );
    }
    let rules = match crate::permissions::approval_rule_texts(&call.name, subject) {
        Ok(rules) => rules,
        Err(reason) => {
            return Some(format!(
                "Note: no workspace rule was added — {reason}. This call was approved once; \
                 approve it again when you need it repeatedly."
            ));
        }
    };
    match crate::permissions::append_project_allow(ctx.cwd, &rules) {
        Ok(()) => {
            let mut live = ctx
                .permission_rules
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for rule in &rules {
                live.add_allow(rule);
            }
            Some(format!(
                "Note: approved for this workspace — added {} to {}.",
                rules.join(", "),
                crate::permissions::PERMISSIONS_FILE
            ))
        }
        Err(detail) => Some(format!(
            "Note: the approval rule could not be saved ({detail}). This call was approved once; \
             fix the workspace rule file to save further rules."
        )),
    }
}

/// Event emission from free paths (background subagent tasks/gated free
/// functions): the same channel and session id as Session::emit. Ordering is
/// channel order — no sequence cursor is carried.
pub(crate) fn emit_bg(
    session_id: &str,
    tx: &async_channel::Sender<Event>,
    build: impl FnOnce(String) -> Event,
) {
    let _ = tx.send_blocking(build(session_id.to_string()));
}

/// Persist the change tracker's newly inserted session baselines. The dirty
/// queue is shared by the parent and every child driver, so any gate or the
/// turn-end backstop may drain it; each path is upserted at most once.
pub(super) fn persist_tracker_originals(
    tracker: &mut ChangeTracker,
    store: &Arc<Mutex<Store>>,
    session_id: &str,
) {
    const MAX_ORIGINAL_BYTES: usize = 4 * 1024 * 1024;
    let dirty = tracker.take_dirty();
    if dirty.is_empty() {
        return;
    }
    let store = store.lock().expect("store lock");
    for path in dirty {
        if let Some(original) = tracker.original(&path) {
            let oversized = original
                .as_ref()
                .is_some_and(|content| content.len() > MAX_ORIGINAL_BYTES);
            if !oversized {
                store.upsert_file_original(
                    session_id,
                    &path.to_string_lossy(),
                    original.as_deref(),
                );
            }
        }
    }
}
