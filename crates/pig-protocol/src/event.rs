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

/// core → UI events
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    SessionConfigured {
        session_id: String,
        cwd: PathBuf,
        model: String,
        provider_name: String,
        /// Session's current model/reasoning level/exec mode (values restored on
        /// reopen, inherited by new sessions)
        provider_id: Option<String>,
        model_id: Option<String>,
        reasoning_level: Option<String>,
        exec_mode: ExecMode,
        /// Plan mode switch (restored on reopen, inherited by new sessions;
        /// orthogonal to exec_mode)
        #[serde(default)]
        plan_enabled: bool,
        /// Session-level "read/write outside workspace" switches (to restore the
        /// UI mode menu's checkbox states)
        #[serde(default)]
        fs_read_outside: bool,
        #[serde(default)]
        fs_write_outside: bool,
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
    /// Messages arriving mid-turn are queued; auto-continued after the turn ends
    MessageQueued {
        session_id: String,
        seq: u64,
        text: String,
    },
    /// Settled user messages from resume replay and new sends (live goes through
    /// this event too; the UI appends uniformly)
    UserMessage {
        session_id: String,
        seq: u64,
        /// Clean body text (attachment links no longer embed text; the UI renders
        /// thumbnails via image_nums)
        text: String,
        files: Vec<String>,
        /// Number of attached images (kept for protocol compatibility; display
        /// uses image_nums)
        #[serde(default)]
        image_count: usize,
        /// Media file numbers of the attached images (the N in
        /// {sessions}/{id}.media/{N}.ext); the UI loads thumbnails from these,
        /// consistent with image order
        #[serde(default)]
        image_nums: Vec<u32>,
    },
    TurnStarted {
        session_id: String,
        seq: u64,
        turn_id: String,
    },
    /// Reasoning deltas (GLM/DeepSeek's reasoning_content), live-only
    ReasoningDelta {
        session_id: String,
        seq: u64,
        item_id: String,
        delta: String,
    },
    TextDelta {
        session_id: String,
        seq: u64,
        item_id: String,
        delta: String,
    },
    /// Final text value, a replayable boundary
    TextDone {
        session_id: String,
        seq: u64,
        item_id: String,
        full_text: String,
    },
    ToolCallBegin {
        session_id: String,
        seq: u64,
        item_id: String,
        tool: String,
        input_summary: String,
        /// Full arguments JSON (for approval card display)
        detail: String,
    },
    ToolCallEnd {
        session_id: String,
        seq: u64,
        item_id: String,
        output: String,
        is_error: bool,
        /// This invocation's edit diff for write/modify tools (for inline card
        /// rendering; cumulative session diffs go through FileChanged)
        #[serde(default)]
        edit: Option<EditDiff>,
    },
    /// Subagent live progress (live-only, not persisted to rollout):
    /// item_id = the parent session's Agent tool card
    SubagentProgress {
        session_id: String,
        seq: u64,
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
        seq: u64,
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
        seq: u64,
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
    /// Compact started (emitted both for manual /compact and the automatic
    /// pre-sampling watermark trigger; the summary is a one-shot blocking request
    /// during which the turn has no other events — the UI relies on this to show
    /// "compacting context")
    CompactStarted {
        session_id: String,
        seq: u64,
        /// true = triggered automatically pre-sampling; false = the user's manual /compact
        automatic: bool,
    },
    ContextCompacted {
        session_id: String,
        seq: u64,
        omitted: usize,
        note: String,
        /// true = triggered automatically pre-sampling; false = the user's manual /compact
        automatic: bool,
    },
    /// core blocks waiting for Op::ApprovalReply (same request_id)
    ApprovalRequested {
        session_id: String,
        seq: u64,
        request_id: String,
        tool: String,
        /// Bash: the full command; Write/Edit: path + diff preview
        detail: String,
        /// Reason key for high-risk commands (bash.rs DangerReason.key; the UI
        /// localizes the warning line by key)
        #[serde(default)]
        danger_key: Option<String>,
    },
    /// core switched the exec mode on its own (after ExitPlanMode approval):
    /// the UI syncs the mode chip
    ExecModeChanged {
        session_id: String,
        seq: u64,
        mode: ExecMode,
    },
    /// Plan mode switch changed (emitted when the model toggles it via
    /// EnterPlanMode/ExitPlanMode; UI-initiated toggles go through Op::SetPlanMode
    /// with no event back)
    PlanModeChanged {
        session_id: String,
        seq: u64,
        enabled: bool,
    },
    /// AskUserQuestion: core blocks waiting for Op::QuestionReply (same request_id)
    QuestionRequested {
        session_id: String,
        seq: u64,
        request_id: String,
        questions: Vec<QuestionItem>,
    },
    FileChanged {
        session_id: String,
        seq: u64,
        path: String,
        unified_diff: String,
        additions: u32,
        deletions: u32,
    },
    FileReverted {
        session_id: String,
        seq: u64,
        path: String,
    },
    /// Todo snapshot after the TodoList tool writes successfully (including the
    /// initial empty snapshot on session create/switch)
    TodoListChanged {
        session_id: String,
        seq: u64,
        items: Vec<TodoItem>,
    },
    /// Panel snapshot after a background task's status changes (start/exit/stop)
    TaskListChanged {
        session_id: String,
        seq: u64,
        tasks: Vec<TaskSummary>,
    },
    TurnComplete {
        session_id: String,
        seq: u64,
        duration_ms: u64,
        /// Turn token stats (None for aborted turns or turns without usage data)
        #[serde(default)]
        stats: Option<TurnUsageStats>,
    },
    TurnAborted {
        session_id: String,
        seq: u64,
    },
    /// End of a turn: the agent's file changes this turn (per file, the net diff
    /// from just before its first write this turn to now, matching ZCode's turn
    /// header panel)
    TurnFileChanges {
        session_id: String,
        seq: u64,
        files: Vec<EditDiff>,
    },
    /// Reply to Op::LoadSubagent: a read-only projection of the subagent's full
    /// conversation (the right-side "subagent" tab)
    SubagentHistory {
        session_id: String,
        seq: u64,
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
        seq: u64,
        agent_id: String,
        item: Option<SubagentItem>,
        finished: bool,
    },
    Error {
        session_id: Option<String>,
        seq: u64,
        /// Structured error (core is language-agnostic; the UI localizes by kind,
        /// detail is the English original)
        error: CoreError,
    },
}
