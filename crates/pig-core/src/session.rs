use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use pig_protocol::{ApprovalDecision, CoreError, Event, ExecMode, Op, SessionMeta};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::config;
use crate::paths::normalize_workspace_path;
use crate::provider::ResolvedModel;
use crate::provider::{ChatMsg, ProviderEvent, ToolCall};
use crate::rollout::{Rollout, RolloutRecord, now_secs, rebuild_history};
use crate::store::Store;
use crate::tool::{ChangeTracker, ToolContext};
use crate::{prompt, provider, tool};
use pig_protocol::AppConfig;

/// Pending approvals: request_id → reply channel. Shared between the manager
/// and every session; request_ids carry a session_id prefix and are globally
/// unique.
/// Approval coalescing key: (tool name, full command/path, whether a danger
/// popup). Concurrent waiters with the same key share one decision (ten Swarm
/// subagents all running `sleep 5` are answered once); the full command is
/// part of the key rather than the first word, so allowing `sleep 5` does not
/// also allow same-first-word but different commands like `sleep 100`.
pub type ApprovalCoalesceKey = (String, String, bool);
/// Approval wait table: request_id → (reply channel, coalescing key);
/// requests with a None key (the ExitPlanMode plan confirmation) do not
/// coalesce and are resolved by their own request_id alone. The channel
/// payload carries optional feedback (kimi Revise: a plan rejection can carry
/// text for the model to revise with; same-key fan-out recipients get the
/// decision only, no feedback)
pub type PendingApprovals = Arc<
    Mutex<
        HashMap<
            String,
            (
                oneshot::Sender<(ApprovalDecision, Option<String>)>,
                Option<ApprovalCoalesceKey>,
            ),
        >,
    >,
>;

/// Resolve one approval: wake the waiter for this request_id and also wake
/// concurrent waiters sharing the same coalescing key (the Op::ApprovalReply
/// handling path). The UI approval bar can show only one entry at a time;
/// when several Swarm subagents concurrently await approval for the same
/// command, the displaced waiter would never be answered without the fan-out.
/// feedback is delivered only with this request_id (plan revision comments);
/// same-key fan-out recipients get None
pub fn resolve_approval(
    pending: &PendingApprovals,
    request_id: &str,
    decision: ApprovalDecision,
    feedback: Option<String>,
) {
    let mut pending = pending.lock().expect("pending lock");
    let coalesce = pending.get(request_id).and_then(|(_, key)| key.clone());
    if let Some((reply, _)) = pending.remove(request_id) {
        let _ = reply.send((decision, feedback));
    }
    let Some(key) = coalesce else { return };
    let same_key: Vec<String> = pending
        .iter()
        .filter(|(_, (_, other))| other.as_ref() == Some(&key))
        .map(|(id, _)| id.clone())
        .collect();
    for id in same_key {
        if let Some((reply, _)) = pending.remove(&id) {
            let _ = reply.send((decision, None));
        }
    }
}

/// Pending structured questions: request_id → reply channel. None = the user
/// skipped; outer level per question, inner level holds the selected labels
/// for that question ("other" free text is placed in as a label verbatim).
pub type PendingQuestions = Arc<Mutex<HashMap<String, oneshot::Sender<Option<Vec<Vec<String>>>>>>>;

/// Session-level model override (SetModel)
#[derive(Clone, Debug, Default)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning_level: Option<String>,
}

/// Model selection persisted in SessionMeta → ModelSelection (missing
/// provider/model means no override)
fn meta_to_selection(meta: &SessionMeta) -> Option<ModelSelection> {
    match (&meta.provider_id, &meta.model_id) {
        (Some(provider_id), Some(model_id)) => Some(ModelSelection {
            provider_id: provider_id.clone(),
            model_id: model_id.clone(),
            reasoning_level: meta.reasoning_level.clone(),
        }),
        _ => None,
    }
}

