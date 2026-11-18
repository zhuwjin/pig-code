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
pub struct CoreErrorContext {
    pub session_id: String,
    pub agent_id: String,
    pub run_id: Option<String>,
    pub attempt: Option<u64>,
    pub call_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoreError {
    // Config read/write
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
    /// A configured provider cannot be used as written. `detail` names the
    /// problem; it never contains a secret value.
    ProviderConfig {
        detail: String,
    },
    CredentialStore {
        credential: String,
        detail: String,
    },
    RevertTurnInFlight,
    /// Recall rejected: a turn (or the compact pseudo-turn) is in flight
    RecallTurnInFlight,
    /// Recall rejected: the session has no user message to recall
    NothingToRecall,
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
    /// The provider rejected the request because the input exceeds the
    /// model's context window (conservatively classified by provider code).
    /// Core may compact and retry once; the detail is the upstream text.
    ContextWindowExceeded {
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
    /// A durable rollout append or sync failed. `detail` says whether the
    /// record was serialized, partially written (later appends are refused), or
    /// merely not synced.
    RolloutWrite {
        path: String,
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
    // Lifecycle hooks
    /// A hook blocked the action (UserPromptSubmit continue:false / exit 2);
    /// detail is the hook-supplied reason
    HookBlocked {
        detail: String,
    },
    TaskStore {
        detail: String,
    },
    PluginManifest {
        detail: String,
    },
    PluginPermission {
        detail: String,
    },
    /// A plugin service request is well-formed but cannot be executed as
    /// written (invalid revision, binary file, line range, size).
    PluginInvalidInput {
        detail: String,
    },
    PluginWrite {
        detail: String,
    },
    PluginTransport {
        detail: String,
    },
    /// Catch-all for unclassified upstream/internal errors: UI shows generic text
    /// plus the English detail
    Internal {
        detail: String,
    },
    Contextual {
        context: CoreErrorContext,
        error: Box<CoreError>,
    },
    /// Fallback for unknown kinds during deserialization (forward compatibility)
    #[serde(other)]
    Unknown,
}

impl CoreError {
    pub fn with_context(self, context: CoreErrorContext) -> Self {
        if matches!(self, Self::Contextual { .. }) {
            self
        } else {
            Self::Contextual {
                context,
                error: Box::new(self),
            }
        }
    }

    pub fn context(&self) -> Option<&CoreErrorContext> {
        match self {
            Self::Contextual { context, .. } => Some(context),
            _ => None,
        }
    }

    pub fn semantic_error(&self) -> &Self {
        match self {
            Self::Contextual { error, .. } => error.semantic_error(),
            other => other,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contextual_errors_keep_identity_and_semantic_payload() {
        let error = CoreError::StreamRead {
            detail: "connection reset".into(),
        }
        .with_context(CoreErrorContext {
            session_id: "s1".into(),
            agent_id: "a1".into(),
            run_id: Some("r1".into()),
            attempt: Some(2),
            call_id: Some("c1".into()),
        });
        let encoded = serde_json::to_value(&error).expect("serialize contextual error");
        assert_eq!(encoded["kind"], "contextual");
        assert_eq!(encoded["context"]["attempt"], 2);
        assert_eq!(encoded["error"]["kind"], "stream_read");
        assert_eq!(
            error.semantic_error().semantic_error(),
            error.semantic_error()
        );
        let decoded: CoreError = serde_json::from_value(encoded).expect("deserialize");
        assert_eq!(decoded, error);
        assert_eq!(
            decoded.context().expect("context").call_id.as_deref(),
            Some("c1")
        );
    }
}
