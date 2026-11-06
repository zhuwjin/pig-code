use super::*;

/// Connection status snapshot of a single MCP server (for settings page display):
/// success carries the tool count; failure has connected=false and error as the
/// structured reason
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub connected: bool,
    pub tool_count: usize,
    pub error: Option<CoreError>,
}

/// Heuristic composition of the conversation surface — what the next request
/// would send: the system prompt, the tool schemas, and every other message.
/// All three are fixed-density estimates (4 characters per token), not
/// provider counts, so they explain the shape of the context and never sum to
/// the provider-anchored usage figure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBreakdown {
    /// The surviving system prompt (always the frozen session prompt).
    #[serde(default)]
    pub system: u64,
    /// Tool schemas of the newest request.
    #[serde(default)]
    pub tools: u64,
    /// Everything else on the surface: user/assistant/tool messages,
    /// compaction summaries and superseded prompts.
    #[serde(default)]
    pub messages: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    /// Anthropic `pause_turn`: the assistant turn is not finished; the same
    /// turn continues with the persisted assistant content (hosted server
    /// tools may resume). This is a successful step boundary, not an
    /// incomplete/failed response.
    Pause,
}

impl FinishReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::ToolCalls => "tool_calls",
            Self::Length => "length",
            Self::ContentFilter => "content_filter",
            Self::Pause => "pause",
        }
    }

    pub fn is_incomplete(self) -> bool {
        matches!(self, Self::Length | Self::ContentFilter)
    }
}

#[cfg(test)]
mod tests {
    use super::FinishReason;

    #[test]
    fn error_fatality_and_deferred_plan_default_for_legacy_payloads() {
        let error: super::Event = serde_json::from_value(serde_json::json!({
            "Error": {
                "session_id": null,
                "error": {"kind": "internal", "detail": "legacy"},
            }
        }))
        .expect("legacy error event");
        assert!(
            matches!(error, super::Event::Error { fatal: true, .. }),
            "an unset fatality keeps the old turn-ending interpretation"
        );

        let plan: super::Event = serde_json::from_value(serde_json::json!({
            "PlanModeChanged": {
                "session_id": "s",
                "enabled": true,
            }
        }))
        .expect("legacy plan event");
        assert!(
            matches!(
                plan,
                super::Event::PlanModeChanged {
                    deferred: false,
                    ..
                }
            ),
            "a legacy plan event is an immediate application"
        );
    }

    #[test]
    fn events_carry_no_sequence_cursor() {
        let event = super::Event::TurnStarted {
            session_id: "s".into(),
            turn_id: "t".into(),
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert!(
            json["TurnStarted"].get("seq").is_none(),
            "Event ordering is channel order; there is no replay cursor: {json}"
        );
    }

    #[test]
    fn pause_is_a_typed_success_boundary_not_incomplete() {
        assert_eq!(FinishReason::Pause.as_str(), "pause");
        assert!(!FinishReason::Pause.is_incomplete());
        assert_eq!(
            serde_json::from_str::<FinishReason>("\"pause\"").unwrap(),
            FinishReason::Pause
        );
        // Existing record/wire values keep their meaning.
        assert_eq!(FinishReason::Stop.as_str(), "stop");
        assert_eq!(FinishReason::ToolCalls.as_str(), "tool_calls");
        assert_eq!(FinishReason::Length.as_str(), "length");
        assert_eq!(FinishReason::ContentFilter.as_str(), "content_filter");
    }
}

/// One normalized hosted-search source shown under the assistant text that
/// cited it. Pure display data: provider-native encrypted fields never enter
/// this type. `url` is always `http`/`https`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    /// Display title; empty = the UI falls back to the URL
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Citation span inside the assistant text, when the provider reports one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_index: Option<u32>,
}

