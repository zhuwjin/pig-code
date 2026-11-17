//! Contract types between Pig Code's UI and core (see docs/PLAN.md §2.2).
//! Pure serde types, no business logic; both UI and core depend on this crate.
//!
//! Principle: delta events (streaming fragments, live-only) are separated from done
//! events (full final values, replayable); session events carry session_id. Ordering
//! is channel order. EventEnvelope adds journal identity and a replay cursor.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Images attached when sending a message (clipboard paste): an in-process channel,
/// raw bytes + mime. core compresses them into the model context; on-disk persistence
/// uses the rollout's ImageRef (no base64 stored).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingImage {
    pub bytes: Vec<u8>,
    pub mime: String,
}

mod config;
mod error;
mod event;
mod op;
mod plugin;
mod plugin_runtime;

pub use config::*;
pub use error::*;
pub use event::*;
pub use op::*;
pub use plugin::*;
pub use plugin_runtime::*;

/// Sandbox policy (Codex's permission profile): the filesystem boundary.
///
/// Only `mode` exists today; future fields (extra writable roots, a per-session
/// network policy, named profiles from config) are additive with serde defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSettings {
    pub mode: SandboxMode,
}

/// The three sandbox modes, named like Codex's `sandbox_mode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// When the agent may pause for approval (Codex's `AskForApproval`).
///
/// `OnRequest` asks at a boundary (an outside-workspace write, a high-risk
/// command, a credential file); `Never` never asks and fails the request
/// instead. A future `Granular` value is an additional variant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    OnRequest,
    Never,
}

/// Who reviews an eligible approval request (Codex's `ApprovalsReviewer`).
///
/// Only `User` exists today; an automatic reviewer is an additional variant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalsReviewer {
    #[default]
    User,
    Policy,
}

/// Canonical permission state: Codex's three orthogonal axes.
///
/// Presets ([`ExecMode`]) are UI shortcuts that fill all three; the state that
/// is persisted, sent, and enforced is this struct. New dimensions' values are
/// added as variants/fields with serde defaults, never by reshaping this type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSettings {
    pub sandbox: SandboxSettings,
    pub approval: ApprovalPolicy,
    pub reviewer: ApprovalsReviewer,
}

impl PermissionSettings {
    /// The preset this state corresponds to, if any (the UI uses it to label
    /// the mode chip; a future custom combination simply returns `None`).
    pub const fn preset(self) -> Option<ExecMode> {
        match (self.sandbox.mode, self.approval, self.reviewer) {
            (SandboxMode::ReadOnly, ApprovalPolicy::OnRequest, ApprovalsReviewer::User) => {
                Some(ExecMode::ReadOnly)
            }
            (SandboxMode::WorkspaceWrite, ApprovalPolicy::OnRequest, ApprovalsReviewer::User) => {
                Some(ExecMode::Auto)
            }
            (SandboxMode::DangerFullAccess, ApprovalPolicy::Never, ApprovalsReviewer::User) => {
                Some(ExecMode::Full)
            }
            _ => None,
        }
    }
}

/// Execution mode preset (Codex's `ApprovalPreset`): a named combination of the
/// three permission axes, kept as the UI's picker values.
///
/// - `ReadOnly`: model Bash runs in a read-only sandbox without routine
///   approvals. The model decides whether a command needs write access and
///   requests it explicitly (`sandbox_permissions: with_additional_permissions`);
///   an unrequested write is rejected by the sandbox.
/// - `Auto`: tools run inside the sandbox without routine approvals; dangerous
///   commands and writes outside the workspace still ask.
/// - `Full`: no sandbox and no routine approvals; dangerous commands are
///   rejected outright and writes outside the workspace do not ask.
///
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecMode {
    #[default]
    ReadOnly,
    Auto,
    Full,
}

