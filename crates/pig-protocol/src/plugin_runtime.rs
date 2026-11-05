use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const PLUGIN_UI_PROTOCOL_VERSION: u32 = 1;
pub const PLUGIN_SERVICES_PROTOCOL_VERSION: u32 = 1;

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
    #[serde(rename = "ui.publish")]
    UiPublish,
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
        if self.ui_protocol != PLUGIN_UI_PROTOCOL_VERSION
            || self.services_protocol != PLUGIN_SERVICES_PROTOCOL_VERSION
        {
            return Err(PluginProtocolError::new(
                PluginErrorCode::UnsupportedProtocol,
                "the plugin requires an incompatible UI or service protocol",
            ));
        }
        Ok(PluginNegotiation {
            ui_protocol: PLUGIN_UI_PROTOCOL_VERSION,
            services_protocol: PLUGIN_SERVICES_PROTOCOL_VERSION,
            locale: locale.to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiEventKind {
    Invoke,
    Change,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginUiEvent {
    pub contribution: String,
    pub event_id: String,
    pub kind: PluginUiEventKind,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(default)]
    pub context: PluginHostContext,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginUiState {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default = "ui_default_enabled")]
    pub visible: bool,
    #[serde(default = "ui_default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub loading: bool,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(default)]
    pub choices: Option<Vec<crate::PluginUiChoice>>,
    #[serde(default)]
    pub details: Vec<PluginUiDetail>,
    #[serde(default)]
    pub segments: Vec<PluginUiSegment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginUiDetail {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginUiSegment {
    pub label: String,
    #[serde(default)]
    pub tone: PluginUiSegmentTone,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiSegmentTone {
    #[default]
    Muted,
    Accent,
    Success,
    Warning,
    Danger,
}

fn ui_default_enabled() -> bool {
    true
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

pub const PLUGIN_HOST_API_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginHostContext {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginHostRequest {
    pub api_version: u32,
    pub instance: String,
    pub surface: String,
    pub contribution: String,
    pub revision: u64,
    pub request_id: String,
    pub method: PluginHostMethod,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginHostMethod {
    #[serde(rename = "session.snapshot")]
    SessionSnapshot,
    #[serde(rename = "session.subscribe")]
    SessionSubscribe,
    #[serde(rename = "session.unsubscribe")]
    SessionUnsubscribe,
    #[serde(rename = "storage.read")]
    StorageRead,
    #[serde(rename = "storage.write")]
    StorageWrite,
    #[serde(rename = "navigation.open")]
    NavigationOpen,
    #[serde(rename = "overlay.close")]
    OverlayClose,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginHostResponse {
    pub api_version: u32,
    pub surface: String,
    pub revision: u64,
    pub request_id: String,
    pub result: Result<serde_json::Value, PluginProtocolError>,
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
        tracker.accept(PluginStream::Ui, 1).unwrap();
        tracker.accept(PluginStream::Services, 1).unwrap();
        assert!(tracker.accept(PluginStream::Ui, 1).is_err());
        assert!(tracker.accept(PluginStream::Ui, 2).is_ok());
        assert_eq!(tracker.next(PluginStream::Ui), 3);
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
    fn envelope_json_has_the_stable_protocol_shape() {
        let envelope = PluginEnvelope::new(
            PluginMessageKind::UiPublish,
            "moon:sha256:test:user:default",
            3,
            PluginStream::Ui,
            42,
            18,
            serde_json::json!([]),
        )
        .with_request_id("request-1");
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["protocol"], 1);
        assert_eq!(json["kind"], "ui.publish");
        assert_eq!(json["stream"], "ui");
        assert_eq!(json["requestId"], "request-1");
        assert_eq!(json["generation"], 18);
        envelope.validate().unwrap();
    }
}