/// provider+model(+reasoning level) → endpoint resolution.
/// `default_level`: the thinking level to use when there is no model override
/// (applies to the configured default model).
pub fn resolve_model(
    config: &AppConfig,
    selection: Option<&ModelSelection>,
    default_level: Option<&str>,
) -> Option<ResolvedModel> {
    let (provider_id, model_id, level) = match selection {
        Some(sel) => (
            sel.provider_id.clone(),
            sel.model_id.clone(),
            sel.reasoning_level.clone(),
        ),
        None => (
            config.default_provider.clone(),
            config.default_model.clone(),
            default_level.map(str::to_string),
        ),
    };
    let provider = config
        .providers
        .iter()
        .find(|p| p.enabled && p.id == provider_id)
        .or_else(|| config.providers.iter().find(|p| p.enabled))?;
    let model = provider
        .models
        .iter()
        .find(|m| m.id == model_id)
        .or_else(|| provider.models.first())?;
    let reasoning_params = level
        .as_ref()
        .and_then(|level| model.reasoning_params.get(level).cloned());
    Some(ResolvedModel {
        base_url: provider.base_url.clone(),
        api_key: config::expand_env(&provider.api_key),
        model: model.id.clone(),
        context_window: model.context_window,
        max_output_tokens: model.max_output_tokens,
        api_format: provider.api_format,
        reasoning_params,
        cap_web_search: model.cap_web_search,
        web_search_tool: model.web_search_tool.clone(),
        input_image: model.input_image,
        provider_name: provider.name.clone(),
    })
}

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    history: Vec<ChatMsg>,
    /// Full input projection of the previous model-io trace (the prefix
    /// baseline for delta persistence; empty after resume — the first entry
    /// automatically falls back to full, self-healing)
    io_last_input: Vec<crate::model_io::ModelIoMessage>,
    /// Event sequence number (Arc-shared: background subagent tasks emit
    /// events via emit_bg on the same counter)
    seq: Arc<std::sync::atomic::AtomicU64>,
    turn_counter: u64,
    tracker: ChangeTracker,
    state: crate::task::SessionToolState,
    /// Memory of "always allowed in this session": (tool name, subject) —
    /// Bash = command first word, Write/Edit = path
    always_allowed: HashSet<(String, String)>,
    /// Project-level allow/deny rules (.pigcode/permissions.toml, loaded once
    /// at session creation/replay)
    permissions: crate::permissions::PermissionRules,
    /// Plan mode toggle (orthogonal to mode; flipped by the
    /// EnterPlanMode/ExitPlanMode tools and Op::SetPlanMode, persisted
    /// write-through to the store)
    plan_enabled: bool,
    pending: PendingApprovals,
    pending_questions: PendingQuestions,
    mode: ExecMode,
    model_override: Option<ModelSelection>,
    rollout: Option<Rollout>,
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
    /// Date frozen at session start (displayed in the env block); corrected
    /// via turn_reminder on date rollover
    date_frozen: String,
    /// Last-reminded date/AGENTS.md content (turn_reminder dedup: remind only
    /// when different from the frozen value; identical content is not
    /// re-injected)
    date_reminded: String,
    agents_reminded: String,
    /// Last-reminded exec mode (turn_reminder dedup: the first turn with None
    /// always reminds once; afterwards only the first turn after a mode switch
    /// reminds again; not persisted, self-heals on the first turn after
    /// resume)
    mode_reminded: Option<(ExecMode, bool)>,
    /// Last out-of-workspace toggle state the model was reminded of (the
    /// turn_reminder baseline is both-off)
    fs_reminded: (bool, bool),
    /// Subagent profile snapshot frozen at session start (for the profile
    /// list embedded in the Agent/AgentSwarm tool descriptions — rescanning
    /// every step would let editing a profile break the tools prefix cache;
    /// spawning reads fresh via load_profiles, and a stale listing self-heals
    /// through errors)
    profiles_snapshot: Vec<crate::agent::AgentProfile>,
    last_total_tokens: Option<u64>,
    /// Token usage accumulated in the current turn (written to the turn_usage
    /// table at turn end)
    turn_input: u64,
    turn_cache_read: u64,
    turn_output: u64,
    /// Reasoning/thinking slice of turn_output (ProviderEvent::Usage's
    /// reasoning_output accumulated; subagent usage tuples don't carry the
    /// split, so child runs contribute 0)
    turn_reasoning_output: u64,
    /// Pure API time accumulated in the current turn (provider requests only,
    /// excluding tool execution/approval waits; token speed is computed from
    /// it, so a long-running command does not drag the speed down)
    turn_api_ms: u64,
    /// Time spent waiting for the first output token within turn_api_ms (time
    /// to first token; accumulated across multi-step calls)
    turn_ttft_ms: u64,
    /// Provider request count within the turn (average TTFT = turn_ttft_ms /
    /// turn_api_steps)
    turn_api_steps: u64,
    /// Session totals (restored on replay): uncached input / cache-hit input,
    /// used for the average cache hit rate
    input_total: u64,
    cache_read_total: u64,
    /// Subagent sequence number (agent_id = a{timestamp}-{agent_seq+1}; the
    /// timestamp keeps it from colliding with persisted subagent files across
    /// restarts)
    agent_seq: u64,
    /// App config snapshot (for resolving explicit subagent models; None =
    /// not loaded, only inheritance available)
    app_config: Option<AppConfig>,
    /// MCP connection manager: lazily connected before the first step's
    /// sampling (None = not attempted yet); Arc-shared — parallel-group spawn
    /// tasks and background subagent tasks clone the owned handle to fetch
    /// tools; child processes are backstopped by kill_on_drop, reclaimed when
    /// the session drops
    mcp: Option<Arc<crate::mcp::McpManager>>,
}

enum StepOutcome {
    TextOnly,
    ToolsExecuted,
    Ended,
}

/// Event emission from free paths (background subagent tasks/gated free
/// functions): seq increments atomically, sharing the same Arc counter with
/// Session::emit (foreground/background event seqs stay ordered).
/// Coalesce-key value for an out-of-workspace access kind ("read"/"write"):
/// parallel same-kind outside requests share one popup
fn fs_kind_key(access: tool::FsAccess) -> String {
    match access {
        tool::FsAccess::Read => "read".to_string(),
        tool::FsAccess::Write => "write".to_string(),
    }
}

pub(crate) fn emit_bg(
    session_id: &str,
    seq: &std::sync::atomic::AtomicU64,
    tx: &async_channel::Sender<Event>,
    build: impl FnOnce(String, u64) -> Event,
) {
    let next = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let _ = tx.send_blocking(build(session_id.to_string(), next));
}

/// Context for gated execution (the argument bundle of exec_tool_gated_ctx):
/// the Session wrapper assembles it by borrowing fields, background subagent
/// tasks assemble it from owned snapshots/Arc clones — the same gate runs in
/// foreground and background.
pub(crate) struct GateCtx<'a> {
    pub cwd: &'a Path,
    pub mode: ExecMode,
    pub tracker: &'a mut ChangeTracker,
    pub state: &'a crate::task::SessionToolState,
    pub pending: &'a PendingApprovals,
    pub permissions: &'a crate::permissions::PermissionRules,
    pub always_allowed: &'a mut HashSet<(String, String)>,
    pub session_id: &'a str,
    /// Plan mode toggle: when Write/Edit writes the plan file (kimi
    /// writesOnlyPlanFile), the approval gate passes it through approval-free
    /// (path check in tool::is_plan_file_write)
    pub plan_enabled: bool,
    pub seq: &'a std::sync::atomic::AtomicU64,
    pub store: &'a Arc<Mutex<Store>>,
    /// Runtime tools beyond the built-ins (MCP): the lookup fallback by name
    /// in the execution segment; the root session passes all connected MCP
    /// tools, subagents pass the subset narrowed by profile inheritance rules
    pub extra_tools: &'a [Box<dyn tool::Tool>],
}

