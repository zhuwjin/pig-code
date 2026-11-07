use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const PLUGIN_PROTOCOL_VERSION: u32 = 1;
pub const PLUGIN_SERVICES_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStream {
    Lifecycle,
    Events,
    Contributions,
    Tools,
    Services,
    Logs,
}

impl PluginStream {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lifecycle => "lifecycle",
            Self::Events => "events",
            Self::Contributions => "contributions",
            Self::Tools => "tools",
            Self::Services => "services",
            Self::Logs => "logs",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginMessageKind {
    #[serde(rename = "lifecycle.hello")]
    LifecycleHello,
    #[serde(rename = "lifecycle.start")]
    LifecycleStart,
    #[serde(rename = "lifecycle.ready")]
    LifecycleReady,
    #[serde(rename = "lifecycle.health")]
    LifecycleHealth,
    #[serde(rename = "lifecycle.stop")]
    LifecycleStop,
    #[serde(rename = "lifecycle.failed")]
    LifecycleFailed,
    #[serde(rename = "contribution.register")]
    ContributionRegister,
    #[serde(rename = "contribution.remove")]
    ContributionRemove,
    #[serde(rename = "service.request")]
    ServiceRequest,
    #[serde(rename = "service.response")]
    ServiceResponse,
    #[serde(rename = "service.event")]
    ServiceEvent,
    #[serde(rename = "tools.call")]
    ToolsCall,
    #[serde(rename = "tools.select")]
    ToolsSelect,
    #[serde(rename = "tools.result")]
    ToolsResult,
    #[serde(rename = "tools.cancel")]
    ToolsCancel,
    #[serde(rename = "log.event")]
    LogEvent,
    #[serde(rename = "host.action")]
    HostAction,
    #[serde(rename = "runtime")]
    Runtime,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginEnvelope {
    pub protocol: u32,
    pub kind: PluginMessageKind,
    pub instance: String,
    pub epoch: u64,
    pub stream: PluginStream,
    pub sequence: u64,
    #[serde(default)]
    pub request_id: Option<String>,
    pub generation: u64,
    pub body: serde_json::Value,
}

impl PluginEnvelope {
    pub fn new(
        kind: PluginMessageKind,
        instance: impl Into<String>,
        epoch: u64,
        stream: PluginStream,
        sequence: u64,
        generation: u64,
        body: serde_json::Value,
    ) -> Self {
        Self {
            protocol: PLUGIN_PROTOCOL_VERSION,
            kind,
            instance: instance.into(),
            epoch,
            stream,
            sequence,
            request_id: None,
            generation,
            body,
        }
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn validate(&self) -> Result<(), PluginProtocolError> {
        if self.protocol != PLUGIN_PROTOCOL_VERSION {
            return Err(PluginProtocolError::new(
                PluginErrorCode::UnsupportedProtocol,
                format!("unsupported plugin protocol {}", self.protocol),
            ));
        }
        if self.instance.is_empty() || self.sequence == 0 {
            return Err(PluginProtocolError::new(
                PluginErrorCode::InvalidEnvelope,
                "plugin instance and a positive sequence are required",
            ));
        }
        if self.body.is_null()
            && !matches!(
                self.kind,
                PluginMessageKind::LifecycleHealth | PluginMessageKind::LifecycleStop
            )
        {
            return Err(PluginProtocolError::new(
                PluginErrorCode::InvalidEnvelope,
                "this message requires a body",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginSequenceTracker {
    streams: BTreeMap<PluginStream, u64>,
}

impl PluginSequenceTracker {
    pub fn next(&mut self, stream: PluginStream) -> u64 {
        let sequence = self.streams.entry(stream).or_default();
        *sequence += 1;
        *sequence
    }

    pub fn accept(
        &mut self,
        stream: PluginStream,
        sequence: u64,
    ) -> Result<(), PluginProtocolError> {
        let previous = self.streams.entry(stream).or_default();
        if sequence <= *previous {
            return Err(PluginProtocolError::new(
                PluginErrorCode::StaleSequence,
                format!("stale {stream:?} sequence {sequence}; last accepted is {previous}"),
            ));
        }
        *previous = sequence;
        Ok(())
    }

    pub fn reset(&mut self) {
        self.streams.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginErrorCode {
    UnsupportedProtocol,
    UnsupportedCapability,
    InvalidEnvelope,
    StaleSequence,
    StaleEpoch,
    StaleGeneration,
    StaleRevision,
    PermissionDenied,
    InvalidContribution,
    InvalidOperation,
    /// Tool arguments do not satisfy the declared input JSON Schema.
    InvalidInput,
    /// A tool result does not satisfy the declared output JSON Schema.
    InvalidOutput,
    /// A tool's concurrency limit is reached or the same call is in flight.
    Busy,
    ProtectedTarget,
    QuotaExceeded,
    Timeout,
    Cancelled,
    NotActive,
    NotFound,
    Transport,
    Internal,
}

impl PluginErrorCode {
    /// Stable wire name (the same string serde writes).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedProtocol => "unsupported_protocol",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::InvalidEnvelope => "invalid_envelope",
            Self::StaleSequence => "stale_sequence",
            Self::StaleEpoch => "stale_epoch",
            Self::StaleGeneration => "stale_generation",
            Self::StaleRevision => "stale_revision",
            Self::PermissionDenied => "permission_denied",
            Self::InvalidContribution => "invalid_contribution",
            Self::InvalidOperation => "invalid_operation",
            Self::InvalidInput => "invalid_input",
            Self::InvalidOutput => "invalid_output",
            Self::Busy => "busy",
            Self::ProtectedTarget => "protected_target",
            Self::QuotaExceeded => "quota_exceeded",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::NotActive => "not_active",
            Self::NotFound => "not_found",
            Self::Transport => "transport",
            Self::Internal => "internal",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginProtocolError {
    pub code: PluginErrorCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    /// Structured extras a caller can act on (a stale revision's current
    /// value, a timeout's remaining budget), never a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl PluginProtocolError {
    pub fn new(code: PluginErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            details: None,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn from_core(error: &crate::CoreError) -> Self {
        let code = match error {
            crate::CoreError::PluginPermission { .. } => PluginErrorCode::PermissionDenied,
            crate::CoreError::PluginManifest { .. } => PluginErrorCode::InvalidContribution,
            crate::CoreError::PluginInvalidInput { .. } => PluginErrorCode::InvalidInput,
            crate::CoreError::PluginTransport { .. } => PluginErrorCode::Transport,
            _ => PluginErrorCode::Internal,
        };
        let message = match error {
            crate::CoreError::PluginPermission { detail }
            | crate::CoreError::PluginManifest { detail }
            | crate::CoreError::PluginInvalidInput { detail }
            | crate::CoreError::PluginTransport { detail }
            | crate::CoreError::PluginWrite { detail }
            | crate::CoreError::CredentialStore { detail, .. } => detail.clone(),
            other => format!("{other:?}"),
        };
        Self::new(code, message)
    }
}

impl std::fmt::Display for PluginProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginHello {
    pub ui_protocol: u32,
    pub services_protocol: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginNegotiation {
    pub ui_protocol: u32,
    pub services_protocol: u32,
    pub locale: String,
}

impl PluginHello {
    pub fn negotiate(&self, locale: &str) -> Result<PluginNegotiation, PluginProtocolError> {
        if self.ui_protocol != PLUGIN_PROTOCOL_VERSION
            || self.services_protocol != PLUGIN_SERVICES_PROTOCOL_VERSION
        {
            return Err(PluginProtocolError::new(
                PluginErrorCode::UnsupportedProtocol,
                "the plugin requires an incompatible UI or service protocol",
            ));
        }
        Ok(PluginNegotiation {
            ui_protocol: PLUGIN_PROTOCOL_VERSION,
            services_protocol: PLUGIN_SERVICES_PROTOCOL_VERSION,
            locale: locale.to_owned(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginRuntimeContributionRequest {
    pub request_id: String,
    pub package: String,
    pub digest: String,
    pub epoch: u64,
    pub generation: u64,
    pub contributions: crate::PluginContributions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginServiceRequest {
    pub service: PluginService,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// A stable key for a side-effecting request: the host records its result
    /// and replays it instead of executing the effect again (including after a
    /// restart). Without one, the request's own id is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// A read-only Git operation. The host derives the repository from the
/// session's workspace; callers never supply a working directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginGitReadRequest {
    Status,
    Diff {
        path: String,
        #[serde(default)]
        staged: bool,
    },
    Log {
        /// 1..=100; the host default is 20.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Show {
        /// A revision or ref accepted by `git show`; leading `-` is rejected.
        rev: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Branches,
}

/// One bounded workspace-relative text read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginWorkspaceFileReadRequest {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
    /// Returned content budget (default 256 KiB, max 1 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

impl PluginService {
    /// Stable wire name (the same string serde writes).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StorageRead => "storage_read",
            Self::StorageWrite => "storage_write",
            Self::ConfigRead => "config_read",
            Self::ConfigWrite => "config_write",
            Self::HttpRequest => "http_request",
            Self::CredentialSet => "credential_set",
            Self::CredentialClear => "credential_clear",
            Self::SessionRead => "session_read",
            Self::WorkspaceRead => "workspace_read",
            Self::NavigationOpen => "navigation_open",
            Self::ComposerSubmit => "composer_submit",
            Self::ResourceRead => "resource_read",
            Self::LocaleRead => "locale_read",
            Self::StorageList => "storage_list",
            Self::StorageKeyRead => "storage_key_read",
            Self::StorageKeyWrite => "storage_key_write",
            Self::StorageKeyDelete => "storage_key_delete",
            Self::SessionSubscribe => "session_subscribe",
            Self::SessionUnsubscribe => "session_unsubscribe",
            Self::SessionCreate => "session_create",
            Self::SessionSend => "session_send",
            Self::SessionInterrupt => "session_interrupt",
            Self::SessionWait => "session_wait",
            Self::ModelRequest => "model_request",
            Self::AgentList => "agent_list",
            Self::AgentStart => "agent_start",
            Self::AgentSend => "agent_send",
            Self::AgentWait => "agent_wait",
            Self::AgentStop => "agent_stop",
            Self::AgentRead => "agent_read",
            Self::GitStatus => "git_status",
            Self::GitDiff => "git_diff",
            Self::GitLog => "git_log",
            Self::GitShow => "git_show",
            Self::GitBranches => "git_branches",
            Self::WorkspaceFileRead => "workspace_file_read",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginService {
    StorageRead,
    StorageWrite,
    ConfigRead,
    ConfigWrite,
    HttpRequest,
    CredentialSet,
    CredentialClear,
    SessionRead,
    WorkspaceRead,
    NavigationOpen,
    ComposerSubmit,
    ResourceRead,
    LocaleRead,
    // Keyed private storage (revision CAS).
    StorageList,
    StorageKeyRead,
    StorageKeyWrite,
    StorageKeyDelete,
    // Session observation and control.
    SessionSubscribe,
    SessionUnsubscribe,
    SessionCreate,
    SessionSend,
    SessionInterrupt,
    SessionWait,
    // A controlled one-shot model call.
    ModelRequest,
    // Subagent profiles and delegation.
    AgentList,
    AgentStart,
    AgentSend,
    AgentWait,
    AgentStop,
    AgentRead,
    // Read-only workspace inspection.
    GitStatus,
    GitDiff,
    GitLog,
    GitShow,
    GitBranches,
    WorkspaceFileRead,
}

pub const PLUGIN_HOST_NAVIGATION_TARGETS: &[&str] =
    &["main", "models", "appearance", "plugins", "marketplace"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginServiceResponse {
    pub service: PluginService,
    pub result: Result<serde_json::Value, PluginProtocolError>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginContextUsage {
    pub used: u64,
    pub total: u64,
    pub cache_read_total: u64,
    pub input_total: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginSessionStats {
    pub running: bool,
    pub output_rate: u64,
    pub turns: u64,
    pub steps: u64,
    pub output: u64,
    pub decode_ms: u64,
    pub api_ms: u64,
    pub tool_ms: u64,
    pub ttft_ms: u64,
    pub ttft_steps: u64,
    pub usage: Option<PluginContextUsage>,
    pub breakdown: Option<crate::ContextBreakdown>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginTurnStats {
    pub id: Option<String>,
    pub index: u64,
    pub duration_ms: u64,
    pub stats: Option<crate::TurnUsageStats>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginSessionProjection {
    pub id: String,
    pub cwd: std::path::PathBuf,
    #[serde(default)]
    pub stats: PluginSessionStats,
    #[serde(default)]
    pub turns: Vec<PluginTurnStats>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginResource {
    pub mime: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginLifecycleState {
    Discovered,
    Verified,
    Installed,
    Enabled,
    #[default]
    Preparing,
    Starting,
    Retrying,
    Active,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginRuntimeMetrics {
    #[serde(default)]
    pub pending_requests: usize,
    #[serde(default)]
    pub timed_out_requests: u64,
    #[serde(default)]
    pub cancelled_requests: u64,
    #[serde(default)]
    pub queue_depth: usize,
    #[serde(default)]
    pub dropped_events: u64,
    #[serde(default)]
    pub network_bytes: u64,
    #[serde(default)]
    pub storage_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginRuntimeSnapshot {
    pub instance: String,
    pub state: PluginLifecycleState,
    pub epoch: u64,
    pub generation: u64,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub metrics: PluginRuntimeMetrics,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_replay_and_sequence_validation_is_per_stream() {
        let mut tracker = PluginSequenceTracker::default();
        tracker.accept(PluginStream::Tools, 1).unwrap();
        tracker.accept(PluginStream::Services, 1).unwrap();
        assert!(tracker.accept(PluginStream::Tools, 1).is_err());
        assert!(tracker.accept(PluginStream::Tools, 2).is_ok());
        assert_eq!(tracker.next(PluginStream::Tools), 3);
    }

    #[test]
    fn negotiation_checks_protocol_versions() {
        let hello = PluginHello {
            ui_protocol: 1,
            services_protocol: 1,
        };
        let accepted = hello.negotiate("zh-CN").unwrap();
        assert_eq!(accepted.locale, "zh-CN");
        assert!(
            PluginHello {
                ui_protocol: 2,
                ..hello
            }
            .negotiate("zh-CN")
            .is_err()
        );
    }

    #[test]
    fn workspace_read_requests_parse_the_sdk_shapes() {
        let git: PluginGitReadRequest = serde_json::from_value(serde_json::json!({
            "op": "diff",
            "path": "src/lib.rs",
            "staged": true
        }))
        .expect("git diff request");
        assert_eq!(
            git,
            PluginGitReadRequest::Diff {
                path: "src/lib.rs".into(),
                staged: true
            }
        );
        let file: PluginWorkspaceFileReadRequest = serde_json::from_value(serde_json::json!({
            "path": "src/lib.rs",
            "startLine": 2,
            "endLine": 3,
            "maxBytes": 1024
        }))
        .expect("file read request");
        assert_eq!(file.start_line, Some(2));
        assert_eq!(file.end_line, Some(3));
        assert_eq!(file.max_bytes, Some(1024));
    }

    #[test]
    fn envelope_json_has_the_stable_protocol_shape() {
        let envelope = PluginEnvelope::new(
            PluginMessageKind::ToolsCall,
            "moon:sha256:test:user:default",
            3,
            PluginStream::Tools,
            42,
            18,
            serde_json::json!({"tool": "MoonEcho", "args": {"text": "hello"}}),
        )
        .with_request_id("request-1");
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["protocol"], 1);
        assert_eq!(json["kind"], "tools.call");
        assert_eq!(json["stream"], "tools");
        assert_eq!(json["requestId"], "request-1");
        assert_eq!(json["generation"], 18);
        envelope.validate().unwrap();
    }
}
