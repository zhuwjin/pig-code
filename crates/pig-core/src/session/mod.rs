use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use pig_protocol::{
    ApprovalDecision, CoreError, Event, Op, PermissionSettings, SandboxMode, SessionMeta,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::config;
use crate::rollout::{Rollout, RolloutRecord, now_secs};
use crate::store::Store;
use crate::tool::{ChangeTracker, ToolContext};
use crate::{prompt, tool};
use pig_protocol::AppConfig;
use pig_provider::ResolvedModel;
use pig_provider::{ChatMsg, ToolCall};
use pig_utils::paths::normalize_workspace_path;

mod agent;
use agent::Agent;
mod approval;
mod batch;
mod catalog;
mod compact;
mod early;
mod effects;
mod engine;
mod estimate;
mod fork;
mod gate;
mod gate_context;
mod images;
mod initialization;
mod media;
mod model;
mod notifications;
mod operations;
mod outcome;
mod pending;
mod persistence;
mod replay;
mod repeat;
mod reviewer;
mod run_state;
mod runner;
mod scheduler;
mod subagent;
pub use subagent::AgentRunSupervisor;
pub(crate) use subagent::DelegatedAgentStarted;
pub(crate) use subagent::{LocalAgentExecutionHandle, ManagedOutcome};
mod stats;
mod step;
#[cfg(test)]
mod tests;
mod title;
mod turn;
mod usage;

pub(crate) use reviewer::{
    AgentApprovalReviewer, ApprovalReply, ApprovalRequest, ApprovalReviewer,
    FallbackApprovalReviewer, PolicyApprovalReviewer, UserApprovalReviewer,
};
use run_state::SessionRunState;

pub use approval::approval_detail;
pub use compact::COMPACTION_MARKER;
pub(crate) use compact::{CompactOutcome, CompactTrigger, compact_trigger};
pub(crate) use effects::emit_bg;
use effects::{fs_access_verb, persist_tracker_originals, remember_project_rule};
pub(crate) use gate::exec_tool_gated_ctx;
pub(crate) use gate_context::GateCtx;
pub(crate) use images::compression_note;
pub use images::project_images;
#[cfg(test)]
use initialization::{surviving_prompt_snapshot, uncounted_tail_usage};
pub(crate) use media::project_tool_media;
use media::{tool_documents_to_chat, tool_images_to_chat};
pub use model::ModelSelection;
pub(crate) use model::resolve_runtime_model;
use model::{meta_to_selection, skills_state_with_trust, snapshot_trusted};
use operations::CancelledTool;
use outcome::StepOutcome;
pub(crate) use outcome::{
    AgentRunResult, GatedToolOutcome, MAX_PAUSE_CONTINUATIONS, SubagentOutcome, TurnOutcome,
    new_agent_run_id, pause_limit_detail, pause_limit_error,
};
pub use pending::{
    PendingApprovals, PendingQuestions, drop_session_questions, reject_session_approvals,
    resolve_approval,
};
pub(crate) use pending::{
    PendingMailbox, PendingMessage, persist_pending, restore_pending, restored_queue_seq,
};
pub use pig_protocol::{AgentCatalogEntry, AgentRunStatus};
pub use runner::agent_loop;
pub use title::TITLE_PROMPT_MARKER;
pub(crate) use title::spawn_title_generation;
pub(crate) use turn::pointer_file_references;
pub(crate) use usage::{UsageAnchor, model_key, system_fingerprint};

pub struct Session {
    /// Shared agent context: identity, model, history and tool surface. Root
    /// and child agents both carry one; orchestration keeps the lifecycle.
    pub(crate) agent: Agent,
    /// Present when this Session is a child agent. Root sessions have none;
    /// the orchestration layer creates a child Session and drives the same
    /// turn engine over it.
    pub(super) subagent: Option<Box<subagent::SubagentState>>,
    /// Recallable user-message indices into `history`, in order (a stack:
    /// `recall_last_message` pops and truncates at the popped index). Only real
    /// User-record messages are tracked — hook contexts share the user role but
    /// are not recallable. A `None` entry is a message with no in-memory
    /// truncation point: an in-memory compact either omitted it (it survives
    /// only in the rollout file) or promoted it to the post-note checkpoint
    /// (still present at index 2 but deliberately not recallable, so a recall
    /// cannot truncate to the bare `[system, note]` prefix). Recall still
    /// appends its tombstone but skips the truncate; canonical compact records
    /// re-derive the same tombstones on resume.
    user_ixs: Vec<Option<usize>>,
    /// Full input projection of the previous model-io trace (the prefix
    /// baseline for delta persistence; empty after resume — the first entry
    /// automatically falls back to full, self-healing)
    io_last_input: Vec<crate::model_io::ModelIoMessage>,
    run_state: SessionRunState,
    tracker: ChangeTracker,
    state: crate::task::SessionToolState,
    /// Memory of "always allowed in this session": (tool name, subject) —
    /// ExecCommand = command first word, Write/Edit = path. Ordinary approvals only:
    /// a sandbox widening and a high-risk command are never remembered here.
    /// Shared by the root session and every child agent it owns: an approval
    /// answered in a child applies to the whole session, and vice versa.
    always_allowed: Arc<Mutex<HashSet<(String, String)>>>,
    /// Allow/deny rules: the user level (`{data_dir}/permissions.toml`) unioned
    /// with the project level (`.pigcode/permissions.toml`), loaded at session
    /// creation/replay and reloaded on a trust change. An
    /// `ApprovedForProject` decision under trust appends to the project file
    /// and extends this set live; an untrusted workspace does not offer that
    /// tier, because the next session would ignore the rule. Shared with child
    /// agents so a rule learned by one agent immediately governs the others.
    permission_rules: Arc<Mutex<crate::permissions::PermissionRules>>,
    /// Plan mode toggle (orthogonal to mode; flipped by the
    /// EnterPlanMode/ExitPlanMode tools and Op::SetPlanMode, persisted
    /// write-through to the store)
    plan_enabled: bool,
    reviewer: Arc<dyn ApprovalReviewer>,
    pending: PendingApprovals,
    pending_questions: PendingQuestions,
    /// Canonical permission state (sandbox / approval policy / reviewer)
    permissions: PermissionSettings,
    model_override: Option<ModelSelection>,
    rollout: Option<Rollout>,
    /// Sticky first rollout append/sync failure of this Session instance. Once
    /// set, later records are not appended (a missing durable unit would
    /// otherwise fork the file from memory); it is surfaced as a nonfatal
    /// `Event::Error` at the next durable boundary.
    rollout_failure: Option<CoreError>,
    /// The sticky failure was already reported to the UI.
    rollout_failure_reported: bool,
    delivery_receipts: Arc<Mutex<HashMap<String, crate::task::notification::DeliveryReceipt>>>,
    notification_delivery_failure: Option<CoreError>,
    notification_reconcile_required: bool,
    store: Arc<Mutex<Store>>,
    data_dir: PathBuf,
    /// Git snapshot (branch + dirty) at session start, reused by the env
    /// block — querying live every turn would flip the system prompt's prefix
    /// cache invalid and back with the first edit/commit
    git_snapshot: Option<String>,
    /// AGENTS.md section frozen at session start (injected into the system
    /// prompt). Mid-session changes are pushed via turn_reminder; the frozen
    /// copy is never written back — to protect the prefix cache (same
    /// trade-off as kimi agentsMdReminder)
    agents_prompt: String,
    /// Skill listing section frozen at session start (injected into the
    /// system prompt; bodies are still read on demand by the Skill tool).
    /// Rescanning every turn would let adding/removing/editing skills in
    /// Settings break the prefix cache (same trade-off as kimi-code
    /// frozenSkillListing), at the cost of changes only taking effect for new
    /// sessions
    skills_prompt: String,
    /// Workspace memory section frozen at session start (injected after the
    /// AGENTS.md section). Mid-session updates (/remember appends, post-turn
    /// extraction rewrites) ride the turn_reminder change-triggered channel —
    /// the frozen copy itself is never rewritten (prefix-cache stability)
    memory_prompt: String,
    /// Whether memory was enabled at session start (frozen; the reminder's
    /// fresh copy is only computed when true — a session that started with
    /// memory off never injects memory content, even after a mid-session
    /// /remember wrote the file)
    memory_enabled: bool,
    /// Date frozen at session start (displayed in the env block); corrected
    /// via turn_reminder on date rollover
    date_frozen: String,
    /// Last-reminded date/AGENTS.md/memory content (turn_reminder dedup:
    /// remind only when different from the frozen value; identical content is
    /// not re-injected)
    date_reminded: String,
    agents_reminded: String,
    memory_reminded: String,
    /// Reminder state is persisted with the prompt snapshot so resume does not
    /// either duplicate old reminders or rewrite the messages that carried them.
    permissions_reminded: Option<(PermissionSettings, bool, String)>,
    /// Profile description used by this agent's delegation tool schemas;
    /// spawning still resolves the current profiles by name.
    profiles_summary: String,
    prompt_text: Option<String>,
    prompt_memory_enabled: Option<bool>,
    /// Skills-state + trust sentinel (see `skills_state_with_trust`); a change
    /// in either forces the next turn to rebuild the system prompt.
    prompt_skills_state: Option<String>,
    projected_input_image: Option<bool>,
    projected_input_pdf: Option<bool>,
    recorded_prompt: Option<crate::rollout::PromptSnapshot>,
    /// Subagent sequence number (agent_id = a{timestamp}-{agent_seq+1}; the
    /// timestamp keeps it from colliding with persisted subagent files across
    /// restarts)
    agent_seq: u64,
    /// Monotonic allocator for internal per-invocation tool ids (tool cards'
    /// `item_id`, approval/request ids). Provider tool-call ids stay untouched
    /// on the wire and in history; this only keeps duplicate provider ids from
    /// aliasing two invocations.
    tool_item_seq: u64,
    /// Repeat-call breaker for the current turn (root turns reset it at turn
    /// start; a child drive starts clean). It holds no model-visible state:
    /// reminders ride the tool results and the trip ends the turn.
    repeat: repeat::RepeatBreaker,
    /// Root stream bookkeeping for early read execution (see `early`): calls
    /// whose input streamed during the current attempt, and the read-only ones
    /// already executing before the assistant message commits.
    streamed_calls: std::collections::BTreeMap<usize, early::StreamedCall>,
    early_tools: Vec<early::EarlyTool>,
    /// Root only: the runner's steer admission signal. A step that is still
    /// streaming its model request subscribes to it, so a steer can preempt
    /// the request instead of waiting for the step boundary.
    steer_signal: Option<tokio::sync::watch::Sender<u64>>,
    /// The one resolved runtime view of this workspace: effective config
    /// (trusted project layer included), workspace trust, project sandbox
    /// layer and provider/model endpoint capabilities. Root model resolution,
    /// subagent profile/model resolution and trust refresh all use this object;
    /// the runner and the session share it through an `Arc`, so a refresh
    /// replaces one atomic value instead of synchronizing copies.
    runtime: Option<Arc<crate::config::RuntimeConfig>>,
    /// Workspace trust decision (the same value that admits project hooks,
    /// MCP servers, skills, agent profiles, permission rules and the project
    /// config layer). The token names this canonical workspace, so it cannot
    /// be reused for another checkout.
    trust: crate::config::WorkspaceTrust,
    plugin_tools: crate::plugins::PluginToolset,
    plugin_tool_filter: crate::plugins::PluginToolFilter,
    /// MCP connection manager: lazily connected before the first step's
    /// sampling (None = not attempted yet); Arc-shared — parallel-group spawn
    /// tasks and background subagent tasks clone the owned handle to fetch
    /// tools; child processes are backstopped by kill_on_drop, reclaimed when
    /// the session drops
    mcp: Option<Arc<crate::mcp::McpManager>>,
    /// Lifecycle hook snapshot (user config + project file, loaded once at
    /// create/load like permissions)
    hooks: crate::hooks::HookSet,
    /// Contexts stashed by SessionStart/UserPromptSubmit hooks; drained at the
    /// next turn start into history as model-visible HookContext records
    hook_context_stash: Vec<String>,
    /// SessionStart hook chain already ran (once per Session instance)
    session_start_hook_ran: bool,
    /// "startup" | "resume": the SessionStart hook payload's source, set by
    /// the constructor that built this Session
    session_start_source: &'static str,
}

impl Session {
    /// The session id events, approvals and durable session rows belong to.
    /// A child agent identifies itself by `agent.id` but reports into its
    /// parent conversation.
    pub(crate) fn event_session_id(&self) -> &str {
        self.subagent
            .as_ref()
            .map_or(self.agent.id.as_str(), |subagent| {
                subagent.parent_session_id.as_str()
            })
    }

    pub(super) fn is_subagent(&self) -> bool {
        self.subagent.is_some()
    }
}
