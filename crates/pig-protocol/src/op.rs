use super::*;

/// A session action a plugin requests through its host services. The core
/// runner is the only place that touches sessions, so every action arrives as
/// one narrow Op and answers with one `Event::PluginSessionResult`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginSessionAction {
    /// Create a session in `cwd` (the plugin's declared workspace), with the
    /// same permission settings the UI would use.
    Create {
        cwd: PathBuf,
        #[serde(default)]
        provider_id: Option<String>,
        #[serde(default)]
        model_id: Option<String>,
        #[serde(default)]
        reasoning_level: Option<String>,
        #[serde(default)]
        permissions: Option<PermissionSettings>,
    },
    /// Send a message to an existing session under its own current
    /// permissions (a plugin can never widen them).
    Send {
        session_id: String,
        content: String,
        #[serde(default)]
        delivery: MessageDelivery,
    },
    /// Stop the session's running turn.
    Interrupt { session_id: String },
    /// Wait for the session's running turn, or answer `idle` at once.
    Wait { session_id: String, timeout_ms: u64 },
    /// One non-streaming model call with a purpose and explicit limits.
    Model {
        #[serde(default)]
        session_id: Option<String>,
        purpose: String,
        #[serde(default)]
        system: Option<String>,
        prompt: String,
        #[serde(default)]
        max_output_tokens: Option<u64>,
    },
    /// The subagent profiles a session can delegate to.
    AgentList {
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Start a delegated subagent in an idle session.
    AgentStart {
        session_id: String,
        profile: String,
        prompt: String,
        description: String,
        #[serde(default)]
        background: bool,
    },
    /// Steer a running delegated subagent.
    AgentSend {
        session_id: String,
        agent_id: String,
        message: String,
    },
    /// Wait for a delegated subagent to settle.
    AgentWait {
        session_id: String,
        agent_id: String,
        timeout_ms: u64,
    },
    /// Stop a delegated subagent.
    AgentStop {
        session_id: String,
        agent_id: String,
    },
    /// Read a delegated subagent's conversation projection.
    AgentRead {
        session_id: String,
        agent_id: String,
    },
}

/// UI → core commands
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
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
        /// Current permissions in the UI; None → the workspace's most recently
        /// active session / default
        permissions: Option<PermissionSettings>,
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
    /// Recall the last user message: truncates the last User rollout record and
    /// everything after it (the whole turn it triggered) out of the model context
    /// and the UI transcript. Append-only on disk (a Recall marker record), so the
    /// turn's usage statistics survive. Rejected while a turn is in flight
    RecallLastMessage {
        session_id: String,
    },
    SendMessage {
        session_id: String,
        content: String,
        files: Vec<String>,
        /// Pasted/dragged-in images (raw bytes; core compresses them before they
        /// enter the model context)
        #[serde(default)]
        images: Vec<PendingImage>,
        /// Permissions pinned to this message (the turn runs under them even if
        /// the session's settings change while it is queued)
        permissions: PermissionSettings,
        /// Delivery intent while a turn is running: Queue (default, runs as
        /// the next turn) or Steer (inject into the running turn at the next
        /// safe boundary). Steer falls back to the queue when the turn cannot
        /// be steered (compaction in flight, attachments, oversized text);
        /// ignored while idle
        #[serde(default)]
        delivery: MessageDelivery,
    },
    Interrupt {
        session_id: String,
    },
    /// Skip the session's active model-call retry wait (the next attempt starts
    /// immediately); no-op when no wait is in flight
    RetryNow {
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
    SetProviderApiKey {
        provider_id: String,
        secret: SecretString,
    },
    ClearProviderApiKey {
        provider_id: String,
    },
    /// Replace the session's permission state (the UI sends a preset's
    /// `permissions()`; a future settings page can send any combination)
    SetPermissions {
        session_id: String,
        permissions: PermissionSettings,
    },
    /// Plan mode switch (orthogonal to exec mode; the model toggles it on its side
    /// via EnterPlanMode/ExitPlanMode)
    SetPlanMode {
        session_id: String,
        enabled: bool,
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
    /// Cancel one pending message by id (works on both lanes: the future-turn
    /// queue and the steer mailbox of the running turn)
    CancelQueued {
        session_id: String,
        id: String,
    },
    /// Move one pending message before an anchor in its own lane (None = lane
    /// tail; a missing anchor degrades to the lane tail, the item is never lost)
    ReorderQueued {
        session_id: String,
        id: String,
        before_id: Option<String>,
    },
    /// Move one pending message between the queue and steer lanes without
    /// changing its id or its relative order within the shared pending list
    MoveQueuedLane {
        session_id: String,
        id: String,
        steer: bool,
    },
    /// Resume a paused queue (QueueDrainChanged with auto_drain=false): the
    /// queue-lane head starts as the next turn
    ResumeQueue {
        session_id: String,
    },
    /// Resolve a message held by the paused-queue confirmation flow
    /// (Event::HeldQueuePrompt)
    ResolveHeldQueue {
        session_id: String,
        disposition: HeldDisposition,
    },
    /// Send one queued message now: while a turn runs this cancels it (the
    /// wrap-up then starts the moved-to-front item as a fresh turn); while
    /// idle the item starts immediately
    SendQueuedNow {
        session_id: String,
        id: String,
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
    /// Trust the current workspace's project hooks (.pigcode/hooks.toml): the
    /// canonical root is persisted into hooks.trusted_projects and the live
    /// session's project hooks become active from the next event on; replies
    /// with Event::ProjectHooksTrusted
    TrustProjectHooks {
        session_id: String,
    },
    SyncPlugins {
        packages: Vec<PluginPackageSource>,
    },
    SetPluginEnabled {
        package: String,
        digest: String,
        enabled: bool,
    },
    SyncPluginRuntimeContributions {
        request_id: String,
        package: String,
        digest: String,
        epoch: u64,
        generation: u64,
        contributions: PluginContributions,
    },
    CommitPluginUpdate {
        package: String,
        digest: String,
    },
    CancelPluginUpdate {
        package: String,
        digest: String,
    },
    RefreshPluginMarketplace,
    InstallMarketplacePlugin {
        package: PluginMarketplacePackage,
    },
    QueryPlugin {
        request_id: String,
        package: String,
        digest: String,
        scope: PluginScope,
        session_id: Option<String>,
    },
    PluginAction {
        request_id: String,
        package: String,
        digest: String,
        action: PluginAction,
        session_id: Option<String>,
    },
    PluginToolResult {
        result: PluginToolResult,
    },
    PluginToolsSelection {
        selection: PluginToolsSelection,
    },
    PluginSessionAction {
        request_id: String,
        package: String,
        digest: String,
        action: PluginSessionAction,
    },
    PluginHttpRequest {
        request_id: String,
        package: String,
        digest: String,
        request: PluginHttpRequest,
    },
    SetPluginSecret {
        request_id: String,
        package: String,
        digest: String,
        credential: String,
        secret: SecretString,
    },
    ClearPluginSecret {
        request_id: String,
        package: String,
        digest: String,
        credential: String,
    },
    SetPluginCredentialGrant {
        package: String,
        digest: String,
        credential: String,
        origin: String,
        allowed: bool,
    },
    Shutdown,
}