/// Free-function implementation of gated execution (shared by
/// Session::exec_tool_gated and the background subagent drive): danger
/// blocklist → permissions deny/allow → AutoEdit read-only pass-through →
/// approval gate → execution → session-level side effects (TodoList snapshot
/// persisted+pushed, FileChanged persisted+pushed, file_originals persisted).
/// ToolCallBegin/history.push/RolloutRecord::ToolCall/ToolCallEnd are not
/// here — the caller owns them (parent and child each write their own
/// history and rollout). When `tool` is None (unknown tool name), the
/// approval/permission name matches all miss and flow reaches the execution
/// segment, which reports "unknown tool".
pub(crate) async fn exec_tool_gated_ctx(
    ctx: &mut GateCtx<'_>,
    call: &ToolCall,
    tool: Option<&dyn tool::Tool>,
    item_id: &str,
    turn_id: &str,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> GatedToolOutcome {
    // Dangerous commands hitting the blocklist force a popup (same as ZCode
    // alwaysAsk): every mode except Yolo pops, and always_allowed does not
    // apply to them; in plan mode the caller already hard-rejects the whole
    // category, so this is not reached. Yolo (container/sandbox, unregulated)
    // skips even the danger check and never pops up.
    let bash_command = if call.name == "Bash" {
        serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|args| args["command"].as_str().map(str::to_string))
    } else {
        None
    };

    // Project rules (.pigcode/permissions.toml): a deny hit → hard rejection
    // in all modes (including Yolo), checked before the danger popup (an
    // explicit user-written deny is the strongest intent). Note that Bash
    // matches the full command string (finer-grained than always_allowed's
    // first word).
    let perm_subject = match call.name.as_str() {
        "Bash" => bash_command.clone(),
        "Write" | "Edit" => Some(tool::approval_subject(call)),
        // MCP tools use the full name as subject: the project rule
        // `mcp__fs__write(*)` can hit
        name if name.starts_with("mcp__") => Some(call.name.clone()),
        _ => None,
    };
    if let Some(subject) = perm_subject.as_deref()
        && let Some(rule) = ctx.permissions.deny_hit(&call.name, subject)
    {
        let note = format!("Blocked by a project rule: {rule} (.pigcode/permissions.toml)");
        return GatedToolOutcome::Rejected { note };
    }
    // An allow hit skips approval (except dangerous commands — the danger
    // check below takes priority with its popup)
    let allowed_by_rules = perm_subject
        .as_deref()
        .is_some_and(|subject| ctx.permissions.allow_hit(&call.name, subject));

    let danger_reason = if ctx.mode == ExecMode::Yolo {
        None
    } else {
        bash_command.as_deref().and_then(tool::is_dangerous_command)
    };
    // AutoEdit passes through read-only commands on the conservative
    // allowlist (ls/git status and the like); the danger check above takes
    // priority. File-dumping commands (cat etc.) get an argument-level check
    // here: sensitive/out-of-bounds targets are not allowed through
    let readonly_bash = ctx.mode == ExecMode::AutoEdit
        && bash_command
            .as_deref()
            .is_some_and(|command| tool::is_readonly_command(command, ctx.cwd));
    // "Always allowed in this session" is keyed down to (tool, subject):
    // Bash = command first word, Write/Edit = path
    let approval_key = (call.name.clone(), tool::approval_subject(call));

    // Out-of-workspace file access with the session toggles off: an approval
    // request instead of a hard error — Allow runs this one call (the per-call
    // fs_grant on ToolContext), AlwaysAllow covers the same access kind for
    // the rest of the session (the same session memory as other tools'),
    // Reject returns the boundary error as a tool error. Applies in every
    // mode: leaving the workspace is a scope change worth one click even under
    // FullAccess/Yolo. tmp and the extra read roots stay exempt; the
    // sensitive-file filter runs after resolve regardless of the grant.
    let fs_outside = tool::fs_outside_intent(ctx.state, ctx.cwd, &call.name, &call.arguments);
    let needs_normal = tool.is_some_and(|t| tool::requires_approval(t, ctx.mode))
        && !ctx.always_allowed.contains(&approval_key)
        && !readonly_bash
        && !allowed_by_rules
        // kimi writesOnlyPlanFile: in plan mode, writing the plan file
        // passes through approval-free
        && !(ctx.plan_enabled && tool::is_plan_file_write(ctx.cwd, &call.arguments));

    if danger_reason.is_some() || fs_outside.is_some() || needs_normal {
        let request_id = format!("{}-{turn_id}-approval-{item_id}", ctx.session_id);
        // Bash detail = the bare command; the danger warning goes through
        // danger_key (the GUI localizes the title line by key). An
        // out-of-workspace request leads with its own access line instead.
        let detail_text = match &fs_outside {
            Some((access, path)) => format!(
                "{} outside the workspace: {path}",
                match access {
                    tool::FsAccess::Read => "Read",
                    tool::FsAccess::Write => "Write",
                }
            ),
            None => approval_detail(call, ctx.cwd),
        };
        // The coalescing key reuses perm_subject (Bash = full command,
        // Write/Edit = path, MCP = tool name); tools without a subject do not
        // coalesce (conservative), and the danger flag is part of the key —
        // normal popups never coalesce into danger popups. Out-of-workspace
        // requests coalesce by access kind (parallel same-kind outside reads
        // share one popup)
        let coalesce_key = match &fs_outside {
            Some((access, _)) => Some(("fs-outside".to_string(), fs_kind_key(*access), false)),
            None => perm_subject
                .clone()
                .map(|subject| (call.name.clone(), subject, danger_reason.is_some())),
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        ctx.pending
            .lock()
            .expect("pending lock")
            .insert(request_id.clone(), (reply_tx, coalesce_key));
        let danger_key = danger_reason.map(|reason| reason.key.to_string());
        emit_bg(ctx.session_id, ctx.seq, tx, |session_id, seq| {
            Event::ApprovalRequested {
                session_id,
                seq,
                request_id: request_id.clone(),
                tool: call.name.clone(),
                detail: detail_text.clone(),
                danger_key,
            }
        });
        let (decision, _feedback) = tokio::select! {
            reply = reply_rx => reply.unwrap_or((ApprovalDecision::Reject, None)),
            _ = cancel.cancelled() => {
                ctx.pending.lock().expect("pending lock").remove(&request_id);
                return GatedToolOutcome::Cancelled;
            }
        };
        match decision {
            ApprovalDecision::Allow => {}
            ApprovalDecision::AlwaysAllow => {
                // Dangerous commands are not recorded in always_allowed:
                // allowed this time only, equivalent to Allow
                if danger_reason.is_none() {
                    if let Some((access, _)) = &fs_outside {
                        // "Always this session" = flipping the session toggle
                        // itself — the same switch the composer mode menu
                        // shows: persisted to meta (resumed sessions keep it),
                        // and FsAccessChanged lets the UI check the box
                        match access {
                            tool::FsAccess::Read => ctx
                                .state
                                .fs_read_outside
                                .store(true, std::sync::atomic::Ordering::Relaxed),
                            tool::FsAccess::Write => ctx
                                .state
                                .fs_write_outside
                                .store(true, std::sync::atomic::Ordering::Relaxed),
                        }
                        let read_outside = ctx
                            .state
                            .fs_read_outside
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let write_outside = ctx
                            .state
                            .fs_write_outside
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let session_id = ctx.session_id.to_string();
                        if let Ok(store) = ctx.store.lock() {
                            store.update_session(&session_id, |meta| {
                                meta.fs_read_outside = read_outside;
                                meta.fs_write_outside = write_outside;
                                meta.updated_at = now_secs();
                            });
                        }
                        emit_bg(ctx.session_id, ctx.seq, tx, |session_id, seq| {
                            Event::FsAccessChanged {
                                session_id,
                                seq,
                                read_outside,
                                write_outside,
                            }
                        });
                    }
                    if needs_normal {
                        ctx.always_allowed.insert(approval_key);
                    }
                }
            }
            ApprovalDecision::Reject => {
                let note = if let Some((access, path)) = &fs_outside {
                    format!(
                        "The user declined {} outside the workspace ({path}). Respect their decision; do not route around it via the shell, and continue within the workspace.",
                        fs_kind_key(*access)
                    )
                } else {
                    match danger_reason {
                        Some(reason) => format!(
                        "The user rejected this high-risk command ({}). Respect their decision; use a different approach, or explain why it is needed before continuing.",
                        reason.en
                    ),
                        None => "The user rejected this action. Respect their decision; use a different approach, or explain why it is needed before continuing."
                            .to_string(),
                    }
                };
                return GatedToolOutcome::Rejected { note };
            }
        }
    }

    let result = {
        let tool_ctx = ToolContext {
            cwd: ctx.cwd,
            tracker: ctx.tracker,
            state: ctx.state,
            fs_grant: fs_outside.map(|(access, _)| access),
        };
        tokio::select! {
            result = tool::execute_with_extra(call, tool_ctx, ctx.extra_tools) => Some(result),
            _ = cancel.cancelled() => None,
        }
    };
    let Some((output, is_error, file_change, edit, images)) = result else {
        return GatedToolOutcome::Cancelled;
    };
    // Images enter the model context via history (Anthropic blocks / OpenAI
    // split user messages); the rollout ToolCall record stores only the
    // output text (size summary included), base64 is not persisted
    let chat_images = tool_images_to_chat(&call.arguments, images);
    // After a successful TodoList write, push the todo snapshot to the UI
    // (reads output the list itself; no need to push again)
    if call.name == "TodoList" && !is_error {
        let items = ctx.state.todos.lock().expect("todos lock").clone();
        // Write operations (with a todos argument) persist to the SQLite
        // todos table (upsert of current state); reads do not persist. The
        // event-stream JSONL no longer records state snapshots
        let is_write = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .is_some_and(|v| v.get("todos").is_some());
        if is_write {
            let json = serde_json::to_string(&items).unwrap_or_default();
            ctx.store
                .lock()
                .expect("store lock")
                .set_todos(ctx.session_id, &json);
        }
        emit_bg(ctx.session_id, ctx.seq, tx, |session_id, seq| {
            Event::TodoListChanged {
                session_id,
                seq,
                items,
            }
        });
    }
    if let Some(change) = file_change {
        // Current change state persists to the SQLite file_changes table
        // (upsert by path; rows deleted when the net delta reaches zero);
        // the JSONL keeps only the message/tool event stream
        {
            let store = ctx.store.lock().expect("store lock");
            if change.additions == 0 && change.deletions == 0 {
                store.delete_file_change(ctx.session_id, &change.path);
            } else {
                store.upsert_file_change(
                    ctx.session_id,
                    &change.path,
                    &change.unified_diff,
                    change.additions,
                    change.deletions,
                );
            }
        }
        emit_bg(ctx.session_id, ctx.seq, tx, |session_id, seq| {
            Event::FileChanged {
                session_id,
                seq,
                path: change.path,
                unified_diff: change.unified_diff,
                additions: change.additions,
                deletions: change.deletions,
            }
        });
    }
    // Original snapshots newly added by this tool persist to the
    // file_originals table (cross-restart diff baseline / revert); files over
    // 4MB are not persisted (the baseline falls back to process memory,
    // consistent with kimi-code)
    let dirty = ctx.tracker.take_dirty();
    if !dirty.is_empty() {
        const MAX_ORIGINAL_BYTES: usize = 4 * 1024 * 1024;
        let store = ctx.store.lock().expect("store lock");
        for path in dirty {
            if let Some(original) = ctx.tracker.original(&path) {
                let oversized = original
                    .as_ref()
                    .is_some_and(|content| content.len() > MAX_ORIGINAL_BYTES);
                if !oversized {
                    store.upsert_file_original(
                        ctx.session_id,
                        &path.to_string_lossy(),
                        original.as_deref(),
                    );
                }
            }
        }
    }
    GatedToolOutcome::Executed {
        output,
        is_error,
        edit,
        images: chat_images,
    }
}

/// ToolImage → ChatImage: the label comes from the path in the call arguments
/// (shared by the gated path and the parallel read-only segment).
fn tool_images_to_chat(
    arguments: &str,
    images: Vec<tool::ToolImage>,
) -> Vec<crate::provider::ChatImage> {
    let label = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v["path"].as_str().map(str::to_string));
    images
        .into_iter()
        .map(|img| crate::provider::ChatImage {
            media_type: img.media_type,
            data_base64: img.data_base64,
            label: label.clone(),
        })
        .collect()
}

