//! Session-scoped shell and subagent tasks share one registry and TaskChange
//! channel. Task facts and the notification outbox live in SQLite; watchers
//! publish terminal snapshots only after output is drained. Foreground commands
//! that time out continue in the background, and their spill file
//! ({data_dir}/sessions/{session_id}/tool-results/{id}.log) keeps the full
//! output. Entries still running in the foreground are hidden
//! from snapshots (a plain command is not a background task); foreground is
//! flipped only when a timeout converts it to a background task, making it
//! visible on screen.

use crate::NoConsoleExt as _;
use std::path::{Path, PathBuf};

mod access;
pub mod notification;
mod output;
mod process;
mod registry;
mod sandbox;
mod shell;
mod spill;
#[cfg(test)]
mod tests;

pub use access::{
    CredentialGrant, SandboxDenialView, credential_grant, credential_write_is_high_risk,
    metadata_carve_outs, sandbox_read_denial,
};
pub use notification::TaskChange;
pub use process::{ForegroundOutcome, run_foreground, spawn_background, stop_task};
#[cfg(test)]
pub(crate) use registry::SUBAGENT_SLOT_TEST_LOCK;
pub use registry::{
    AgentPartial, AgentRunMetadata, AgentWriterGuard, MAX_CONCURRENT_SUBAGENTS, ReadState,
    SessionToolState, TaskEntry, TaskRegistry, acquire_subagent_slot, mark_agent_task_started,
    note_output, register_agent_run_queued, register_agent_task, register_agent_task_queued,
    snapshot, subagent_slots_available, tail_chars,
};

pub(crate) use output::kill_process_tree;
pub(crate) use process::{run_foreground_with_profile, spawn_background_with_profile};
pub(crate) use registry::{
    AgentSteer, TaskWaitInterest, mark_agent_notification_delivered, task_wait_delivered,
    unregister_agent_steers,
};

pub use sandbox::AdditionalPermissions;
pub use sandbox::MissingPathBehavior;
pub use sandbox::SandboxAccess;
pub use sandbox::SandboxEntry;
pub use sandbox::SandboxKind;
pub use sandbox::SandboxOverride;
pub use sandbox::SandboxPath;
pub use sandbox::SandboxProfile;
pub use sandbox::SandboxSettings;
pub use sandbox::SandboxSpecialPath;
#[cfg(target_os = "windows")]
pub(crate) use sandbox::helper_main;
/// User-level sandbox settings: platform-neutral, because the read-deny table is
/// enforced by every backend (Seatbelt included).
pub use sandbox::init_sandbox_settings;
#[cfg(target_os = "windows")]
pub use sandbox::init_windows_sandbox_network;
#[cfg(target_os = "windows")]
pub use sandbox::{
    SandboxDenyRuleKind, SandboxReadiness, sandbox_classify_deny_rule, sandbox_readiness,
    validate_deny_rule,
};
pub(crate) use sandbox::{
    canonical_root, denial_summary, sandbox_profile_for, session_write_roots,
    validate_project_filesystem_entry, within as sandbox_within,
};
pub(crate) use shell::*;
pub use shell::{
    WindowsSandboxShell, WindowsShell, available_hook_shells, shell_label, windows_sandbox_shell,
    windows_shell,
};