impl ExecMode {
    /// Resolve this preset into the three permission axes.
    pub const fn permissions(self) -> PermissionSettings {
        let (mode, approval) = match self {
            Self::ReadOnly => (SandboxMode::ReadOnly, ApprovalPolicy::OnRequest),
            Self::Auto => (SandboxMode::WorkspaceWrite, ApprovalPolicy::OnRequest),
            Self::Full => (SandboxMode::DangerFullAccess, ApprovalPolicy::Never),
        };
        PermissionSettings {
            sandbox: SandboxSettings { mode },
            approval,
            reviewer: ApprovalsReviewer::User,
        }
    }
}

/// File diff produced by a single edit (Write/Edit): for inline rendering in UI tool cards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditDiff {
    pub path: String,
    pub unified_diff: String,
    pub additions: u32,
    pub deletions: u32,
}

/// One changed file in the git workspace (an unstaged/staged list item).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitFileChange {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    /// porcelain status: M/A/D/R/C (conflict)/? (untracked)
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    /// Approve this one call.
    Allow,
    /// Approve this one call and remember it as a workspace rule: core appends
    /// a conservative `Tool(<pattern>)` entry to `.pigcode/permissions.toml`
    /// and applies it right away. Offered only for an ordinary tool+subject
    /// approval (Bash/Write/Edit) — never for high-risk commands, sandbox
    /// widenings, credential files or the plan confirmation.
    #[serde(rename = "approved_for_project")]
    ApprovedForProject,
    /// Approve and remember for the rest of the session (Codex's
    /// `ApprovedForSession`): the tool+subject pair is recorded, so a repeat
    /// of the same command/path is approved silently. Never offered for a
    /// sandbox widening (a boundary move is approved one command at a time).
    ApprovedForSession,
    Reject,
}

impl ApprovalDecision {
    /// Stable key for logs and hook payloads.
    pub fn key(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::ApprovedForProject => "approved_for_project",
            Self::ApprovedForSession => "approved_for_session",
            Self::Reject => "reject",
        }
    }
}

/// Delivery intent of a message sent while a turn is running: the next turn
/// after the current one ends (Queue, the default) or injection into the
/// running turn at the next safe boundary (Steer). Steer falls back to the
/// queue when steering is unavailable (compaction turn in flight, attachments,
/// oversized text)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDelivery {
    #[default]
    Queue,
    Steer,
}

impl MessageDelivery {
    /// The other intent (the composer's Ctrl+Enter inverts the configured
    /// default)
    pub fn opposite(self) -> Self {
        match self {
            Self::Queue => Self::Steer,
            Self::Steer => Self::Queue,
        }
    }
}

/// Why the queue's auto-drain paused (Event::QueueDrainChanged.reason)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuePauseReason {
    /// The user stopped the running turn while the queue was non-empty
    Stopped,
    /// The turn ended with a model/provider failure
    Error,
    /// The app restarted with a persisted non-empty queue: restored paused
    /// (no auto-drain across a restart — the running turn's context is gone)
    Restarted,
}

/// Disposition of a message held by the paused-queue confirmation flow
/// (Op::ResolveHeldQueue)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldDisposition {
    /// Drop the queued items and start the held message now
    ClearQueueAndSend,
    /// Keep the queue and append the held message to its tail (still paused
    /// until Op::ResumeQueue)
    KeepQueueAndSend,
    /// Drop the held message (the queue is untouched)
    Cancel,
}

/// One pending message in a lane snapshot (Event::QueueChanged /
/// Event::SteerChanged); ids come from the session's queue counter ("q1"...)
/// and are unique within the session
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub text: String,
    pub permissions: PermissionSettings,
    /// Pasted image attachments (carried so the queue row can badge them and
    /// edit-to-composer can restore them into the input)
    #[serde(default)]
    pub images: Vec<PendingImage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiFormat {
    OpenAiChat,
    AnthropicMessages,
    OpenAiResponses,
}

/// Marker sentence opening the session-title sidecar prompt. Shared
/// vocabulary: pig-core builds the prompt around it, the mock provider (and
/// any test double) recognizes title requests by it.
pub const TITLE_PROMPT_MARKER: &str = "Generate a title for this coding session";