/// Result of gated tool execution (exec_tool_gated)
pub(crate) enum GatedToolOutcome {
    /// Executed; the caller owns history/rollout/ToolCallEnd (parent and
    /// child each write their own)
    Executed {
        output: String,
        is_error: bool,
        /// Diff of this edit (for inline rendering in the parent session's
        /// tool card; ignored on the subagent path)
        edit: Option<pig_protocol::EditDiff>,
        images: Vec<crate::provider::ChatImage>,
    },
    /// Rejected by approval/rules (note is the message for the model); the
    /// caller owns history/rollout/ToolCallEnd
    Rejected { note: String },
    /// Cancelled (interrupted while awaiting approval or executing); the
    /// caller decides TurnAborted and the wrap-up
    Cancelled,
}

/// Result of subagent delegation (run_subagent/run_swarm)
pub(crate) enum SubagentOutcome {
    /// The subagent has finished (success or failure is in the note; a
    /// background dispatch yields an immediate receipt); the caller owns
    /// history/rollout/ToolCallEnd. card = single agent card metadata (the
    /// Agent path; persisted with the rollout ToolCall record, replay
    /// rebuilds the agent card); cards = batch agent cards (the AgentSwarm
    /// path, one per subagent). Early exits from argument/profile/model
    /// resolution failure have no agent_id, leaving both empty
    Finished {
        note: String,
        is_error: bool,
        card: Option<crate::rollout::AgentCardRecord>,
        cards: Vec<crate::rollout::AgentCardRecord>,
    },
    /// Cancelled (interrupted while awaiting approval or sampling/executing);
    /// the caller emits TurnAborted and wraps up. card/cards = agent card
    /// metadata (cancellation is likewise persisted with the rollout; replay
    /// rebuilds the agent cards)
    Cancelled {
        card: Option<crate::rollout::AgentCardRecord>,
        cards: Vec<crate::rollout::AgentCardRecord>,
    },
}