/// core → UI events
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum Event {
    SessionConfigured {
        session_id: String,
        cwd: PathBuf,
        model: String,
        provider_name: String,
        /// Session's current model/reasoning level/permissions (values restored
        /// on reopen, inherited by new sessions)
        provider_id: Option<String>,
        model_id: Option<String>,
        reasoning_level: Option<String>,
        permissions: PermissionSettings,
        /// Plan mode switch (restored on reopen, inherited by new sessions;
        /// orthogonal to permissions)
        #[serde(default)]
        plan_enabled: bool,
    },
    SessionList {
        sessions: Vec<SessionMeta>,
    },
    /// Session title changed (list sync after the auto-naming sidecar finishes /
    /// a manual rename)
    SessionTitleChanged {
        session_id: String,
        title: String,
    },
    WorkspaceList {
        workspaces: Vec<WorkspaceMeta>,
    },
    ConfigSnapshot {
        config: AppConfig,
    },
    ProviderCredentials {
        credentials: std::collections::BTreeMap<String, ProviderCredentialStatus>,
    },
    TestResult {
        provider_id: String,
        result: ConnTestResult,
    },
    /// models.dev model metadata lookup result; info=None = the ID is not in the source
    ModelInfo {
        id: String,
        info: Option<ModelRegistryInfo>,
    },
    /// Reply to Op::ListMcpServers: the session's MCP server status list
    /// (None = session missing or lazy connection not started yet — MCP connects
    /// only before the first turn's sampling)
    McpServerList {
        session_id: String,
        servers: Option<Vec<McpServerStatus>>,
    },
    FileSearchResults {
        session_id: String,
        query: String,
        results: Vec<String>,
    },
    GitInfo {
        cwd: PathBuf,
        current_branch: Option<String>,
        branches: Vec<String>,
    },
    BranchChanged {
        cwd: PathBuf,
        branch: String,
    },
    GitStatus {
        cwd: PathBuf,
        /// false = not a git repo (both lists empty)
        is_git: bool,
        unstaged: Vec<GitFileChange>,
        staged: Vec<GitFileChange>,
    },
    GitDiff {
        cwd: PathBuf,
        path: String,
        staged: bool,
        /// Pure diff text (placeholder notes don't go here, see note)
        diff: String,
        /// Structured note for an unavailable/truncated diff (UI localizes the
        /// placeholder text by kind)
        #[serde(default)]
        note: Option<GitDiffNote>,
    },
    /// Live-only authoritative snapshot of the future-turn queue (messages
    /// admitted while a turn was running). Emitted on every mutation and on
    /// session open; the list is persisted in the session's store record, so a
    /// restart re-emits it instead of losing accepted messages
    QueueChanged {
        session_id: String,
        items: Vec<QueueItem>,
    },
    /// Live-only: the queue's auto-drain flipped — paused after the turn
    /// ended with an error or a user stop while the queue was non-empty, or
    /// resumed by Op::ResumeQueue (the head then starts as the next turn)
    QueueDrainChanged {
        session_id: String,
        auto_drain: bool,
        /// Pause reason, present only when auto_drain turned false
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<QueuePauseReason>,
    },
    /// Live-only: a send arrived while the queue is paused and non-empty —
    /// core holds the message; Op::ResolveHeldQueue decides its fate
    HeldQueuePrompt {
        session_id: String,
        /// The held message's text (prompt display)
        text: String,
        /// The queue lane the disposition applies to
        items: Vec<QueueItem>,
    },
    /// Live-only: the held message was resolved (started / appended /
    /// dropped); the UI closes the confirmation prompt
    HeldQueueResolved {
        session_id: String,
    },
    /// Live-only authoritative snapshot of the steer lane (messages accepted
    /// for injection into the running turn; they become real user messages at
    /// the next safe boundary, or fall back to the queue front if the turn
    /// ends first)
    SteerChanged {
        session_id: String,
        items: Vec<QueueItem>,
    },
    /// Settled user messages from resume replay and new sends (live goes through
    /// this event too; the UI appends uniformly)
    UserMessage {
        session_id: String,
        /// Clean body text (attachment links no longer embed text; the UI renders
        /// thumbnails via image_nums)
        text: String,
        files: Vec<String>,
        /// Media file numbers of the attached images (the N in
        /// {sessions}/{id}.media/{N}.ext); the UI loads thumbnails from these,
        /// consistent with image order
        #[serde(default)]
        image_nums: Vec<u32>,
        /// When the message was sent (unix ms; 0 = unknown, e.g. a rollout
        /// written before this field existed). The user bubble's action row
        /// prints it; replay restores it from the rollout's User record.
        #[serde(default)]
        sent_at_ms: u64,
    },
    /// The last user message (and the whole turn it triggered) was recalled:
    /// the UI truncates its message list from the last user-role row to the end.
    /// Emitted both live (the recall confirmation) and on resume replay (one per
    /// Recall record, in order)
    MessageRecalled {
        session_id: String,
    },
    TurnStarted {
        session_id: String,
        turn_id: String,
    },
    /// Reasoning deltas (GLM/DeepSeek's reasoning_content), live-only
    ReasoningDelta {
        session_id: String,
        item_id: String,
        delta: String,
    },
    TextDelta {
        session_id: String,
        item_id: String,
        delta: String,
    },
    /// Final text value, a replayable boundary
    TextDone {
        session_id: String,
        item_id: String,
        full_text: String,
    },
    /// Normalized hosted-search sources derived from the assistant step's
    /// durable native replay. `item_id` is the exact text item the sources
    /// belong to; the UI attaches them to that Markdown segment. Live and
    /// replay emit the same value from the same persisted replay.
    Sources {
        session_id: String,
        item_id: String,
        sources: Vec<Source>,
    },
    ToolCallBegin {
        session_id: String,
        item_id: String,
        tool: String,
        input_summary: String,
        /// Full arguments JSON (for approval card display)
        detail: String,
    },
    ToolCallEnd {
        session_id: String,
        item_id: String,
        output: String,
        is_error: bool,
        #[serde(default)]
        stopped: bool,
        /// This invocation's edit diff for write/modify tools (for inline card
        /// rendering; cumulative session diffs go through FileChanged)
        #[serde(default)]
        edit: Option<EditDiff>,
    },
    /// Live-only (never persisted, replay never emits it): the model started
    /// streaming a tool call's arguments — a pending tool card shows the
    /// arguments growing before execution; the authoritative begin still
    /// arrives via ToolCallBegin with the same item_id
    ToolInputStart {
        session_id: String,
        item_id: String,
        tool: String,
    },
    /// Live-only: one incremental raw-JSON argument fragment for a pending
    /// tool call (newline-batched in the provider, one event per content line)
    ToolInputDelta {
        session_id: String,
        item_id: String,
        delta: String,
    },
    /// Live-only: a pending tool call never made the final list (filtered as
    /// broken model output); the UI drops its card
    ToolInputDiscard {
        session_id: String,
        item_id: String,
    },
    /// Subagent live progress (live-only, not persisted to rollout):
    /// item_id = the parent session's Agent tool card
    SubagentProgress {
        session_id: String,
        item_id: String,
        note: String,
    },
    /// The "agent card" metadata of a subagent tool card (emitted live plus
    /// persisted to rollout, rebuilt from the record on replay):
    /// item_id = the parent session's Agent tool card; without this metadata (the
    /// transient state before this event arrives live), the UI renders the
    /// standard tool card style
    SubagentCard {
        session_id: String,
        item_id: String,
        agent_id: String,
        profile: String,
        description: String,
        /// "{provider_name} · {model}" (appends " · {level}" when the profile
        /// carries thought_level)
        model: String,
        /// Whether this run is in the background (run_in_background): a background
        /// card's running state is driven by the subagent's real lifecycle
        /// (SubagentActivity), not the tool call's done
        background: bool,
    },
    ContextUsage {
        session_id: String,
        used: u64,
        total: u64,
        /// Session totals (including resume-restored values): cache-hit input
        /// tokens and cache-miss input tokens; average cache hit rate =
        /// cache_read_total / (cache_read_total + input_total)
        #[serde(default)]
        cache_read_total: u64,
        #[serde(default)]
        input_total: u64,
    },
    /// Heuristic composition of the context, re-emitted whenever the surface
    /// changes (before each request, after a reply is appended, after a
    /// compaction, on session reopen). Kept separate from ContextUsage: a
    /// finished reply changes the composition without producing a new provider
    /// sample.
    ContextBreakdownChanged {
        session_id: String,
        breakdown: ContextBreakdown,
    },
    /// Compact started (emitted both for manual /compact and the automatic
    /// pre-sampling watermark trigger; the summary is a one-shot blocking request
    /// during which the turn has no other events — the UI relies on this to show
    /// "compacting context")
    CompactStarted {
        session_id: String,
        /// true = triggered automatically pre-sampling; false = the user's manual /compact
        automatic: bool,
    },
    ContextCompacted {
        session_id: String,
        omitted: usize,
        note: String,
        /// true = triggered automatically pre-sampling; false = the user's manual /compact
        automatic: bool,
        /// Pre-compaction context usage watermark (None = no Usage sample yet); shown in the
        /// divider as "before → after tokens"
        #[serde(default)]
        used_before: Option<u64>,
        /// Post-compaction estimated watermark (the same value as the refreshed ContextUsage)
        #[serde(default)]
        used_after: Option<u64>,
        /// Bare model summary (None = summary generation failed, truncation fallback); the
        /// note embeds it in model-facing guidance — the UI's "view summary" panel shows this
        #[serde(default)]
        summary: Option<String>,
    },
    /// core blocks waiting for Op::ApprovalReply (same request_id)
    ApprovalRequested {
        session_id: String,
        request_id: String,
        tool: String,
        /// Bash: the full command; Write/Edit: path + diff preview
        detail: String,
        /// Reason key for high-risk commands (bash.rs DangerReason.key; the UI
        /// localizes the warning line by key)
        #[serde(default)]
        danger_key: Option<String>,
        /// Directories an approved sandbox widening would make writable, so a
        /// client can show them as data instead of parsing the detail text.
        /// Empty unless the request is a `with_additional_permissions` ask.
        #[serde(default)]
        requested_write_roots: Vec<String>,
        /// Decisions this request accepts, in display order. Core computes it
        /// because only core knows whether "remember for the session" is
        /// meaningful here (never for high-risk commands); a client must not
        /// offer an option the gate would ignore.
        #[serde(default)]
        available_decisions: Vec<crate::ApprovalDecision>,
    },
    /// core changed the session's permissions on its own (after ExitPlanMode
    /// approval): the UI syncs the permission chip
    PermissionsChanged {
        session_id: String,
        permissions: PermissionSettings,
    },
    /// Plan mode switch changed (emitted when the model toggles it via
    /// EnterPlanMode/ExitPlanMode, and when a UI-initiated toggle cannot reach
    /// the live session because a turn/compaction owns it). A deferred toggle
    /// is already durable in the session metadata and is applied to the
    /// session at the next turn start; the client keeps its toggle in sync
    /// either way.
    PlanModeChanged {
        session_id: String,
        enabled: bool,
        /// true = the switch is persisted and will be applied at the next turn
        /// start (the running turn keeps its current mode); false = applied now
        #[serde(default)]
        deferred: bool,
    },
    /// AskUserQuestion: core blocks waiting for Op::QuestionReply (same request_id)
    QuestionRequested {
        session_id: String,
        request_id: String,
        questions: Vec<QuestionItem>,
    },
    FileChanged {
        session_id: String,
        path: String,
        unified_diff: String,
        additions: u32,
        deletions: u32,
    },
    FileReverted {
        session_id: String,
        path: String,
    },
    /// Todo snapshot after the TodoList tool writes successfully (including the
    /// initial empty snapshot on session create/switch)
    TodoListChanged {
        session_id: String,
        items: Vec<TodoItem>,
    },
    /// Panel snapshot after a background task's status changes (start/exit/stop)
    TaskListChanged {
        session_id: String,
        tasks: Vec<TaskSummary>,
    },
    TaskNotificationBatch {
        session_id: String,
        notifications: Vec<TaskNotification>,
        model_text: String,
        sent_at_ms: u64,
    },
    TaskNotificationQueueChanged {
        session_id: String,
        notifications: Vec<TaskNotification>,
    },
    TurnComplete {
        session_id: String,
        duration_ms: u64,
        /// Turn token stats (None for aborted turns or turns without usage data)
        #[serde(default)]
        stats: Option<TurnUsageStats>,
        /// Present when the final model response ended through length or content filtering.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
    },
    TurnAborted {
        session_id: String,
    },
    /// End of a turn: the agent's file changes this turn (per file, the net diff
    /// from just before its first write this turn to now, matching ZCode's turn
    /// header panel)
    TurnFileChanges {
        session_id: String,
        files: Vec<EditDiff>,
    },
    /// Reply to Op::LoadSubagent: a read-only projection of the subagent's full
    /// conversation (the right-side "subagent" tab)
    SubagentHistory {
        session_id: String,
        agent_id: String,
        /// meta.description (for the tab title/card title)
        title: String,
        /// "{provider} · {model}"
        subtitle: String,
        items: Vec<SubagentItem>,
        /// The subagent was still running at load time (per the task registry):
        /// the panel shows a "running" indicator
        #[serde(default)]
        running: bool,
    },
    /// Subagent live display items (live-only): emitted one by one as the subagent
    /// finishes each batch of messages; item=None + finished=true means the
    /// subagent ended (including cancel/kill), and the panel turns off "running"
    SubagentActivity {
        session_id: String,
        agent_id: String,
        item: Option<SubagentItem>,
        finished: bool,
    },
    /// Live-only: the session's model call entered a retry wait (transient,
    /// never persisted; superseded by the next delta or the turn's end)
    RetryStatus {
        session_id: String,
        status: RetryStatus,
    },
    /// Live-only (never persisted, replay never emits it): a hook chain for the
    /// named event started or settled — a transient indicator near the composer
    /// (same lifecycle as RetryStatus: any later event supersedes Running)
    HookStatus {
        session_id: String,
        /// Hook event name in Claude form ("PreToolUse", "UserPromptSubmit", ...)
        hook_event: String,
        /// Display label: the first running hook's status_message or a default
        label: String,
        state: HookRunState,
    },
    /// Live-only per-hook completion record (never persisted, replay never
    /// emits it): one row of the thread-view hook summary popover of the turn.
    /// Client-safe: no command, paths, or captured output
    HookRunFinished {
        session_id: String,
        /// Hook event name in Claude form ("PreToolUse", "UserPromptSubmit", ...)
        hook_event: String,
        /// Display label: the entry's status_message or the event name
        label: String,
        /// Where the entry was declared (None = provenance unknown)
        source: Option<HookSource>,
        duration_ms: u64,
        /// Terminal state of this entry (Blocked on Stop = asked to continue)
        state: HookRunState,
        /// Sanitized block reason (state == Blocked on blocking events)
        reason: Option<String>,
    },
    /// Project hooks (.pigcode/hooks.toml) exist but the workspace is not
    /// trusted yet: they stay skipped (fail-closed) until
    /// Op::TrustProjectHooks; emitted once per session when hooks are enabled
    ProjectHooksPending {
        session_id: String,
        /// Workspace root path (banner display + trust key)
        path: String,
        /// Skipped project hook entries (across all events)
        count: usize,
    },
    /// Op::TrustProjectHooks accepted: the pending banner closes and project
    /// hooks run from the next event on (live-only)
    ProjectHooksTrusted {
        session_id: String,
    },
    PluginSnapshot {
        snapshot: PluginSnapshot,
    },
    PluginRuntimeContributionCommitted {
        request_id: String,
        epoch: u64,
        snapshot: PluginSnapshot,
    },
    PluginMarketplaceCatalog {
        result: Result<PluginMarketplaceCatalog, CoreError>,
    },
    PluginMarketplaceInstall {
        result: Result<PluginMarketplaceInstall, CoreError>,
    },
    PluginServices {
        request_id: String,
        package: String,
        digest: String,
        result: Result<PluginServiceSnapshot, CoreError>,
    },
    PluginActionResult {
        result: PluginActionResult,
    },
    PluginToolCall {
        call: PluginToolCall,
    },
    /// The session dropped a pending plugin call (turn cancellation or a
    /// dispatcher timeout): the host asks the backend to stop working on it.
    PluginToolCancel {
        session_id: String,
        request_id: String,
    },
    PluginToolsQuery {
        query: PluginToolsQuery,
    },
    PluginHttpResponse {
        request_id: String,
        result: Result<PluginHttpResponse, CoreError>,
    },
    PluginSecretResult {
        request_id: String,
        result: Result<(), CoreError>,
    },
    Error {
        session_id: Option<String>,
        /// Structured error (core is language-agnostic; the UI localizes by kind,
        /// detail is the English original)
        error: CoreError,
        /// true = the error ended the current turn (provider/stream/compaction
        /// failure); false = an operation request was rejected or a background
        /// action failed while the session and any running turn continue. The
        /// client must not tear down turn state for a nonfatal error.
        #[serde(default = "default_true")]
        fatal: bool,
    },
}

/// Why a model call is retrying (mirror of pig-provider's RetryReason; the UI
/// localizes at render time, the string fields stay English detail)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetryReason {
    /// HTTP 429; retry_after is the honored Retry-After when present (seconds)
    RateLimit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_secs: Option<u64>,
    },
    /// HTTP 5xx
    Server(u16),
    /// Send-phase network failure (DNS/TLS/connect/...); detail is the folded root cause
    Network { detail: String },
    /// A 200 stream ended with no text/reasoning/tool calls (one silent retry)
    EmptyCompletion,
    /// Retryable in-band 200 error; detail is the surfaced text
    InBand { detail: String },
}

/// One retry's visible state (mirror of pig-provider's RetryNotice): which
/// retry of how many, how long the wait is (milliseconds), and why
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryStatus {
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    pub reason: RetryReason,
}

/// Terminal state of one hook chain run (Event::HookStatus.state)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookRunState {
    Running,
    Finished,
    Failed,
    Blocked,
}

/// Legacy `Error` events (and hand-written test fixtures) predate the
/// fatality flag; an unset value keeps the old turn-ending interpretation.
fn default_true() -> bool {
    true
}
