use super::*;

/// Structured error payload from core to UI: semantic kind + dynamic parameters.
///
/// Architecture convention (Clean/Hexagonal): core is language-agnostic and unaware of
/// localized text — errors carry semantics in an enum, and multilingual conversion is
/// deferred to the UI render point (pig-app `core_error_text`). `detail` is always the
/// original English diagnostic (upstream error `to_string()` / API response text passed
/// through verbatim); the UI shows it as a verifiable note of the original text.
/// Introducing rust-i18n for these strings on the core side is forbidden (pig-app's
/// i18n guard test would catch it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoreError {
    // Config read/write
    ConfigParse {
        detail: String,
    },
    ConfigRead {
        path: String,
        detail: String,
    },
    ConfigParseFile {
        path: String,
        detail: String,
    },
    ConfigSerialize {
        detail: String,
    },
    ConfigWrite {
        path: String,
        detail: String,
    },
    // Session/fork/revert/compact
    ForkSourceMissing {
        id: String,
    },
    ForkMediaDirCreate {
        detail: String,
    },
    ForkMediaCopy {
        path: String,
        detail: String,
    },
    SessionNotFoundOpen,
    SessionNotFound,
    NoModelConfigured,
    CompactUnavailable,
    ProviderNotFound,
    RevertTurnInFlight,
    RevertNotModified,
    RevertWrite {
        path: String,
        detail: String,
    },
    RevertDelete {
        path: String,
        detail: String,
    },
    // Tasks/subagents
    GitTask {
        detail: String,
    },
    SubagentRead {
        detail: String,
    },
    SubagentReadTask {
        detail: String,
    },
    // Network/stream (root cause folded into detail)
    Network {
        detail: String,
    },
    StreamRead {
        detail: String,
    },
    // rollout/persistence
    RolloutNoMeta {
        id: String,
    },
    SessionsDirCreate {
        detail: String,
    },
    RolloutCreate {
        path: String,
        detail: String,
    },
    RolloutOpen {
        path: String,
        detail: String,
    },
    RolloutRead {
        path: String,
        detail: String,
    },
    RolloutLineParse {
        detail: String,
    },
    DataDirCreate {
        path: String,
        detail: String,
    },
    SkillsStateSerialize {
        detail: String,
    },
    SkillsStateWrite {
        detail: String,
    },
    // git
    GitSpawn {
        detail: String,
    },
    GitCheckout {
        detail: String,
    },
    // MCP (McpServerStatus.error reuses this type)
    McpInitialize {
        detail: String,
    },
    McpSpawn {
        command: String,
        detail: String,
    },
    McpNotifyWrite {
        name: String,
        method: String,
        detail: String,
    },
    McpInvalidUrl {
        detail: String,
    },
    McpHeaderBlocked {
        key: String,
    },
    McpHeaderNameInvalid {
        key: String,
    },
    McpHeaderValueInvalid {
        key: String,
    },
    // Project permissions file
    PermissionsParse {
        detail: String,
    },
    /// Catch-all for unclassified upstream/internal errors: UI shows generic text
    /// plus the English detail
    Internal {
        detail: String,
    },
    /// Fallback for unknown kinds during deserialization (forward compatibility)
    #[serde(other)]
    Unknown,
}

/// Placeholder/truncation note for the git panel's diff view (GitDiff.note; the diff
/// field stays pure diff text).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffNote {
    /// Diff exceeded 1MB and was truncated
    Truncated,
    /// File exceeds 1MB, no text diff
    TooLarge,
    /// Binary file, no text diff
    Binary,
}

/// Result of the settings page provider "test connection" (Event::TestResult payload).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ConnTestResult {
    Connected {
        status: u16,
    },
    Timeout {
        secs: u64,
    },
    /// Connection failed: the upstream error's original text (reqwest/HTTP status etc.)
    Failed {
        detail: String,
    },
}
