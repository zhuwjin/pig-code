use super::*;

/// Pending approvals: request_id → reply channel. Shared between the manager
/// and every session; request_ids carry a session_id prefix and are globally
/// unique.
///
/// Every request is resolved **only** by its own id. Earlier versions fanned a
/// decision out to waiters sharing a (tool, subject) key; with concurrent
/// sessions that could approve an action the user never saw (the same command
/// in another workspace, or a different requested root). The UI queues
/// approval cards per request, so no coalescing is needed.
pub type PendingApprovals =
    Arc<Mutex<HashMap<String, oneshot::Sender<(ApprovalDecision, Option<String>)>>>>;

/// Resolve exactly one approval. A late/duplicate reply for an unknown id is a
/// no-op; feedback rides only with the matching request (plan revision).
pub fn resolve_approval(
    pending: &PendingApprovals,
    request_id: &str,
    decision: ApprovalDecision,
    feedback: Option<String>,
) {
    let mut pending = pending.lock().expect("pending lock");
    if let Some(reply) = pending.remove(request_id) {
        let _ = reply.send((decision, feedback));
    }
}

/// Reject every pending approval belonging to one session (single-path
/// deletion/teardown): the session prefix is part of every request id we mint,
/// so this cannot touch another session's requests.
pub fn reject_session_approvals(pending: &PendingApprovals, session_id: &str) {
    let prefix = format!("{session_id}-");
    let ids: Vec<String> = pending
        .lock()
        .expect("pending lock")
        .keys()
        .filter(|id| id.starts_with(&prefix))
        .cloned()
        .collect();
    for id in ids {
        if let Some(reply) = pending.lock().expect("pending lock").remove(&id) {
            let _ = reply.send((ApprovalDecision::Reject, None));
        }
    }
}

/// Drop every pending structured question belonging to one session (the
/// waiter treats a dropped sender as "skipped").
pub fn drop_session_questions(pending: &PendingQuestions, session_id: &str) {
    let prefix = format!("{session_id}-");
    let ids: Vec<String> = pending
        .lock()
        .expect("pending questions lock")
        .keys()
        .filter(|id| id.starts_with(&prefix))
        .cloned()
        .collect();
    for id in ids {
        pending.lock().expect("pending questions lock").remove(&id);
    }
}

/// Pending structured questions: request_id → reply channel. None = the user
/// skipped; outer level per question, inner level holds the selected labels
/// for that question ("other" free text is placed in as a label verbatim).
pub type PendingQuestions = Arc<Mutex<HashMap<String, oneshot::Sender<Option<Vec<Vec<String>>>>>>>;

/// One pending message admitted while a turn was running. One list carries
/// both lanes (ZCode's model): `steer: true` injects into the running turn
/// at the next safe boundary, `false` runs as the next turn — admission
/// order is global, so a steer that falls back to the queue when the turn
/// ends keeps its position instead of jumping older items
#[derive(Clone)]
pub(crate) struct PendingMessage {
    pub id: String,
    pub text: String,
    pub files: Vec<String>,
    pub images: Vec<pig_protocol::PendingImage>,
    pub permissions: PermissionSettings,
    pub steer: bool,
    /// Held by the paused-queue confirmation flow (Event::HeldQueuePrompt).
    /// Held items share the pending list so nothing can overwrite them; queue
    /// and steer snapshots exclude them, and no drain may pop one until
    /// Op::ResolveHeldQueue decides its fate.
    pub held: bool,
}

/// Persisted form of one pending message (images ride as base64 — queue
/// images are rare and few; the tradeoff is recorded in docs/PLAN.md §7)
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingPersist {
    pub id: String,
    pub text: String,
    pub files: Vec<String>,
    pub images: Vec<PendingImagePersist>,
    pub permissions: PermissionSettings,
    pub steer: bool,
    #[serde(default)]
    pub held: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingImagePersist {
    pub bytes_base64: String,
    pub mime: String,
}

/// Write the pending list to the store (empty list -> row deleted). Called
/// after every lane mutation from both the manager loop and the turn
/// (drain_steer); failures are logged, never fatal
pub(crate) fn persist_pending(
    store: &Arc<Mutex<Store>>,
    session_id: &str,
    pending: &PendingMailbox,
) {
    let items: Vec<PendingPersist> = pending
        .lock()
        .expect("pending lock")
        .iter()
        .map(|message| PendingPersist {
            id: message.id.clone(),
            text: message.text.clone(),
            files: message.files.clone(),
            images: message
                .images
                .iter()
                .map(|image| PendingImagePersist {
                    bytes_base64: pig_utils::image::base64_encode(&image.bytes),
                    mime: image.mime.clone(),
                })
                .collect(),
            permissions: message.permissions,
            steer: message.steer,
            held: message.held,
        })
        .collect();
    let store = store.lock().expect("store lock");
    if items.is_empty() {
        store.delete_pending(session_id);
    } else if let Ok(json) = serde_json::to_string(&items) {
        store.set_pending(session_id, &json);
    }
}

/// Read the persisted pending list back (reopen path): images decode from
/// base64, steer flags flatten to the queue lane (no turn is running after a
/// restart), ids rehydrate the session's counter
pub(crate) fn restore_pending(store: &Arc<Mutex<Store>>, session_id: &str) -> Vec<PendingMessage> {
    let Some(json) = store.lock().expect("store lock").get_pending(session_id) else {
        return Vec::new();
    };
    // Parse the array shape first, then decode each entry on its own: one
    // malformed row must not discard the messages around it.
    let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(&json) else {
        tracing::warn!(
            session_id,
            "persisted pending queue is not a JSON array; dropping it"
        );
        return Vec::new();
    };
    let mut restored = Vec::with_capacity(values.len());
    for value in values {
        let item = match serde_json::from_value::<PendingPersist>(value) {
            Ok(item) => item,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    %error,
                    "persisted queue item failed to parse; skipping this item"
                );
                continue;
            }
        };
        let mut images = Vec::new();
        for image in item.images {
            match pig_utils::image::base64_decode(&image.bytes_base64) {
                Ok(bytes) => images.push(pig_protocol::PendingImage {
                    bytes,
                    mime: image.mime,
                }),
                // Degrade the corrupt attachment, keep the queued text: the
                // user's message is still deliverable without the image.
                Err(error) => tracing::warn!(
                    session_id,
                    %error,
                    "persisted queue image failed to decode; dropping this attachment"
                ),
            }
        }
        restored.push(PendingMessage {
            id: item.id,
            text: item.text,
            files: item.files,
            images,
            permissions: item.permissions,
            steer: false,
            // A restart has no live prompt owner: the held message flattens
            // into the paused queue lane (nothing is lost).
            held: false,
        });
    }
    restored
}

/// Highest `qN` sequence among restored queue ids. Ids are minted as `q{n}`
/// by the manager; a corrupt persisted row (empty/foreign/multibyte id) is
/// ignored instead of panicking on a byte slice, and the counter simply
/// continues above the valid ones.
pub(crate) fn restored_queue_seq(restored: &[PendingMessage]) -> u64 {
    restored
        .iter()
        .filter_map(|message| message.id.strip_prefix('q'))
        .filter_map(|rest| rest.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

/// Pending-message list shared between the manager loop (push/mutate) and
/// the turn future (drain steer items at safe boundaries) — the Session
/// itself is owned by the turn future while running, so data flows through
/// this mirrored Arc handle (same pattern as entry.retry)
pub(crate) type PendingMailbox = Arc<Mutex<VecDeque<PendingMessage>>>;