/// Argument bundle for settle_cancelled_tool: the currently cancelled call +
/// the remaining unexecuted calls of the same response.
struct CancelledTool<'a> {
    call: &'a crate::provider::ToolCall,
    /// Tool card summary (persisted with the rollout record)
    summary: String,
    item_id: &'a str,
    /// Calls after the current one in the same response (will not execute;
    /// empty receipts are backfilled to keep tool_use pairing)
    rest: &'a [crate::provider::ToolCall],
    /// Agent card metadata (the Agent tool's cancellation path; None for
    /// other tools)
    card: Option<crate::rollout::AgentCardRecord>,
    /// Batch agent card metadata (the AgentSwarm tool's cancellation path; an
    /// empty list for other tools)
    cards: Vec<crate::rollout::AgentCardRecord>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        meta: SessionMeta,
        pending: PendingApprovals,
        pending_questions: PendingQuestions,
        store: Arc<Mutex<Store>>,
        sessions_dir: &Path,
        data_dir: PathBuf,
        task_notify: tokio::sync::mpsc::UnboundedSender<String>,
        wake_notify: tokio::sync::mpsc::UnboundedSender<(String, String)>,
        app_config: Option<&AppConfig>,
    ) -> Result<Self, CoreError> {
        let rollout = Rollout::create(sessions_dir, &meta)?;
        // data_dir is moved into the literal, so compute the frozen snapshots
        // as locals first (the directory's state at creation time)
        let skills_prompt = crate::skills::skills_section(&meta.cwd, &data_dir);
        let agents_prompt = prompt::agents_md(&data_dir, &meta.cwd);
        let profiles_snapshot = crate::agent::load_profiles(&meta.cwd, &data_dir);
        let today = prompt::today();
        Ok(Self {
            id: meta.id.clone(),
            cwd: meta.cwd.clone(),
            history: Vec::new(),
            io_last_input: Vec::new(),
            seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_counter: 0,
            tracker: ChangeTracker::default(),
            state: crate::task::SessionToolState::new(
                meta.id,
                task_notify,
                wake_notify,
                // The sessions subtree (subagent result.md/context jsonl) is
                // always readable
                vec![data_dir.join("sessions")],
            ),
            always_allowed: HashSet::new(),
            permissions: load_permissions(&meta.cwd),
            plan_enabled: false,
            pending,
            pending_questions,
            mode: ExecMode::ConfirmBeforeEdit,
            model_override: None,
            rollout: Some(rollout),
            store,
            data_dir,
            git_snapshot: prompt::git_snapshot(&meta.cwd),
            agents_prompt: agents_prompt.clone(),
            skills_prompt,
            date_frozen: today.clone(),
            date_reminded: today,
            agents_reminded: agents_prompt,
            mode_reminded: None,
            fs_reminded: (false, false),
            profiles_snapshot,
            last_total_tokens: None,
            turn_input: 0,
            turn_cache_read: 0,
            turn_output: 0,
            turn_reasoning_output: 0,
            turn_api_ms: 0,
            turn_ttft_ms: 0,
            turn_api_steps: 0,
            input_total: 0,
            cache_read_total: 0,
            agent_seq: 0,
            app_config: app_config.cloned(),
            mcp: None,
        })
    }

    /// Rebuild from rollout. Diff baselines are restored from the
    /// file_originals table into ChangeTracker: after resume, changes are
    /// still computed as "session's first snapshot → current", so revert
    /// works across restarts.
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        id: &str,
        sessions_dir: &Path,
        pending: PendingApprovals,
        pending_questions: PendingQuestions,
        store: Arc<Mutex<Store>>,
        data_dir: PathBuf,
        task_notify: tokio::sync::mpsc::UnboundedSender<String>,
        wake_notify: tokio::sync::mpsc::UnboundedSender<(String, String)>,
        app_config: Option<&AppConfig>,
    ) -> Result<(Self, Vec<RolloutRecord>), CoreError> {
        let mut records = Rollout::load(&sessions_dir.join(format!("{id}.jsonl")))?;
        let Some(RolloutRecord::Meta { cwd, .. }) = records.first() else {
            return Err(CoreError::RolloutNoMeta { id: id.to_string() });
        };
        let cwd = cwd.clone();
        // A resumed session re-freezes the skill listing/AGENTS.md/date/
        // subagent profiles (per the directory's state at resume time)
        let skills_prompt = crate::skills::skills_section(&cwd, &data_dir);
        let agents_prompt = prompt::agents_md(&data_dir, &cwd);
        let profiles_snapshot = crate::agent::load_profiles(&cwd, &data_dir);
        let today = prompt::today();
        let history = rebuild_history(
            &records,
            // Placeholder system prompt: the first run_turn overwrites it
            // wholly with the frozen snapshot
            prompt::system_prompt(&cwd, true, None, &today, &agents_prompt, &skills_prompt),
        );
        let originals = store
            .lock()
            .expect("store lock")
            .file_originals(id)
            .into_iter()
            .map(|(path, content)| (PathBuf::from(path), content))
            .collect();
        let mut tracker = ChangeTracker::default();
        tracker.restore(originals);
        // After resume, take over the same JSONL and keep appending; failure
        // must be loud (setting None would silently drop all subsequent
        // records)
        let mut rollout = Rollout::open_append(sessions_dir, id)?;
        // Crash recovery: a hard kill runs no wrap-up, so the tail turn's
        // usage never reached turn_usage/TurnStats (the live interrupt path
        // writes it at cancel time, so this only fires for uncounted tails).
        // Per-request StepUsage records are durable — sum the tail turn's
        // steps, count them, and append a synthesized TurnStats as the
        // "counted" marker (reopening sees it and skips; replay restores the
        // footer/session totals through the normal path). Timing fields are
        // unknowable after a crash (zeros) and the reasoning split is not in
        // StepUsage (0); the in-flight request's usage is lost either way.
        if let Some((input, cache_read, output)) = uncounted_tail_usage(&records) {
            let recovered = RolloutRecord::TurnStats {
                input,
                cache_read,
                output,
                duration_ms: 0,
                api_ms: 0,
                ttft_ms: 0,
                api_steps: 0,
            };
            rollout.append(&recovered);
            // Attribute to the session's last used model (meta is
            // write-through on SetModel); the rollout file's mtime
            // approximates the turn's time (the recovery moment would skew
            // by-day statistics)
            let meta = store.lock().expect("store lock").get_session(id);
            let resolved = app_config.and_then(|config| {
                resolve_model(
                    config,
                    meta.as_ref().and_then(meta_to_selection).as_ref(),
                    None,
                )
            });
            let (provider, model) = match resolved {
                Some(resolved) => (resolved.provider_name, resolved.model),
                None => {
                    let unknown = || "unknown".to_string();
                    meta.as_ref()
                        .map(|m| {
                            (
                                m.provider_id.clone().unwrap_or_else(unknown),
                                m.model_id.clone().unwrap_or_else(unknown),
                            )
                        })
                        .unwrap_or_else(|| (unknown(), unknown()))
                }
            };
            let ts = std::fs::metadata(sessions_dir.join(format!("{id}.jsonl")))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or_else(now_secs);
            store
                .lock()
                .expect("store lock")
                .record_usage_at(ts, id, &provider, &model, input, cache_read, output, 0);
            records.push(recovered);
        }
        let session = Self {
            id: id.to_string(),
            cwd: cwd.clone(),
            history,
            io_last_input: Vec::new(),
            seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_counter: 0,
            tracker,
            state: crate::task::SessionToolState::new(
                id.to_string(),
                task_notify,
                wake_notify,
                // The sessions subtree (subagent result.md/context jsonl) is
                // always readable
                vec![data_dir.join("sessions")],
            ),
            always_allowed: HashSet::new(),
            permissions: load_permissions(&cwd),
            plan_enabled: false,
            pending,
            pending_questions,
            mode: ExecMode::ConfirmBeforeEdit,
            model_override: None,
            rollout: Some(rollout),
            store,
            data_dir,
            git_snapshot: prompt::git_snapshot(&cwd),
            agents_prompt: agents_prompt.clone(),
            skills_prompt,
            date_frozen: today.clone(),
            date_reminded: today,
            agents_reminded: agents_prompt,
            mode_reminded: None,
            fs_reminded: (false, false),
            profiles_snapshot,
            last_total_tokens: None,
            turn_input: 0,
            turn_cache_read: 0,
            turn_output: 0,
            turn_reasoning_output: 0,
            turn_api_ms: 0,
            turn_ttft_ms: 0,
            turn_api_steps: 0,
            input_total: 0,
            cache_read_total: 0,
            agent_seq: 0,
            app_config: app_config.cloned(),
            mcp: None,
        };
        Ok((session, records))
    }

    /// Root session tool set: built-in + Agent/AgentSwarm + MCP. The subagent
    /// loop uses tool::all() narrowed + MCP inheritance rules, naturally
    /// without Agent/AgentSwarm to prevent nesting; profiles come from the
    /// session's frozen snapshot and the MCP listing from the lazy-connection
    /// snapshot — tools sit at the very front of the cache prefix, so bytes
    /// stay stable within the session
    pub(crate) fn root_tools(&self) -> Vec<Box<dyn tool::Tool>> {
        let mut tools = tool::all_root(&self.cwd, &self.data_dir, &self.profiles_snapshot);
        if let Some(mcp) = &self.mcp {
            tools.extend(mcp.tools());
        }
        tools
    }

    /// Root session tool schema set (for run_step sampling)
    pub(crate) fn root_schemas(&self) -> Vec<serde_json::Value> {
        self.root_tools().iter().map(|tool| tool.schema()).collect()
    }

    fn emit(
        &mut self,
        build: impl FnOnce(String, u64) -> Event,
        tx: &async_channel::Sender<Event>,
    ) {
        emit_bg(&self.id, &self.seq, tx, build);
    }

    fn record(&mut self, record: &RolloutRecord) {
        if let Some(rollout) = &mut self.rollout {
            rollout.append(record);
        }
    }

    /// Unified wrap-up for mid-tool cancellation (user hits stop): the
    /// current call gets a "Stopped" receipt — history (tool_use/tool_result
    /// pairing, so a dangling pair does not get rejected by the API on the
    /// next request), rollout (replay after restart rebuilds the card instead
    /// of it vanishing), ToolCallEnd (settles the live card); calls later in
    /// the same response will not execute, and receive receipts too so
    /// pairing stays complete.
    fn settle_cancelled_tool(
        &mut self,
        cancelled: CancelledTool<'_>,
        tx: &async_channel::Sender<Event>,
    ) {
        self.history.push(ChatMsg::tool_result(
            &cancelled.call.id,
            "Stopped".to_string(),
        ));
        for rest in cancelled.rest {
            self.history
                .push(ChatMsg::tool_result(&rest.id, "Stopped".to_string()));
        }
        self.record(&RolloutRecord::ToolCall {
            tool: cancelled.call.name.clone(),
            summary: cancelled.summary,
            arguments: cancelled.call.arguments.clone(),
            output: "Stopped".to_string(),
            is_error: false,
            edit: None,
            agent_card: cancelled.card,
            agent_cards: cancelled.cards,
        });
        let item_id = cancelled.item_id.to_string();
        self.emit(
            |session_id, seq| Event::ToolCallEnd {
                session_id,
                seq,
                item_id,
                output: "Stopped".to_string(),
                is_error: false,
                edit: None,
            },
            tx,
        );
    }

    /// Turn wrap-up: produce "changes this turn" (persisted to rollout +
    /// pushed to the UI message-stream panel); nothing is emitted if there
    /// are no changes.
    fn flush_turn_changes(&mut self, tx: &async_channel::Sender<Event>) {
        let changes = self.tracker.take_turn_changes(&self.cwd);
        if changes.is_empty() {
            return;
        }
        let files: Vec<pig_protocol::EditDiff> = changes.into_iter().map(Into::into).collect();
        self.record(&RolloutRecord::TurnChanges {
            files: files.clone(),
        });
        self.emit(
            |session_id, seq| Event::TurnFileChanges {
                session_id,
                seq,
                files,
            },
            tx,
        );
    }

    /// Persist the turn's accumulated usage (SQLite turn_usage row for
    /// statistics aggregation + rollout TurnStats record for replay) and build
    /// the UI footer stats. Called on both the normal turn end and the
    /// interrupt/failure wrap-up so interrupted turns still count; the rollout
    /// record doubles as the "counted" marker for the crash-recovery scan in
    /// Session::load. The in-flight request's usage is unrecoverable either
    /// way (usage arrives at the stream's end). Returns None when no request
    /// of the turn reported usage.
    fn flush_turn_stats(
        &mut self,
        config: &ResolvedModel,
        duration_ms: u64,
    ) -> Option<pig_protocol::TurnUsageStats> {
        if self.turn_input + self.turn_cache_read + self.turn_output == 0 {
            return None;
        }
        self.store.lock().expect("store lock").record_usage(
            &self.id,
            &config.provider_name,
            &config.model,
            self.turn_input,
            self.turn_cache_read,
            self.turn_output,
            self.turn_reasoning_output,
        );
        // Persist turn stats: replay restores the footer and session totals
        // (the usage watermark is restored by StepUsage)
        self.record(&RolloutRecord::TurnStats {
            input: self.turn_input,
            cache_read: self.turn_cache_read,
            output: self.turn_output,
            duration_ms,
            api_ms: self.turn_api_ms,
            ttft_ms: self.turn_ttft_ms,
            api_steps: self.turn_api_steps,
        });
        Some(pig_protocol::TurnUsageStats {
            input: self.turn_input,
            cache_read: self.turn_cache_read,
            output: self.turn_output,
            duration_ms,
            api_ms: self.turn_api_ms,
            ttft_ms: self.turn_ttft_ms,
            api_steps: self.turn_api_steps,
        })
    }

    fn touch_index(&mut self) {
        let id = self.id.clone();
        self.store
            .lock()
            .expect("store lock")
            .update_session(&id, |meta| meta.updated_at = now_secs());
    }

    pub fn set_mode(&mut self, mode: ExecMode) {
        self.mode = mode;
    }

    pub fn set_plan_mode(&mut self, enabled: bool) {
        self.plan_enabled = enabled;
    }

    /// Session-level "read/write outside the workspace" toggles (written
    /// into the shared state, effective even mid-turn)
    pub fn set_fs_access(&mut self, read_outside: bool, write_outside: bool) {
        use std::sync::atomic::Ordering;
        self.state
            .fs_read_outside
            .store(read_outside, Ordering::Relaxed);
        self.state
            .fs_write_outside
            .store(write_outside, Ordering::Relaxed);
    }

    pub fn set_model(&mut self, selection: ModelSelection) {
        self.model_override = Some(selection);
    }

    pub fn revert_file(&mut self, path: &str, tx: &async_channel::Sender<Event>) {
        // Path resolution errors (original English) and revert errors
        // uniformly go through the CoreError channel
        let result = tool::resolve_checked(&self.cwd, path, false)
            .map_err(|e| CoreError::Internal { detail: e })
            .and_then(|full| self.tracker.revert(&full).map(|()| full));
        match result {
            Ok(full) => {
                // After a revert neither the change nor the baseline matters
                // anymore: clear the store (change row + original snapshot
                // row)
                {
                    let store = self.store.lock().expect("store lock");
                    store.delete_file_change(&self.id, path);
                    store.delete_file_original(&self.id, &full.to_string_lossy());
                }
                self.emit(
                    |session_id, seq| Event::FileReverted {
                        session_id,
                        seq,
                        path: path.to_string(),
                    },
                    tx,
                );
            }
            Err(error) => self.emit(
                |session_id, seq| Event::Error {
                    session_id: Some(session_id),
                    seq,
                    error,
                },
                tx,
            ),
        }
    }
}

