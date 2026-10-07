use super::*;

/// UI → core commands
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Op {
    NewSession {
        cwd: PathBuf,
        /// Current model selection in the UI; None → the workspace's most recently
        /// active session / config default
        provider_id: Option<String>,
        model_id: Option<String>,
        /// Current reasoning level in the UI (adopted as-is, None = off; the seeded
        /// level arrives via the UI hero defaults)
        reasoning_level: Option<String>,
        /// Current exec mode in the UI; None → the workspace's most recently active
        /// session / default
        exec_mode: Option<ExecMode>,
        /// Plan mode switch (None/false = off; not seeded — plan is a transient state)
        #[serde(default)]
        plan_enabled: Option<bool>,
    },
    OpenSession {
        session_id: String,
    },
    ListSessions,
    ListWorkspaces,
    AddWorkspace {
        path: PathBuf,
    },
    /// Remove the workspace from the sidebar: mark it hidden, keeping the entry and
    /// session data; automatically restored when a new session is created in that workspace
    RemoveWorkspace {
        path: PathBuf,
    },
    /// Rename the workspace display name; alias None restores the default directory name
    RenameWorkspace {
        path: PathBuf,
        alias: Option<String>,
    },
    UpdateSessionMeta {
        session_id: String,
        pinned: Option<bool>,
        archived: Option<bool>,
        /// Manual rename (sets title_custom; auto-naming no longer overwrites afterwards)
        title: Option<String>,
    },
    /// Delete a session: clears sessions plus related tables + the rollout JSONL,
    /// unrecoverable
    DeleteSession {
        session_id: String,
    },
    /// Fork a session: derive a new session from the first N turns of the source's
    /// history (the boundary includes the Nth turn itself); after creation it opens
    /// via the OpenSession cold path (SessionConfigured + replay)
    ForkSession {
        session_id: String,
        /// Number of turns kept (≥1, 0 clamps to 1; exceeding the source's total
        /// turns = full copy)
        turns: usize,
    },
    SendMessage {
        session_id: String,
        content: String,
        files: Vec<String>,
        /// Pasted/dragged-in images (raw bytes; core compresses them before they
        /// enter the model context)
        #[serde(default)]
        images: Vec<PendingImage>,
        mode: ExecMode,
    },
    Interrupt {
        session_id: String,
    },
    ApprovalReply {
        request_id: String,
        decision: ApprovalDecision,
        /// Feedback (kimi Revise: carried to the model for revision when a plan is
        /// rejected; ExitPlanMode only)
        #[serde(default)]
        feedback: Option<String>,
    },
    /// Reply to a structured question: None = the user skipped; the outer vector is
    /// per question, the inner holds that question's selected labels ("Other" free
    /// text goes in as a label verbatim)
    QuestionReply {
        request_id: String,
        answers: Option<Vec<Vec<String>>>,
    },
    SetModel {
        session_id: String,
        provider_id: String,
        model_id: String,
        /// None = reasoning params disabled
        reasoning_level: Option<String>,
    },
    /// Set the reasoning level alone: also effective when no model override exists
    /// (applies to the configured default model) and persisted
    SetReasoning {
        session_id: String,
        reasoning_level: Option<String>,
    },
    GetConfig,
    SaveConfig {
        config: AppConfig,
    },
    TestProvider {
        provider_id: String,
    },
    SetExecMode {
        session_id: String,
        mode: ExecMode,
    },
    /// Plan mode switch (orthogonal to exec mode; the model toggles it on its side
    /// via EnterPlanMode/ExitPlanMode)
    SetPlanMode {
        session_id: String,
        enabled: bool,
    },
    /// Session-level "read/write outside workspace" switches (off by default; tmp
    /// directories are always allowed)
    SetFsAccess {
        session_id: String,
        read_outside: bool,
        write_outside: bool,
    },
    RevertFile {
        session_id: String,
        path: String,
    },
    SearchFiles {
        session_id: String,
        query: String,
        /// Explicit search directory (the hero workspace when no session is created
        /// yet; None = the session's/default directory)
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Sessionless: query a directory's git info (the hero branch selector)
    GitInfo {
        cwd: PathBuf,
    },
    CheckoutBranch {
        cwd: PathBuf,
        branch: String,
    },
    /// Sessionless: the workspace's git change list (unstaged + staged, with
    /// untracked line counts)
    GitStatus {
        cwd: PathBuf,
    },
    /// Sessionless: one file's raw git diff (staged=false unstaged / true staged)
    GitDiff {
        cwd: PathBuf,
        path: String,
        staged: bool,
    },
    /// Cancel the session's first queued message (FIFO)
    CancelQueued {
        session_id: String,
        text: String,
    },
    /// Load a background subagent's full conversation (read-only display in the
    /// right-side "subagent" tab); needs no Session instance, directly reads
    /// {sessions_dir}/{session_id}.agents/{agent_id}.jsonl
    LoadSubagent {
        session_id: String,
        agent_id: String,
    },
    /// Sessionless: look up models.dev metadata by model ID (context/input-output
    /// limits/reasoning levels); a disk cache hit replies directly, a miss (new
    /// model) re-fetches before replying
    ModelLookup {
        id: String,
    },
    Compact {
        session_id: String,
        /// The user's special requirements for this summary (the focus notes appended
        /// in the composer after selecting /compact; aligned with ZCode custom
        /// instructions / kimi-code's custom_instruction_block)
        #[serde(default)]
        instruction: Option<String>,
    },
    /// Query the session's MCP server connection list (for settings page display);
    /// replies with Event::McpServerList
    ListMcpServers {
        session_id: String,
    },
    Shutdown,
}
