use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub const PLUGIN_UI_PROTOCOL_VERSION: u32 = 1;
pub const PLUGIN_SERVICES_PROTOCOL_VERSION: u32 = 1;

pub const PLUGIN_FEATURE_UI_ATOMIC: u64 = 1 << 0;
pub const PLUGIN_FEATURE_UI_HOST_PATCH: u64 = 1 << 1;
pub const PLUGIN_FEATURE_UI_INPUT: u64 = 1 << 2;
pub const PLUGIN_FEATURE_UI_IME: u64 = 1 << 3;
pub const PLUGIN_FEATURE_UI_SCROLL: u64 = 1 << 4;
pub const PLUGIN_FEATURE_UI_ACCESSIBILITY: u64 = 1 << 5;
pub const PLUGIN_FEATURE_DYNAMIC_CONTRIBUTIONS: u64 = 1 << 6;
pub const PLUGIN_FEATURE_SERVICE_CONFIG: u64 = 1 << 7;
pub const PLUGIN_FEATURE_SERVICE_HTTP: u64 = 1 << 8;
pub const PLUGIN_FEATURE_SERVICE_CREDENTIALS: u64 = 1 << 9;
pub const PLUGIN_FEATURE_SERVICE_SESSION: u64 = 1 << 10;
pub const PLUGIN_FEATURE_SERVICE_WORKSPACE: u64 = 1 << 11;
pub const PLUGIN_FEATURE_SERVICE_NAVIGATION: u64 = 1 << 12;
pub const PLUGIN_FEATURE_RESOURCES: u64 = 1 << 13;
pub const PLUGIN_FEATURE_I18N: u64 = 1 << 14;
pub const PLUGIN_FEATURE_LIFECYCLE_HEALTH: u64 = 1 << 15;
pub const PLUGIN_FEATURE_BACKPRESSURE: u64 = 1 << 16;
pub const PLUGIN_HOST_FEATURES: u64 = (1 << 17) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStream {
    Lifecycle,
    Ui,
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
            Self::Ui => "ui",
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
    #[serde(rename = "ui.commit")]
    UiCommit,
    #[serde(rename = "ui.ack")]
    UiAck,
    #[serde(rename = "ui.event")]
    UiEvent,
    #[serde(rename = "ui.activate")]
    UiActivate,
    #[serde(rename = "ui.reset")]
    UiReset,
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
            protocol: PLUGIN_UI_PROTOCOL_VERSION,
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
        if self.protocol != PLUGIN_UI_PROTOCOL_VERSION {
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
    ProtectedTarget,
    QuotaExceeded,
    Timeout,
    Cancelled,
    NotActive,
    NotFound,
    Transport,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginProtocolError {
    pub code: PluginErrorCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

impl PluginProtocolError {
    pub fn new(code: PluginErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn from_core(error: &crate::CoreError) -> Self {
        let code = match error {
            crate::CoreError::PluginPermission { .. } => PluginErrorCode::PermissionDenied,
            crate::CoreError::PluginManifest { .. } => PluginErrorCode::InvalidContribution,
            crate::CoreError::PluginTransport { .. } => PluginErrorCode::Transport,
            _ => PluginErrorCode::Internal,
        };
        let message = match error {
            crate::CoreError::PluginPermission { detail }
            | crate::CoreError::PluginManifest { detail }
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
    #[serde(default)]
    pub required_features: u64,
    #[serde(default)]
    pub optional_features: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginNegotiation {
    pub ui_protocol: u32,
    pub services_protocol: u32,
    pub features: u64,
    pub rejected_optional_features: u64,
    pub locale: String,
}

impl PluginHello {
    pub fn negotiate(&self, locale: &str) -> Result<PluginNegotiation, PluginProtocolError> {
        if self.ui_protocol != PLUGIN_UI_PROTOCOL_VERSION
            || self.services_protocol != PLUGIN_SERVICES_PROTOCOL_VERSION
        {
            return Err(PluginProtocolError::new(
                PluginErrorCode::UnsupportedProtocol,
                "the plugin requires an incompatible UI or service protocol",
            ));
        }
        let missing = self.required_features & !PLUGIN_HOST_FEATURES;
        if missing != 0 {
            return Err(PluginProtocolError::new(
                PluginErrorCode::UnsupportedCapability,
                format!("required host features are unavailable: 0x{missing:x}"),
            ));
        }
        let requested = self.required_features | self.optional_features;
        Ok(PluginNegotiation {
            ui_protocol: PLUGIN_UI_PROTOCOL_VERSION,
            services_protocol: PLUGIN_SERVICES_PROTOCOL_VERSION,
            features: requested & PLUGIN_HOST_FEATURES,
            rejected_optional_features: self.optional_features & !PLUGIN_HOST_FEATURES,
            locale: locale.to_owned(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteDocument {
    pub instance: String,
    pub epoch: u64,
    pub generation: u64,
    pub sequence: u64,
    #[serde(default)]
    pub base_revision: u64,
    pub nodes: Vec<RemoteNode>,
    pub roots: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteNode {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub export_name: Option<String>,
    #[serde(default)]
    pub test_id: Option<String>,
    #[serde(default)]
    pub press: bool,
    #[serde(default)]
    pub children: Vec<String>,
    #[serde(default)]
    pub style: serde_json::Value,
    #[serde(default)]
    pub events: BTreeSet<String>,
    #[serde(default)]
    pub props: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub accessibility: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub source: Option<String>,
}

impl RemoteNode {
    pub fn new(id: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            text: None,
            export_name: None,
            test_id: None,
            press: false,
            children: Vec::new(),
            style: serde_json::json!({}),
            events: BTreeSet::new(),
            props: BTreeMap::new(),
            accessibility: BTreeMap::new(),
            source: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginUiEvent {
    pub contribution: String,
    pub node: String,
    pub event_id: String,
    pub kind: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub surface_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginUiRejection {
    pub code: PluginErrorCode,
    pub target: String,
    pub contribution: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginUiAck {
    pub accepted: bool,
    pub revision: u64,
    #[serde(default)]
    pub rejected: Vec<PluginUiRejection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginUiConflict {
    pub target: String,
    pub winner: String,
    pub suppressed: Vec<String>,
    pub reason: String,
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
}

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
    pub nodes: usize,
    #[serde(default)]
    pub operations: u64,
    #[serde(default)]
    pub rejected_operations: u64,
    #[serde(default)]
    pub pending_requests: usize,
    #[serde(default)]
    pub timed_out_requests: u64,
    #[serde(default)]
    pub cancelled_requests: u64,
    #[serde(default)]
    pub queue_depth: usize,
    #[serde(default)]
    pub coalesced_frames: u64,
    #[serde(default)]
    pub dropped_events: u64,
    #[serde(default)]
    pub network_bytes: u64,
    #[serde(default)]
    pub storage_bytes: u64,
    #[serde(default)]
    pub conflicts: Vec<PluginUiConflict>,
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
    pub features: u64,
    #[serde(default)]
    pub metrics: PluginRuntimeMetrics,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_replay_and_sequence_validation_is_per_stream() {
        let mut tracker = PluginSequenceTracker::default();
        tracker.accept(PluginStream::Ui, 1).unwrap();
        tracker.accept(PluginStream::Services, 1).unwrap();
        assert!(tracker.accept(PluginStream::Ui, 1).is_err());
        assert!(tracker.accept(PluginStream::Ui, 2).is_ok());
        assert_eq!(tracker.next(PluginStream::Ui), 3);
    }

    #[test]
    fn negotiation_rejects_missing_required_features_and_reports_optional_ones() {
        let hello = PluginHello {
            ui_protocol: 1,
            services_protocol: 1,
            required_features: PLUGIN_FEATURE_UI_ATOMIC,
            optional_features: 1 << 63,
        };
        let accepted = hello.negotiate("zh-CN").unwrap();
        assert_eq!(accepted.features, PLUGIN_FEATURE_UI_ATOMIC);
        assert_eq!(accepted.rejected_optional_features, 1 << 63);
        assert!(
            PluginHello {
                required_features: 1 << 63,
                ..hello
            }
            .negotiate("zh-CN")
            .is_err()
        );
    }

    #[test]
    fn envelope_json_has_the_stable_protocol_shape() {
        let envelope = PluginEnvelope::new(
            PluginMessageKind::UiCommit,
            "moon:sha256:test:user:default",
            3,
            PluginStream::Ui,
            42,
            18,
            serde_json::json!({"operations": []}),
        )
        .with_request_id("request-1");
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["protocol"], 1);
        assert_eq!(json["kind"], "ui.commit");
        assert_eq!(json["stream"], "ui");
        assert_eq!(json["requestId"], "request-1");
        assert_eq!(json["generation"], 18);
        envelope.validate().unwrap();
    }
}