mod approval;
mod compact;
mod fork;
mod images;
mod replay;
mod runner;
mod subagent;
mod title;
mod turn;

// Large impls are split into submodules (impl blocks may be scattered within
// a crate; submodules can see the root's private items), the public API is
// pinned by explicit re-exports, and cross-submodule references are forwarded
// via the root.
pub use approval::approval_detail;
pub use compact::COMPACTION_MARKER;
pub(crate) use images::compression_note;
pub use images::project_images;
pub use runner::agent_loop;
pub use title::TITLE_PROMPT_MARKER;
pub(crate) use title::spawn_title_generation;
pub(crate) use turn::pointer_file_references;

/// Sum the tail turn's per-request usage when it was never counted: records
/// after the last User record hold StepUsage but no TurnStats (a turn that
/// ends normally — or is interrupted, which writes TurnStats at cancel time —
/// always closes with TurnStats). Returns (input, cache_read, output); None
/// when the tail turn was counted or consumed nothing.
fn uncounted_tail_usage(records: &[RolloutRecord]) -> Option<(u64, u64, u64)> {
    let tail = records
        .iter()
        .rev()
        .take_while(|r| !matches!(r, RolloutRecord::User { .. }));
    let (mut input, mut cache_read, mut output, mut counted) = (0u64, 0u64, 0u64, false);
    for record in tail {
        match record {
            RolloutRecord::StepUsage {
                input: i,
                cache_read: c,
                output: o,
                ..
            } => {
                input += i;
                cache_read += c;
                output += o;
            }
            RolloutRecord::TurnStats { .. } => counted = true,
            _ => {}
        }
    }
    (!counted && input + cache_read + output > 0).then_some((input, cache_read, output))
}