/// Workspace entry (persisted in the store.sqlite workspaces table)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub path: PathBuf,
    pub added_at: u64,
    /// User-defined display name (rename); None uses the directory name
    #[serde(default)]
    pub alias: Option<String>,
    /// Removed from the sidebar (hidden); automatically restored when a new session
    /// is created in that workspace
    #[serde(default)]
    pub hidden: bool,
}

/// Session list metadata (persisted in the store.sqlite sessions table)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub created_at: u64,
    pub updated_at: u64,
    pub pinned: bool,
    pub archived: bool,
    /// Title was manually renamed: auto-naming (generated by the model from the first
    /// message) no longer overwrites it
    #[serde(default)]
    pub title_custom: bool,
    /// Session's most recently used model/reasoning level/permissions (restored
    /// on reopen; new sessions inherit the workspace's most recently active
    /// values).
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub reasoning_level: Option<String>,
    pub permissions: PermissionSettings,
    /// Plan mode (an independent switch orthogonal to exec mode, same semantics as
    /// ZCode's planEnabled): while enabled, modifying tools are hard-rejected;
    /// approving ExitPlanMode only flips this switch, the permission tier stays
    #[serde(default)]
    pub plan_enabled: bool,
}

/// Todo item of the TodoList tool: session-level state, wholly replaced on write.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

mod task;
pub use task::*;

/// Option of AskUserQuestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Single question of AskUserQuestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionItem {
    pub question: String,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_fields_default_for_legacy_data() {
        let legacy_event: Event = serde_json::from_value(serde_json::json!({
            "ToolCallEnd": {
                "session_id": "s",
                "item_id": "call",
                "output": "Stopped",
                "is_error": true,
            }
        }))
        .expect("legacy tool end");
        assert!(matches!(
            legacy_event,
            Event::ToolCallEnd { stopped: false, .. }
        ));
    }

    #[test]
    fn sources_event_carries_the_exact_text_item_and_round_trips() {
        let event = Event::Sources {
            session_id: "s".into(),
            item_id: "text-3".into(),
            sources: vec![
                Source {
                    url: "https://example.com/a".into(),
                    title: "A".into(),
                    snippet: Some("snippet".into()),
                    start_index: Some(0),
                    end_index: Some(4),
                },
                Source {
                    url: "https://example.com/b".into(),
                    title: String::new(),
                    snippet: None,
                    start_index: None,
                    end_index: None,
                },
            ],
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["Sources"]["item_id"], "text-3");
        assert_eq!(
            json["Sources"]["sources"][0]["url"],
            "https://example.com/a"
        );
        assert!(
            json["Sources"]["sources"][1].get("snippet").is_none(),
            "empty optionals stay compact: {json}"
        );
        let back: Event = serde_json::from_value(json).expect("deserialize");
        assert!(matches!(
            back,
            Event::Sources { item_id, sources, .. }
                if item_id == "text-3" && sources.len() == 2
        ));
    }

    /// Every preset maps to its three axes and back, and a custom combination
    /// survives serialization without being coerced into a preset.
    #[test]
    fn permission_settings_presets_and_custom_round_trip() {
        for mode in [ExecMode::ReadOnly, ExecMode::Auto, ExecMode::Full] {
            let settings = mode.permissions();
            assert_eq!(settings.preset(), Some(mode));
            let json = serde_json::to_string(&settings).expect("serialize");
            let back: PermissionSettings = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, settings);
        }
        let custom = PermissionSettings {
            sandbox: SandboxSettings {
                mode: SandboxMode::WorkspaceWrite,
            },
            approval: ApprovalPolicy::Never,
            reviewer: ApprovalsReviewer::User,
        };
        assert_eq!(custom.preset(), None);
        let json = serde_json::to_string(&custom).expect("serialize");
        let back: PermissionSettings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, custom);
    }
}
