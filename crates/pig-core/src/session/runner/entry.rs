use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pig_protocol::{CoreError, Event, PermissionSettings};
use pig_provider::ResolvedModel;
use tokio_util::sync::CancellationToken;

use crate::session::{ModelSelection, PendingMailbox, Session, TurnOutcome, resolve_runtime_model};

/// Resolve the session's model through its stored workspace runtime config.
/// While the Session is idle its own runtime is authoritative; while a turn
/// owns the Session, the entry's synchronized clone is used. Failures are
/// typed; callers keep any queued/pending message persisted and emit the error
/// as nonfatal.
pub(super) fn resolve_entry(entry: &SessionEntry) -> Result<ResolvedModel, CoreError> {
    let runtime = entry
        .session
        .as_ref()
        .and_then(|session| session.runtime_config())
        .or(entry.runtime.as_deref());
    resolve_runtime_model(
        runtime,
        entry.model_override.as_ref(),
        entry.reasoning_level.as_deref(),
    )
}

pub(super) fn model_label(entry: &SessionEntry) -> (String, String) {
    resolve_entry(entry)
        .map(|resolved| (resolved.model, resolved.provider_name))
        // No model configured: empty-string sentinel (GUI renders the
        // localized "New task/No model" placeholder)
        .unwrap_or_else(|_| (String::new(), String::new()))
}

/// Apply a UI plan toggle that arrived while a turn (or the compact
/// pseudo-turn) owned the session. It was already persisted in the session
/// metadata; applying it now keeps the switch visible and makes the next turn
/// run under it.
pub(super) fn apply_pending_plan(
    entry: &mut SessionEntry,
    session: &mut Session,
    event_tx: &async_channel::Sender<Event>,
) {
    let Some(enabled) = entry.pending_plan.take() else {
        return;
    };
    session.set_plan_mode(enabled);
    session.emit(
        |session_id| Event::PlanModeChanged {
            session_id,
            enabled,
            deferred: false,
        },
        event_tx,
    );
}

pub(crate) type NotificationMailbox = Arc<Mutex<VecDeque<String>>>;

pub(super) struct SessionEntry {
    pub(super) session: Option<Session>,
    pub(super) plugin_dispatcher: crate::plugins::PluginToolDispatcher,
    /// Tool state sharing an Arc with Session: still usable to fetch todo/task
    /// snapshots while the session is inside the turn future (session=None)
    pub(super) state: crate::task::SessionToolState,
    /// Workspace root (usable while the Session is inside a turn future)
    pub(super) cwd: PathBuf,
    /// Effective layered runtime config for this workspace (trusted project
    /// layer included): one source for model resolution, provider expansion
    /// and endpoint normalization. Shared with the Session through an `Arc`,
    /// so updating it mid-turn is the same value the wrap-up installs.
    pub(super) runtime: Option<Arc<crate::config::RuntimeConfig>>,
    pub(super) cancel: Option<CancellationToken>,
    /// The active turn's retry control (Op::RetryNow reaches the in-flight
    /// retry wait through it); same lifetime as `cancel`
    pub(super) retry: Option<pig_provider::CallControl>,
    pub(super) model_override: Option<ModelSelection>,
    /// Session-level reasoning level: exists independently of the model override (applies to the configured default model when there is no override)
    pub(super) reasoning_level: Option<String>,
    /// Exec mode of the most recent user message/manual switch: notification-only turns use it to start turns (not default, which would
    /// override the user's current mode)
    pub(super) last_permissions: PermissionSettings,
    /// Pending messages admitted while a turn was running, one list for both
    /// lanes (queue + steer, ZCode's model): the manager pushes/mutates, the
    /// turn drains steer items at safe boundaries, admission order is global.
    /// Held items (paused-queue confirmation) live here too so nothing can
    /// overwrite them.
    pub(super) pending: PendingMailbox,
    /// Terminal notification IDs awaiting model delivery; the SQLite outbox is
    /// their durable source of truth.
    pub(super) notifications: NotificationMailbox,
    /// Queue id counter shared by both lanes (per session)
    pub(super) queue_seq: u64,
    /// Queue auto-drain: false after the turn ended with an error or a user
    /// stop while the queue was non-empty (the head no longer auto-starts;
    /// Op::ResumeQueue re-enables it). Not persisted — a restart resumes
    /// draining
    pub(super) auto_drain: bool,
    /// The running turn was cancelled for a queue promotion (SendQueuedNow),
    /// not by the user: the wrap-up keeps draining instead of pausing
    pub(super) cancel_promote: bool,
    /// A UI plan toggle that arrived while a turn/compaction owned the
    /// session: already durable in SessionMeta, applied at the next start
    pub(super) pending_plan: Option<bool>,
    /// Whether the running turn accepts steering (false during compaction
    /// turns — Steer deliveries then fall back to the queue lane)
    pub(super) steerable: bool,
    /// MCP status cache: refreshed from the Session's manager at turn wrap-up or
    /// settings-page queries (None = not lazily connected yet; answered from this
    /// cache while the Session is out of hand mid-turn)
    pub(super) mcp_status: Option<Vec<pig_protocol::McpServerStatus>>,
    /// A plugin snapshot changed while a turn owned the Session: the
    /// wrap-up refreshes hooks/skills/profiles/tools/MCP before the next turn.
    pub(super) plugin_surfaces_stale: bool,
}

pub(super) type TurnFuture =
    std::pin::Pin<Box<dyn Future<Output = (String, Session, TurnOutcome)>>>;