/// Load project-level permission rules: a missing file = empty rules; parse
/// failures are non-fatal with a warn-level log (the session creation/replay
/// paths have no suitable event channel, so none is forced)
fn load_permissions(cwd: &std::path::Path) -> crate::permissions::PermissionRules {
    match crate::permissions::PermissionRules::load(cwd) {
        Ok(rules) => {
            if rules.skipped > 0 {
                tracing::warn!(
                    "skipped {} rules with syntax errors (.pigcode/permissions.toml)",
                    rules.skipped
                );
            }
            rules
        }
        Err(error) => {
            tracing::warn!("failed to load (continuing with no rules): {error:?}");
            crate::permissions::PermissionRules::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> RolloutRecord {
        RolloutRecord::User {
            text: "hi".to_string(),
            files: vec![],
            images: vec![],
        }
    }

    fn step(input: u64, cache_read: u64, output: u64) -> RolloutRecord {
        RolloutRecord::StepUsage {
            input,
            cache_read,
            output,
            used: 0,
        }
    }

    fn stats() -> RolloutRecord {
        RolloutRecord::TurnStats {
            input: 1,
            cache_read: 1,
            output: 1,
            duration_ms: 0,
            api_ms: 0,
            ttft_ms: 0,
            api_steps: 1,
        }
    }

    #[test]
    fn crashed_tail_turn_sums_step_usage() {
        // Completed turn (counted) + crashed tail turn → only the tail sums
        let records = vec![
            user(),
            step(5, 5, 5),
            stats(),
            user(),
            step(1, 2, 3),
            step(4, 5, 6),
        ];
        assert_eq!(uncounted_tail_usage(&records), Some((5, 7, 9)));
    }

    #[test]
    fn counted_tail_turn_is_skipped() {
        // Normal end and live interrupt both close with TurnStats
        let records = vec![user(), step(1, 2, 3), stats()];
        assert_eq!(uncounted_tail_usage(&records), None);
    }

    #[test]
    fn usage_free_tail_is_skipped() {
        // Interrupted before any usage arrived; empty (never messaged) session
        assert_eq!(uncounted_tail_usage(&[user()]), None);
        assert_eq!(uncounted_tail_usage(&[]), None);
    }
}
