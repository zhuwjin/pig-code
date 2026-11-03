use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
use pig_provider::{ChatMsg, ProviderEvent, ToolCall};
use pig_utils::paths::normalize_workspace_path;

mod approval;
mod compact;
mod effects;
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
mod runner;
mod subagent;
#[cfg(test)]
mod tests;
mod title;
mod turn;
mod stats;
mod usage;

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
use model::{meta_to_selection, retry_status, skills_state_with_trust, snapshot_trusted};
use operations::CancelledTool;
use outcome::StepOutcome;
pub(crate) use outcome::{
    GatedToolOutcome, MAX_PAUSE_CONTINUATIONS, SubagentOutcome, TurnOutcome, pause_limit_detail,
    pause_limit_error,
};
pub use pending::{
    PendingApprovals, PendingQuestions, drop_session_questions, reject_session_approvals,
    resolve_approval,
};
pub(crate) use pending::{
    PendingMailbox, PendingMessage, persist_pending, restore_pending, restored_queue_seq,
};
pub use runner::agent_loop;
pub use title::TITLE_PROMPT_MARKER;
pub(crate) use title::spawn_title_generation;
pub(crate) use turn::pointer_file_references;
pub(crate) use usage::{UsageAnchor, model_key, system_fingerprint};

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    history: Vec<ChatMsg>,
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
    turn_counter: u64,
    tracker: ChangeTracker,
    state: crate::task::SessionToolState,
    /// Memory of "always allowed in this session": (tool name, subject) —
    /// ExecCommand = command first word, Write/Edit = path. Ordinary approvals only:
    /// a sandbox widening and a high-risk command are never remembered here.
    always_allowed: HashSet<(String, String)>,
    /// Allow/deny rules: the user level (`{data_dir}/permissions.toml`) unioned
    /// with the project level (`.pigcode/permissions.toml`), loaded at session
    /// creation/replay and reloaded on a trust change. An
    /// `ApprovedForProject` decision under trust appends to the project file
    /// and extends this set live; an untrusted workspace does not offer that
    /// tier, because the next session would ignore the rule.
    permission_rules: crate::permissions::PermissionRules,
    /// Plan mode toggle (orthogonal to mode; flipped by the
    /// EnterPlanMode/ExitPlanMode tools and Op::SetPlanMode, persisted
    /// write-through to the store)
    plan_enabled: bool,
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
    /// Only the profile description used by root tool schemas; subagent
    /// spawning still resolves fresh profiles by name.
    profiles_summary: String,
    prompt_text: Option<String>,
    prompt_memory_enabled: Option<bool>,
    /// Skills-state + trust sentinel (see `skills_state_with_trust`); a change
    /// in either forces the next turn to rebuild the system prompt.
    prompt_skills_state: Option<String>,
    projected_input_image: Option<bool>,
    projected_input_pdf: Option<bool>,
    recorded_prompt: Option<crate::rollout::PromptSnapshot>,
    last_total_tokens: Option<u64>,
    /// Durable identity of the turn currently running (shared by StepUsage and
    /// TurnStats). None outside a turn.
    usage_turn_id: Option<String>,
    /// Request/usage baseline refreshed after each persisted assistant step.
    /// Preflight projects the next request from it so tool results appended
    /// after the previous response are counted before sampling.
    usage_anchor: Option<UsageAnchor>,
    /// The reactive context-overflow recovery budget for the current turn
    /// (one durable compaction + retry at most).
    overflow_recovered: bool,
    /// Token usage accumulated in the current turn (written to the turn_usage
    /// table at turn end)
    turn_input: u64,
    turn_cache_read: u64,
    turn_output: u64,
    /// Reasoning/thinking slice of turn_output (ProviderEvent::Usage's
    /// reasoning_output accumulated; subagent usage tuples don't carry the
    /// split, so child runs contribute 0)
    turn_reasoning_output: u64,
    /// Model time accumulated in the current turn: each step from its start to
    /// the stream's end (the harness's `step/start` → message basis). The decode
    /// window is `turn_api_ms - turn_ttft_ms`.
    turn_api_ms: u64,
    /// Time spent waiting for the first output token within turn_api_ms (time
    /// to first token; accumulated across multi-step calls)
    turn_ttft_ms: u64,
    /// Steps that recorded a first token (the average TTFT divisor)
    turn_ttft_steps: u64,
    /// Provider request count within the turn (failed attempts included)
    turn_api_steps: u64,
    /// Summed per-call tool spans in the current turn: permission gate +
    /// approval wait + execution (concurrent read-only calls overlap, so this
    /// is a sum of spans, not the segment's wall time)
    turn_tool_ms: u64,
    stats_turns: Vec<pig_protocol::PluginTurnStats>,
    turn_active: bool,
    rate_started_at: Option<Instant>,
    rate_characters: u64,
    /// Session totals (restored on replay): uncached input / cache-hit input,
    /// used for the average cache hit rate
    input_total: u64,
    cache_read_total: u64,
    /// Subagent sequence number (agent_id = a{timestamp}-{agent_seq+1}; the
    /// timestamp keeps it from colliding with persisted subagent files across
    /// restarts)
    agent_seq: u64,
    /// Monotonic allocator for internal per-invocation tool ids (tool cards'
    /// `item_id`, approval/request ids). Provider tool-call ids stay untouched
    /// on the wire and in history; this only keeps duplicate provider ids from
    /// aliasing two invocations.
    tool_item_seq: u64,
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
