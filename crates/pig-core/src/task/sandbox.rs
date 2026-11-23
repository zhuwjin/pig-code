use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::SessionToolState;

mod config_entries;
mod credential;
mod deny_rules;
#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod model;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) use windows::helper_main;
#[cfg(target_os = "windows")]
pub use windows::{
    DenyRuleKind as SandboxDenyRuleKind, WindowsSandboxReadiness as SandboxReadiness,
    classify_deny_rule as sandbox_classify_deny_rule, sandbox_readiness, validate_deny_rule,
};

pub(crate) use config_entries::validate_entry as validate_project_filesystem_entry;
use deny_rules::DenyRuleGroup;
pub use model::AdditionalPermissions;
pub use model::MissingPathBehavior;
pub(crate) use model::ResolvedPolicy;
pub use model::SandboxAccess;
pub use model::SandboxEntry;
pub use model::SandboxKind;
pub use model::SandboxPath;
pub(crate) use model::SandboxPolicy;
pub use model::SandboxProfile;
pub use model::SandboxSpecialPath;

pub(crate) use credential::CredentialGrant;
pub(crate) use credential::credential_grant;
pub(crate) use credential::is_high_risk as is_high_risk_credential;

/// The protected credential directories for one session: the user-level list
/// (or the built-in default) resolved against the user profile.
///
/// A workspace's own config cannot change the list; the project layer never
/// carries it (see `ProjectSandboxLayer`).
pub(crate) fn credential_roots(state: &SessionToolState) -> Vec<PathBuf> {
    let settings = windows_sandbox_settings().with_project(
        &state
            .sandbox_project
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    );
    let profile = credential::profile_dir();
    credential::roots(profile.as_deref(), settings.credential_dirs.as_deref()).unwrap_or_else(
        |error| {
            // A malformed list must not silently protect nothing: keep the
            // built-in default and say so.
            tracing::warn!("ignoring the configured credential directories: {error}");
            credential::roots(profile.as_deref(), None).unwrap_or_default()
        },
    )
}
/// The concrete read-denial surface the model needs in its context: credential
/// directories, configured rules and the network policy. Workspace metadata
/// carve-outs are computed separately from the session write roots.
#[derive(Clone, Debug, Default)]
pub(crate) struct SandboxDenialSummary {
    pub credential_dirs: Vec<PathBuf>,
    pub deny_read: Vec<String>,
    pub deny_globs: Vec<String>,
    pub filesystem_denies: Vec<String>,
    pub network_blocked: bool,
}

pub(crate) fn denial_summary(state: &SessionToolState, cwd: &Path) -> SandboxDenialSummary {
    let settings = windows_sandbox_settings().with_project(&model::project_layer(state));
    let mut filesystem_denies = Vec::new();
    if let Ok(entries) = config_entries::compile(&settings.filesystem, cwd) {
        for entry in entries {
            if entry.access != model::SandboxAccess::Deny {
                continue;
            }
            let label = match &entry.path {
                model::SandboxPath::Path { path } => path.display().to_string(),
                model::SandboxPath::GlobPattern { pattern } => pattern.clone(),
                model::SandboxPath::Special { value } => match value {
                    model::SandboxSpecialPath::Root => ":root".to_string(),
                },
            };
            filesystem_denies.push(label);
        }
    }
    let mut deny_globs: Vec<String> = settings
        .deny
        .iter()
        .flat_map(|group| group.globs.iter().cloned())
        .collect();
    deny_globs.sort();
    deny_globs.dedup();
    SandboxDenialSummary {
        credential_dirs: credential_roots(state),
        deny_read: settings.deny_read.clone(),
        deny_globs,
        filesystem_denies,
        network_blocked: network_blocked_for(state),
    }
}

pub(crate) use model::resolve;
pub(crate) use model::session_write_roots;
pub(crate) use model::within;

/// Resolve the sandbox profile from the canonical permission state. Plan mode
/// is orthogonal and always forces read-only, as before.
pub(crate) fn sandbox_profile_for(
    permissions: pig_protocol::PermissionSettings,
    plan_enabled: bool,
) -> SandboxProfile {
    if plan_enabled {
        return SandboxProfile::ReadOnly;
    }
    match permissions.sandbox.mode {
        pig_protocol::SandboxMode::ReadOnly => SandboxProfile::ReadOnly,
        pig_protocol::SandboxMode::WorkspaceWrite => SandboxProfile::WorkspaceWrite,
        pig_protocol::SandboxMode::DangerFullAccess => SandboxProfile::Unrestricted,
    }
}

/// Network access of a sandboxed command.
///
/// `Block` is enforced by the platform sandbox, not by the command: Seatbelt
/// simply omits `(allow network*)` under `(deny default)`, and PSEC passes
/// `NetworkPolicy::Block` to MXC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetworkPolicy {
    Allow,
    Block,
}

impl NetworkPolicy {
    /// Read the process-wide policy. `PIGCODE_SANDBOX_NETWORK=block` denies all
    /// network access to model commands; the saved `[windows] sandbox_network`
    /// is the fallback when the variable is unset. The environment variable wins
    /// so tests and one-off runs can force a policy without rewriting config.
    ///
    /// The default is `Allow` (matching Codex's `network_access = true`
    /// profiles). A real allowlist needs a proxy, see docs/WINDOWS_SANDBOX.md.
    ///
    /// The process-wide policy with a workspace's own `[windows]
    /// sandbox_network` applied on top: the environment variable still wins, so
    /// a one-off run can force a policy the project layer cannot undo.
    fn configured_for(project_block: Option<bool>) -> Self {
        Self::resolve(
            std::env::var("PIGCODE_SANDBOX_NETWORK").ok().as_deref(),
            project_block.unwrap_or_else(windows_network_block),
        )
    }

    /// Pure precedence: an explicit environment value wins, then the saved
    /// `[windows] sandbox_network`, then allow.
    fn resolve(env: Option<&str>, configured_block: bool) -> Self {
        match env {
            Some(value) if value.eq_ignore_ascii_case("block") => Self::Block,
            Some(_) => Self::Allow,
            None if configured_block => Self::Block,
            None => Self::Allow,
        }
    }
}

/// Whether network access is blocked for one session. Full mode runs without a
/// sandbox and therefore cannot enforce it, which is why the caller needs the
/// answer before it spawns. The workspace's own layer can turn the block on; the
/// environment variable still wins over both.
pub(crate) fn network_blocked_for(state: &SessionToolState) -> bool {
    let project = state
        .sandbox_project
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .network_block;
    NetworkPolicy::configured_for(project) == NetworkPolicy::Block
}

/// Configured sandbox network block flag installed at startup and on config save.
static SANDBOX_NETWORK_BLOCK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn windows_network_block() -> bool {
    SANDBOX_NETWORK_BLOCK.load(std::sync::atomic::Ordering::Relaxed)
}

/// Install sandbox network blocking across all platforms (startup and config save).
pub fn init_sandbox_network(block: bool) {
    SANDBOX_NETWORK_BLOCK.store(block, std::sync::atomic::Ordering::Relaxed);
}

/// Cross-platform read-only sandbox availability & configuration snapshot.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlatformSandboxStatus {
    /// Whether `[sandbox] enabled` is currently true in user/system config.
    pub enabled: bool,
    /// Whether this host supports enforcing the OS sandbox backend.
    pub supported: bool,
    /// Active platform backend label (`windows-psec`, `macos-seatbelt`, `linux-bwrap`, `unsupported`).
    pub backend: String,
    /// Actionable diagnostic when `supported` is false.
    pub detail: Option<String>,
}

/// Inspect current OS sandbox enablement and host support without mutating state.
pub fn platform_sandbox_status() -> PlatformSandboxStatus {
    let enabled = windows_sandbox_settings().enabled;
    #[cfg(target_os = "windows")]
    {
        let readiness = windows::sandbox_readiness();
        let supported = readiness.usable();
        let detail = if supported {
            None
        } else {
            Some(
                "Windows process security environment (PSEC) is unavailable on this host. \
                 Set `[sandbox] enabled = false` to explicitly opt out of OS sandboxing."
                    .to_string(),
            )
        };
        PlatformSandboxStatus {
            enabled,
            supported,
            backend: "windows-psec".to_string(),
            detail,
        }
    }
    #[cfg(target_os = "macos")]
    {
        let supported = macos::is_available();
        let detail = if supported {
            None
        } else {
            Some(
                "/usr/bin/sandbox-exec is unavailable on this host. Set `[sandbox] enabled = false` \
                 to explicitly opt out of OS sandboxing."
                    .to_string(),
            )
        };
        PlatformSandboxStatus {
            enabled,
            supported,
            backend: "macos-seatbelt".to_string(),
            detail,
        }
    }
    #[cfg(target_os = "linux")]
    {
        match linux::probe() {
            Ok(_) => PlatformSandboxStatus {
                enabled,
                supported: true,
                backend: "linux-bwrap".to_string(),
                detail: None,
            },
            Err(detail) => PlatformSandboxStatus {
                enabled,
                supported: false,
                backend: "linux-bwrap".to_string(),
                detail: Some(detail),
            },
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        PlatformSandboxStatus {
            enabled,
            supported: false,
            backend: "unsupported".to_string(),
            detail: Some(
                "This platform does not have a supported OS sandbox backend. Set \
                 `[sandbox] enabled = false` to explicitly opt out of OS sandboxing."
                    .to_string(),
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxOverride {
    #[default]
    NoOverride,
    EscalatedWithRestrictions,
}

/// Deny/sandbox settings the policy needs on every launch, installed once at
/// agent startup and refreshed after a config save.
///
/// The read-deny rules are **platform-neutral**: both backends enforce the same
/// compiled rule set.
///
/// A process-wide slot rather than a `SessionToolState` field: the policy is
/// built deep inside the command path, and these values are the **user-level**
/// configuration (a config save applies to the next command of every session).
/// A workspace's own layer is a `SessionToolState` field applied on top of this
/// one at launch time, because it is per workspace rather than per process.
static SANDBOX_DENY_SETTINGS: std::sync::RwLock<Option<SandboxSettings>> =
    std::sync::RwLock::new(None);

/// The sandbox rules in force for one command: the user-level settings, with
/// the workspace's own layer already applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxSettings {
    /// `[sandbox] enabled`: master toggle for OS command sandboxing across all platforms.
    pub(crate) enabled: bool,
    /// `[sandbox] deny_read`: simple rules (a literal path or a glob).
    pub(crate) deny_read: Vec<String>,
    /// `[[sandbox.deny]]`: structured groups with their own exceptions. A
    /// fresh install is seeded with the sensitive-file groups.
    pub(crate) deny: Vec<DenyRuleGroup>,
    /// `[sandbox.filesystem]`: path or pattern to access, in Codex's shape.
    pub(crate) filesystem: std::collections::BTreeMap<String, pig_protocol::SandboxFsPermission>,
    /// `[sandbox] credential_dirs`: `None` keeps the built-in list, `Some([])`
    /// turns the protection off.
    pub(crate) credential_dirs: Option<Vec<String>>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            deny_read: Vec::new(),
            deny: Vec::new(),
            filesystem: std::collections::BTreeMap::new(),
            credential_dirs: None,
        }
    }
}

impl SandboxSettings {
    pub fn new(deny_read: Vec<String>) -> Self {
        Self {
            deny_read,
            ..Default::default()
        }
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// The structured deny groups (`[[sandbox.deny]]`).
    pub fn with_deny_groups(mut self, groups: Vec<DenyRuleGroup>) -> Self {
        self.deny = groups;
        self
    }

    /// The user-level settings a config file asks for.
    pub fn from_config(config: &pig_protocol::AppConfig) -> Self {
        Self {
            enabled: config.sandbox.enabled,
            deny_read: config.effective_deny_read(),
            deny: config
                .sandbox
                .deny
                .iter()
                .flatten()
                .map(DenyRuleGroup::from)
                .collect(),
            filesystem: config.sandbox.filesystem.clone(),
            credential_dirs: config.sandbox.credential_dirs.clone(),
        }
    }

    /// Apply a workspace's own layer **additively**.
    ///
    /// A project entry may only add denials to the user-level settings. A
    /// whole-root user deny is never replaced by a narrower project scope, and
    /// a user scope's child denials are unioned with the project's. The
    /// structured deny groups are not part of the layer at all (see
    /// `ProjectSandboxLayer`), and `deny_read` is left untouched: project
    /// `deny_read` rules arrive as flattened filesystem denials instead.
    pub(crate) fn with_project(&self, layer: &crate::config::ProjectSandboxLayer) -> Self {
        use pig_protocol::{SandboxFsAccess, SandboxFsPermission};
        let mut merged = self.clone();
        let Some(entries) = &layer.filesystem else {
            return merged;
        };
        for (key, value) in entries {
            match value {
                SandboxFsPermission::Access(SandboxFsAccess::Deny) => {
                    // A deny entry only ever widens protection.
                    merged.filesystem.insert(key.clone(), value.clone());
                }
                SandboxFsPermission::Scoped(children) => {
                    match merged.filesystem.get_mut(key) {
                        // Preserve the user's whole-root denial instead of
                        // replacing it with a narrower project shape.
                        Some(SandboxFsPermission::Access(SandboxFsAccess::Deny)) => {}
                        Some(SandboxFsPermission::Scoped(existing)) => {
                            for (subpath, access) in children {
                                existing.insert(subpath.clone(), *access);
                            }
                        }
                        Some(SandboxFsPermission::Access(_)) => {
                            tracing::warn!(
                                "ignoring the project `[sandbox.filesystem]` scope under \
                                 `{key}`: it cannot be merged into the user-level access entry"
                            );
                        }
                        None => {
                            merged.filesystem.insert(key.clone(), value.clone());
                        }
                    }
                }
                SandboxFsPermission::Access(_) => {
                    // Defensive: config parsing drops project grants, but a
                    // manually constructed layer must not widen access either.
                    tracing::warn!(
                        "ignoring a project filesystem grant for `{key}`: a project may only add \
                         denials"
                    );
                }
            }
        }
        merged
    }
}

/// Install the user-level sandbox configuration (agent startup and after a
/// config save). Platform-neutral: macOS enforces the same rule table through
/// Seatbelt.
pub fn init_sandbox_settings(settings: SandboxSettings) {
    let mut slot = SANDBOX_DENY_SETTINGS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(settings);
}

/// The session's read denials, compiled once and queried per path.
///
/// A **veto view only**: [`DenialView::reason`] says a path is refused;
/// `None` means no rule denies it, never that the kernel permits it. The native
/// ACL set is narrower than the policy text in places (volume roots are granted
/// through the well-known SIDs, the profile root is not granted at all), so the
/// tools keep treating an OS failure as authoritative.
///
/// Built once per tool call: `deny_rules::compile` stats directories for glob
/// prefixes, so recompiling inside a Grep loop over thousands of files would
/// dominate the search.
pub(crate) struct DenialView {
    credential_roots: Vec<PathBuf>,
    literal_denials: Vec<PathBuf>,
    rules: super::sandbox::deny_rules::DenyRules,
    filesystem_denies: Vec<(model::SandboxPath, String)>,
    /// Set when the configured rule table could not be compiled. The shell
    /// launch fails closed on the same error; the tool view must not answer
    /// "no rule denies it" from a table that could not be read.
    config_error: Option<String>,
    /// Directories this call was approved to **list** (names only). Their
    /// entries are still denied for reading; only enumeration is lifted.
    listing_roots: Vec<PathBuf>,
}

impl DenialView {
    /// Compile the session's rules for the tool veto view.
    pub(crate) fn for_state(state: &SessionToolState, cwd: &Path) -> Self {
        let settings = windows_sandbox_settings().with_project(&model::project_layer(state));
        Self::for_settings(&settings, state, cwd)
    }

    /// [`Self::for_state`] for an explicit settings snapshot (unit tests).
    pub(crate) fn for_settings(
        settings: &SandboxSettings,
        state: &SessionToolState,
        cwd: &Path,
    ) -> Self {
        let mut config_error = None;
        let (literal_denials, rules) =
            match super::sandbox::deny_rules::compile(&settings.deny, &settings.deny_read, cwd) {
                Ok(compiled) => (compiled.literal_denials, compiled.rules),
                Err(error) => {
                    config_error = Some(format!(
                        "the sandbox deny rules could not be compiled: {error}"
                    ));
                    (Vec::new(), Default::default())
                }
            };
        let filesystem_denies =
            match super::sandbox::config_entries::compile(&settings.filesystem, cwd) {
                Ok(entries) => entries
                    .into_iter()
                    .filter(|entry| entry.access == model::SandboxAccess::Deny)
                    .map(|entry| {
                        let label = match &entry.path {
                            model::SandboxPath::Path { path } => {
                                format!("`[sandbox.filesystem]` rule for {}", path.display())
                            }
                            model::SandboxPath::GlobPattern { pattern } => {
                                format!("`[sandbox.filesystem]` pattern `{pattern}`")
                            }
                            model::SandboxPath::Special { .. } => {
                                "`[sandbox.filesystem]` rule".to_string()
                            }
                        };
                        (entry.path, label)
                    })
                    .collect(),
                Err(error) => {
                    if config_error.is_none() {
                        config_error = Some(format!(
                            "the sandbox filesystem rules could not be compiled: {error}"
                        ));
                    }
                    Vec::new()
                }
            };
        Self {
            credential_roots: credential_roots(state),
            literal_denials,
            rules,
            filesystem_denies,
            config_error,
            listing_roots: Vec::new(),
        }
    }

    /// Whether the session's policy carries any **explicit** read-deny entry.
    /// This mirrors Codex's `has_denied_read_restrictions`: it inspects the
    /// policy entries (`deny_read`, `[[sandbox.deny]]`, `[sandbox.filesystem]`),
    /// not backend-implicit protections such as profile credential directories.
    /// An approved unsandboxed run drops *all* sandbox restrictions; the dialog
    /// is what makes that visible.
    pub(crate) fn has_denials(&self) -> bool {
        self.config_error.is_some()
            || !self.literal_denials.is_empty()
            || self.rules.has_rules()
            || !self.filesystem_denies.is_empty()
    }

    /// Record the directories an approval allows this call to list.
    pub(crate) fn with_listing_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.listing_roots = roots
            .into_iter()
            .filter(|root| root.is_dir())
            .map(crate::task::sandbox::canonical_root)
            .collect();
        self
    }

    /// Whether `path` is an entry whose **name** this call may see because the
    /// containing directory was approved for listing. Reading its content is
    /// still denied by [`Self::reason`]; this only answers enumeration.
    pub(crate) fn listable_entry(&self, path: &Path) -> bool {
        let parent = match path.parent() {
            Some(parent) => parent,
            None => return false,
        };
        self.listing_roots
            .iter()
            .any(|root| model::within(parent, root) && model::within(path, root))
    }

    /// `Some(reason)` when the sandbox would refuse reading `path`.
    pub(crate) fn reason(&self, path: &Path) -> Option<String> {
        if let Some(error) = &self.config_error {
            // Fail closed: an unreadable rule table cannot prove the path is
            // allowed, and the shell launch refuses for the same reason.
            return Some(format!(
                "The sandbox rules are misconfigured, so no file access is allowed until they \
                 are fixed: {error}"
            ));
        }
        // A Windows alternate data stream (`dir/.env:secret`) is a different
        // object whose path still carries the base file name; every rule shape
        // must see both spellings.
        #[cfg(target_os = "windows")]
        let ads_base = alternate_data_stream_base(path);
        #[cfg(not(target_os = "windows"))]
        let ads_base: Option<PathBuf> = None;
        if self
            .credential_roots
            .iter()
            .any(|root| model::within(path, root))
        {
            return Some(format!(
                "{} is inside a profile credential directory, which the sandbox denies reading. \
                 Do not try to read it another way. If the task really needs this file, ask for \
                 it explicitly: run an ExecCommand call with `sandbox_permissions: \
                 \"with_additional_permissions\"` naming this one file in \
                 `additional_permissions.file_system.write`; the user approves or declines, and \
                 a directory inside these stores is never approved.",
                path.display()
            ));
        }
        if self.literal_denials.iter().any(|denied| {
            model::within(path, denied)
                || ads_base
                    .as_deref()
                    .is_some_and(|base| model::within(base, denied))
        }) {
            return Some(format!(
                "{} is denied by a configured sandbox deny rule",
                path.display()
            ));
        }
        // A stream belongs to its base file: evaluate the rule table on the
        // base so a suffix can neither hide a protected name nor defeat a
        // template exception (`.env.example:readable` stays readable).
        let rule_path = ads_base.as_deref().unwrap_or(path);
        if self.rules.denies_path(rule_path) {
            return Some(format!(
                "{} matches a sandbox deny rule (a built-in sensitive-file group or a configured one)",
                path.display()
            ));
        }
        let candidate = super::sandbox::deny_rules::normalize_candidate(path);
        let ads_candidate = ads_base
            .as_deref()
            .map(super::sandbox::deny_rules::normalize_candidate);
        for (entry, label) in &self.filesystem_denies {
            match entry {
                model::SandboxPath::Path { path: denied } => {
                    if model::within(path, denied)
                        || ads_base
                            .as_deref()
                            .is_some_and(|base| model::within(base, denied))
                    {
                        return Some(format!("{} is denied by {label}", path.display()));
                    }
                }
                model::SandboxPath::GlobPattern { pattern } => {
                    let matcher = globset::GlobBuilder::new(pattern)
                        .case_insensitive(true)
                        .literal_separator(true)
                        .build()
                        .map(|glob| glob.compile_matcher());
                    let matches = matcher
                        .map(|matcher| {
                            matcher.is_match(&candidate)
                                || ads_candidate
                                    .as_deref()
                                    .is_some_and(|base| matcher.is_match(base))
                        })
                        .unwrap_or(false);
                    if matches {
                        return Some(format!("{} is denied by {label}", path.display()));
                    }
                }
                model::SandboxPath::Special { .. } => {}
            }
        }
        None
    }
}

/// `dir/.env:secret` → `dir/.env` on Windows; `None` when there is no stream
/// suffix or the name is not Unicode.
#[cfg(target_os = "windows")]
fn alternate_data_stream_base(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let (base, stream) = name.split_once(':')?;
    if base.is_empty() || stream.is_empty() {
        return None;
    }
    Some(path.with_file_name(base))
}

/// Whether reading `path` is refused by the sandbox rules for one session.
///
/// Convenience wrapper for one-shot checks; loops should build a
/// [`DenialView`] once instead.
pub(crate) fn read_denial(state: &SessionToolState, cwd: &Path, path: &Path) -> Option<String> {
    DenialView::for_state(state, cwd).reason(path)
}

/// Workspace metadata that stays read-only inside a writable root: `.pigcode`,
/// `.git/hooks`/`config`/`info/attributes`, `.agents`, `.aws`, and a worktree
/// pointer's gitdir (see `model::workspace_carve_outs`).
///
/// The sandbox expresses these as read-only entries for the model shell. The
/// file tools do not run under the sandbox, so they consult the same function;
/// that keeps one list instead of two.
pub(crate) fn metadata_carve_outs(roots: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
    model::workspace_carve_outs(roots, cwd)
}

pub(crate) fn windows_sandbox_settings() -> SandboxSettings {
    SANDBOX_DENY_SETTINGS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .unwrap_or_default()
}

#[derive(Clone, Debug)]
pub(crate) struct SandboxCommand {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    /// Trusted parent-to-helper environment overrides (empty on macOS). These
    /// carry the bounded launcher transport, never model text on a command line.
    pub(crate) env: Vec<(OsString, OsString)>,
}

impl SandboxCommand {
    pub(crate) fn new(program: OsString, args: Vec<OsString>) -> Self {
        Self {
            program,
            args,
            env: Vec::new(),
        }
    }
}

/// Turn a model command into the command the platform sandbox runs.
///
/// The base policy comes from the profile (execution mode / plan mode) and the
/// overlay from an approved one-command request; `resolve` materializes both
/// into the concrete grants and denials the backend consumes.
pub(crate) fn prepare_command(
    state: &SessionToolState,
    cwd: &Path,
    command_workdir: Option<&Path>,
    command: SandboxCommand,
    sandbox_override: SandboxOverride,
    profile: SandboxProfile,
    additional: &AdditionalPermissions,
) -> std::io::Result<SandboxCommand> {
    if command.program.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sandbox command program cannot be empty",
        ));
    }
    // Full mode or an explicit user/system `[sandbox] enabled = false` opt-out
    // spawns directly without OS sandbox wrapping.
    if profile == SandboxProfile::Unrestricted || !windows_sandbox_settings().enabled {
        return Ok(command);
    }
    let mut overlay = additional.clone();
    // The macOS retry is an approved widening of *ordinary* filesystem access:
    // the write grant for this one command covers the filesystem root, while the
    // sensitive-file denies and the metadata carve-outs stay in force.
    if sandbox_override == SandboxOverride::EscalatedWithRestrictions {
        overlay.entries.push(SandboxEntry::write_root());
    }
    let policy = SandboxPolicy::for_session_with_workdir(cwd, command_workdir, state, profile)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;

    #[cfg(target_os = "macos")]
    {
        let resolved = resolve(&policy, &overlay, cwd).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
        })?;
        macos::prepare_command(command, &resolved)
    }
    #[cfg(target_os = "linux")]
    {
        let resolved = resolve(&policy, &overlay, cwd).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
        })?;
        linux::prepare_command(command, &resolved, cwd)
    }
    #[cfg(target_os = "windows")]
    {
        // `EscalatedWithRestrictions` is Codex's approved command profile: the
        // overlay carries a root write entry, while the deny rules and the
        // protected metadata carve-outs stay in force. The helper resolves and
        // materializes both, the same split Codex uses; the parent only
        // validates what it can.
        policy.validate().map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
        })?;
        if !windows::sandbox_readiness().usable() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "command cannot be sandboxed: Windows process security environment (PSEC) is \
                 unavailable on this host. Set `[sandbox] enabled = false` in user config to \
                 explicitly run without an OS sandbox.",
            ));
        }
        windows::prepare_command(&policy, &overlay, cwd, command)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (&policy, &overlay);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "command cannot be sandboxed: this platform has no supported OS sandbox backend. \
             Set `[sandbox] enabled = false` in user config to explicitly run without an OS sandbox.",
        ))
    }
}

/// Roots the sandbox may read.
///
/// Full disk, following Codex: `workspace-write` starts from a `:root` read
/// grant, so the model can inspect anything the user can. Writes remain the
/// boundary. On Windows the "root" is every mounted volume, because a single
/// drive letter is not a filesystem root there.
pub(crate) fn read_roots(cwd: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        super::sandbox::windows::volume_roots_for(cwd)
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![filesystem_root(cwd)]
    }
}

/// Used by the POSIX read root; Windows materializes `:root` through
/// `volume_roots_for` instead.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) fn filesystem_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .last()
        .unwrap_or_else(|| Path::new("/"))
        .to_path_buf()
}

/// Canonicalize when possible. On Windows this also strips the `\\?\` verbatim
/// prefix so policy roots, native grants and shell paths share one spelling.
pub(crate) fn canonical_root(root: PathBuf) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        dunce::canonicalize(&root).unwrap_or(root)
    }
    #[cfg(not(target_os = "windows"))]
    {
        root.canonicalize().unwrap_or(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(read_roots: Vec<PathBuf>, rw_roots: Vec<PathBuf>) -> SessionToolState {
        let (tx, _rx) = async_channel::bounded(1024);
        SessionToolState::new("sandbox-policy-test".to_string(), tx, read_roots, rw_roots)
    }

    /// Scratch space under the crate's target directory. Test binaries must not
    /// depend on `%TEMP%` being writable: sandboxed CI/dev shells may hand them
    /// a read-only temp directory.
    fn scratch_root() -> PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join("test-tmp");
        std::fs::create_dir_all(&root).expect("create test scratch root");
        root
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = scratch_root().join(format!("pig-sandbox-policy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create policy test directory");
        canonical_root(dir)
    }

    fn resolved(cwd: &Path, state: &SessionToolState, profile: SandboxProfile) -> ResolvedPolicy {
        let policy = SandboxPolicy::for_session(cwd, state, profile).expect("policy");
        resolve(&policy, &AdditionalPermissions::default(), cwd).expect("resolve")
    }

    /// Codex's `volume_roots(policy_cwd, command_cwd)`: a workdir on another
    /// UNC share or mapped remote drive must be readable even though the policy
    /// root stays the workspace (local drives are already covered by `:root`).
    #[cfg(target_os = "windows")]
    #[test]
    fn command_workdir_volume_joins_the_read_set() {
        let cwd = temp_dir("workdir-volume");
        let state = state(Vec::new(), Vec::new());
        for workdir in [
            Path::new(r"\\server\other\project"),
            Path::new(r"Z:\projects\app"),
        ] {
            let root = super::windows::volume_root_for(workdir).expect("volume root");
            let policy = SandboxPolicy::for_session_with_workdir(
                &cwd,
                Some(workdir),
                &state,
                SandboxProfile::WorkspaceWrite,
            )
            .expect("policy");
            let resolved =
                resolve(&policy, &AdditionalPermissions::default(), &cwd).expect("resolve");
            assert!(
                resolved.read.contains(&root),
                "workdir volume {} must be readable: {:?}",
                root.display(),
                resolved.read
            );
        }
    }

    #[test]
    fn policy_preserves_workspace_tmp_and_extra_roots() {
        let cwd = temp_dir("workspace");
        let extra = cwd.join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        let state = state(vec![cwd.join("read-only-extra")], vec![extra.clone()]);

        let policy = resolved(&cwd, &state, SandboxProfile::WorkspaceWrite);

        assert!(policy.write.contains(&cwd));
        assert!(policy.write.contains(&extra));
        assert!(policy.write.contains(&canonical_root(std::env::temp_dir())));
        // Extra read roots stay read-only and are not folded into the writes.
        assert!(
            policy
                .read
                .iter()
                .any(|root| *root == canonical_root(cwd.join("read-only-extra")))
        );
        assert_eq!(policy.kind, SandboxKind::Restricted);
        assert!(policy.deny_sensitive_files);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn policy_does_not_use_the_posix_tmp_literal_on_windows() {
        let cwd = temp_dir("posix-tmp");
        let policy = resolved(
            &cwd,
            &state(Vec::new(), Vec::new()),
            SandboxProfile::WorkspaceWrite,
        );

        assert!(
            policy.write.iter().all(|root| root != Path::new("/tmp")),
            "the POSIX literal is not an absolute Windows path"
        );
    }

    #[test]
    fn policy_grants_full_disk_read_and_keeps_writes_in_the_workspace() {
        let cwd = temp_dir("read-everywhere");
        let state = state(Vec::new(), Vec::new());

        let policy = resolved(&cwd, &state, SandboxProfile::WorkspaceWrite);

        // Codex parity: `workspace-write` starts from a `:root` read grant,
        // materialized per host.
        assert!(
            !policy.read.is_empty(),
            "the policy must carry a full-disk read root"
        );
        assert!(
            !policy
                .write
                .contains(&canonical_root(filesystem_root(&cwd))),
            "writes stay out of the filesystem root"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_read_policy_covers_every_volume_but_writes_do_not() {
        let cwd = temp_dir("flags-windows");
        let policy = resolved(
            &cwd,
            &state(Vec::new(), Vec::new()),
            SandboxProfile::WorkspaceWrite,
        );

        // Reads: every mounted volume (Codex's full-disk read on a platform
        // without a single root), plus the shell's own install root.
        assert!(!policy.read.is_empty());
        for volume in read_roots(&cwd) {
            assert!(
                policy.read.contains(&volume),
                "volume root {} must be readable: {:?}",
                volume.display(),
                policy.read
            );
        }
        // Writes: the workspace, tmp and the extra roots only.
        assert!(
            policy
                .write
                .iter()
                .all(|root| !super::windows::is_volume_root(root)),
            "writes never reach a volume root: {:?}",
            policy.write
        );
    }

    #[test]
    fn policy_deduplicates_canonical_roots() {
        let cwd = temp_dir("dedup");
        let only_read = cwd.join("data");
        std::fs::create_dir_all(&only_read).unwrap();
        // The same root twice, and a write root that is also a session root.
        let state = state(
            vec![only_read.clone(), only_read.clone()],
            vec![cwd.clone(), cwd.clone()],
        );

        let policy = resolved(&cwd, &state, SandboxProfile::WorkspaceWrite);

        assert_eq!(
            policy
                .read
                .iter()
                .filter(|root| **root == only_read)
                .count(),
            1
        );
        assert_eq!(policy.write.iter().filter(|root| **root == cwd).count(), 1);
        // Write beats read for the same path: the workspace is not also listed
        // as readable.
        assert!(!policy.read.contains(&cwd));
    }

    #[test]
    fn prepare_rejects_empty_program() {
        let cwd = temp_dir("empty-program");
        let state = state(Vec::new(), Vec::new());

        let error = prepare_command(
            &state,
            &cwd,
            None,
            SandboxCommand::new(OsString::new(), Vec::new()),
            SandboxOverride::NoOverride,
            SandboxProfile::WorkspaceWrite,
            &AdditionalPermissions::default(),
        )
        .expect_err("empty program must fail");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn unrestricted_profile_skips_sandbox_preparation() {
        let cwd = temp_dir("unrestricted");
        let state = state(Vec::new(), Vec::new());
        let command = SandboxCommand::new("cmd.exe".into(), vec!["/C".into(), "echo hi".into()]);
        let prepared = prepare_command(
            &state,
            &cwd,
            None,
            command,
            SandboxOverride::NoOverride,
            SandboxProfile::Unrestricted,
            &AdditionalPermissions::default(),
        )
        .expect("unrestricted preparation");

        // No helper, no policy materialization: the command is spawned as-is.
        assert_eq!(prepared.program, std::ffi::OsString::from("cmd.exe"));
        assert!(prepared.env.is_empty());
    }

    #[test]
    fn explicit_sandbox_disabled_skips_os_sandbox_and_enabled_fails_closed_when_unsupported() {
        let _guard = sandbox_settings_test_lock();
        let cwd = temp_dir("disabled-toggle");
        let state = state(Vec::new(), Vec::new());
        let command = SandboxCommand::new("echo".into(), vec!["hi".into()]);

        init_sandbox_settings(SandboxSettings::default().with_enabled(false));
        let prepared = prepare_command(
            &state,
            &cwd,
            None,
            command.clone(),
            SandboxOverride::NoOverride,
            SandboxProfile::WorkspaceWrite,
            &AdditionalPermissions::default(),
        )
        .expect("explicitly disabled sandbox must bypass OS wrapper");
        assert_eq!(prepared.program, std::ffi::OsString::from("echo"));
        assert!(prepared.env.is_empty());
        assert!(!platform_sandbox_status().enabled);

        init_sandbox_settings(SandboxSettings::default().with_enabled(true));
        let status = platform_sandbox_status();
        assert!(status.enabled);
        if !status.supported {
            let err = prepare_command(
                &state,
                &cwd,
                None,
                command,
                SandboxOverride::NoOverride,
                SandboxProfile::WorkspaceWrite,
                &AdditionalPermissions::default(),
            )
            .expect_err("enabled sandbox on unsupported host must fail closed");
            assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
            assert!(err.to_string().contains("cannot be sandboxed"), "{err}");
        }
    }

    /// An approved request is an overlay on the read-only base: exactly the
    /// approved roots become writable, the workspace does not.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn read_only_plus_an_overlay_writes_only_the_approved_root() {
        let cwd = temp_dir("read-only-writes");
        let approved = cwd.join("approved");
        std::fs::create_dir_all(&approved).unwrap();
        let state = state(Vec::new(), Vec::new());

        let policy =
            SandboxPolicy::for_session(&cwd, &state, SandboxProfile::ReadOnly).expect("policy");
        let overlay = AdditionalPermissions::write_roots(vec![approved.clone()]);
        let resolved = resolve(&policy, &overlay, &cwd).expect("resolve");

        assert_eq!(
            resolved.write,
            vec![canonical_root(approved)],
            "the writable set is exactly the approved roots"
        );
        assert!(
            !resolved.write.contains(&cwd),
            "the workspace itself stays read-only"
        );
        // The deny scan still covers the session write set.
        assert!(
            resolved
                .deny_scan_roots
                .iter()
                .any(|root| *root == canonical_root(cwd.clone()))
        );
    }

    /// An approved escalation widens *ordinary* filesystem access with a
    /// `:root` write entry; the base profile is unchanged.
    #[test]
    fn escalated_overlay_widens_writes_to_the_filesystem_root() {
        let cwd = temp_dir("escalated");
        let state = state(Vec::new(), Vec::new());
        let policy = SandboxPolicy::for_session(&cwd, &state, SandboxProfile::WorkspaceWrite)
            .expect("policy");
        let overlay = AdditionalPermissions {
            entries: vec![SandboxEntry::write_root()],
        };

        let resolved = resolve(&policy, &overlay, &cwd).expect("resolve");

        // The write grant at the filesystem root subsumes the base `:root`
        // read entry (write over read precedence), so read coverage is the
        // backend's job — Seatbelt treats writable roots as readable.
        #[cfg(not(target_os = "windows"))]
        assert!(
            resolved.write.contains(&filesystem_root(&cwd)),
            "the escalation widens writes to the filesystem root: {:?}",
            resolved.write
        );
        assert!(resolved.deny_sensitive_files);
    }

    /// `[sandbox] deny_read` / `[[sandbox.deny]]` reach
    /// every policy built after the setting is installed. The slot is
    /// process-wide, so this test restores it and only uses patterns that cannot
    /// match another test's fixture.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_sandbox_settings_reach_the_policy() {
        let _lock = sandbox_settings_test_lock();
        init_sandbox_settings(
            SandboxSettings::new(vec!["zz-test-only-denials/**".to_string()]).with_deny_groups(
                vec![DenyRuleGroup {
                    globs: vec!["**/.zz-test-only-secret".to_string()],
                    exceptions: Vec::new(),
                    write_exceptions: Vec::new(),
                }],
            ),
        );
        let state = SessionToolState::for_test();
        let policy = SandboxPolicy::for_session(
            std::path::Path::new("C:/"),
            &state,
            SandboxProfile::WorkspaceWrite,
        )
        .expect("policy");
        assert_eq!(
            policy.deny_groups.len(),
            2,
            "the configured group and the simple rule are both compiled"
        );
        assert!(
            policy
                .deny_groups
                .iter()
                .any(|group| group.globs == vec!["**/.zz-test-only-secret".to_string()]),
            "the structured group reaches the policy: {:?}",
            policy.deny_groups
        );
        init_sandbox_settings(SandboxSettings::new(Vec::new()));
        let policy = SandboxPolicy::for_session(
            std::path::Path::new("C:/"),
            &state,
            SandboxProfile::WorkspaceWrite,
        )
        .expect("policy");
        assert!(
            policy.deny_groups.is_empty(),
            "nothing configured, no rules"
        );
    }

    /// The sandbox settings slot and `USERPROFILE`/`HOME` are process-wide;
    /// these tests mutate one or both, so they serialize.
    fn sandbox_settings_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The configured credential list reaches `credential_roots`, and an
    /// explicit empty list really disables the protection.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_credential_list_reaches_the_policy() {
        let _env = sandbox_settings_test_lock();
        let profile =
            std::env::temp_dir().join(format!("pig-credential-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(profile.join(".ssh")).expect("create fixture");
        std::fs::create_dir_all(profile.join(".config")).expect("create fixture");
        // SAFETY: the sandbox tests serialize on their own mutex; this unit test
        // is the only writer of the variable here.
        let previous = std::env::var_os("USERPROFILE");
        unsafe { std::env::set_var("USERPROFILE", &profile) };
        let state = SessionToolState::for_test();

        init_sandbox_settings(SandboxSettings::new(Vec::new()));
        let defaults = credential_roots(&state);
        let defaults_ok = defaults.iter().any(|root| root.ends_with(".ssh"))
            && defaults.iter().any(|root| root.ends_with(".config"));

        let mut settings = SandboxSettings::new(Vec::new());
        settings.credential_dirs = Some(vec![".ssh".to_string()]);
        init_sandbox_settings(settings);
        let narrowed = credential_roots(&state);
        let narrowed_ok = narrowed.len() == 1 && narrowed[0].ends_with(".ssh");

        let mut settings = SandboxSettings::new(Vec::new());
        settings.credential_dirs = Some(Vec::new());
        init_sandbox_settings(settings);
        let off = credential_roots(&state);

        init_sandbox_settings(SandboxSettings::new(Vec::new()));
        if let Some(previous) = previous {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        } else {
            unsafe { std::env::remove_var("USERPROFILE") };
        }
        let _ = std::fs::remove_dir_all(&profile);

        assert!(defaults_ok, "the built-in list applies when unset");
        assert!(narrowed_ok, "a list replaces the defaults");
        assert!(off.is_empty(), "an explicit empty list turns it off");
    }

    /// Regression: on macOS/Linux there is no `USERPROFILE`, so the profile
    /// comes from `HOME` — before the fallback, `~/.ssh` classified as an
    /// ordinary path and credential protection silently did nothing.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn the_profile_falls_back_to_home() {
        let _env = sandbox_settings_test_lock();
        let profile =
            std::env::temp_dir().join(format!("pig-credential-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(profile.join(".ssh")).expect("create fixture");
        // SAFETY: serialized on the credential lock, like the other writers.
        let previous_userprofile = std::env::var_os("USERPROFILE");
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("USERPROFILE");
            std::env::set_var("HOME", &profile);
        }
        let state = SessionToolState::for_test();

        init_sandbox_settings(SandboxSettings::new(Vec::new()));
        let roots = credential_roots(&state);
        let grant = crate::task::credential_grant(&state, &profile.join(".ssh"));

        init_sandbox_settings(SandboxSettings::new(Vec::new()));
        if let Some(previous) = previous_userprofile {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        }
        if let Some(previous) = previous_home {
            unsafe { std::env::set_var("HOME", previous) };
        }
        let _ = std::fs::remove_dir_all(&profile);

        assert!(
            roots.iter().any(|root| root.ends_with(".ssh")),
            "HOME supplies the profile: {roots:?}"
        );
        assert!(
            matches!(grant, crate::task::CredentialGrant::Directory),
            "`.ssh` under HOME is a credential directory"
        );
    }

    /// A malformed configured list keeps the built-in protection instead of
    /// silently protecting nothing.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_malformed_credential_list_falls_back_to_the_defaults() {
        let _env = sandbox_settings_test_lock();
        let profile =
            std::env::temp_dir().join(format!("pig-credential-malformed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(profile.join(".ssh")).expect("create fixture");
        // SAFETY: see `the_credential_list_reaches_the_policy`.
        let previous = std::env::var_os("USERPROFILE");
        unsafe { std::env::set_var("USERPROFILE", &profile) };
        let state = SessionToolState::for_test();

        let mut settings = SandboxSettings::new(Vec::new());
        settings.credential_dirs = Some(vec!["../escape".to_string()]);
        init_sandbox_settings(settings);
        let roots = credential_roots(&state);

        init_sandbox_settings(SandboxSettings::new(Vec::new()));
        if let Some(previous) = previous {
            unsafe { std::env::set_var("USERPROFILE", previous) };
        } else {
            unsafe { std::env::remove_var("USERPROFILE") };
        }
        let _ = std::fs::remove_dir_all(&profile);

        assert!(
            roots.iter().any(|root| root.ends_with(".ssh")),
            "the default list stays in force: {roots:?}"
        );
    }

    /// A workspace's own layer only adds denials: the user-level `deny_read`
    /// list and structured groups stay exactly as configured, while a project
    /// filesystem denial joins the table.
    #[test]
    fn the_project_layer_only_adds_denials() {
        let user = SandboxSettings::new(vec!["user-rule/**".to_string()]).with_deny_groups(vec![
            DenyRuleGroup {
                globs: vec!["**/.env".to_string()],
                exceptions: Vec::new(),
                write_exceptions: Vec::new(),
            },
        ]);
        let layer = crate::config::ProjectSandboxLayer {
            filesystem: Some(std::collections::BTreeMap::from([(
                "/data/vault".to_string(),
                pig_protocol::SandboxFsPermission::Access(pig_protocol::SandboxFsAccess::Deny),
            )])),
            network_block: Some(true),
        };
        let merged = user.with_project(&layer);
        assert_eq!(merged.deny_read, user.deny_read);
        assert!(merged.filesystem.contains_key("/data/vault"));
        // The seeded groups are the user's own configuration: a project layer
        // has no say over them.
        assert_eq!(merged.deny, user.deny);

        // A layer that mentions only one key leaves the others alone.
        let partial = user.with_project(&crate::config::ProjectSandboxLayer {
            network_block: Some(false),
            ..Default::default()
        });
        assert_eq!(partial.deny_read, user.deny_read);
        assert!(partial.filesystem.is_empty());
    }

    /// The exact regression: a user whole-root deny must survive a project
    /// scoped denial under the same root, and a user scope's child denials must
    /// be unioned with the project's rather than replaced.
    #[test]
    fn a_project_scope_cannot_narrow_a_user_whole_root_deny() {
        use pig_protocol::{SandboxFsAccess, SandboxFsPermission};
        use std::collections::BTreeMap;

        let mut settings = SandboxSettings::default();
        settings.filesystem.insert(
            "/data/repo".to_string(),
            SandboxFsPermission::Access(SandboxFsAccess::Deny),
        );
        settings.filesystem.insert(
            "/data/scoped".to_string(),
            SandboxFsPermission::Scoped(BTreeMap::from([(
                "user-child".to_string(),
                SandboxFsAccess::Deny,
            )])),
        );
        let layer = crate::config::ProjectSandboxLayer {
            filesystem: Some(BTreeMap::from([
                (
                    "/data/repo".to_string(),
                    SandboxFsPermission::Scoped(BTreeMap::from([(
                        "project-child".to_string(),
                        SandboxFsAccess::Deny,
                    )])),
                ),
                (
                    "/data/scoped".to_string(),
                    SandboxFsPermission::Scoped(BTreeMap::from([(
                        "project-child".to_string(),
                        SandboxFsAccess::Deny,
                    )])),
                ),
            ])),
            network_block: None,
        };
        let merged = settings.with_project(&layer);
        assert_eq!(
            merged.filesystem.get("/data/repo"),
            Some(&SandboxFsPermission::Access(SandboxFsAccess::Deny)),
            "the user's whole-root deny is preserved, not narrowed to a child scope"
        );
        let Some(SandboxFsPermission::Scoped(children)) = merged.filesystem.get("/data/scoped")
        else {
            panic!("the user scope is preserved: {:?}", merged.filesystem);
        };
        assert_eq!(children.get("user-child"), Some(&SandboxFsAccess::Deny));
        assert_eq!(
            children.get("project-child"),
            Some(&SandboxFsAccess::Deny),
            "the project's scoped child denial is unioned"
        );
    }

    /// `[windows] sandbox_network` is the fallback, and the environment variable
    /// (including an explicit `allow`) overrides it.
    #[test]
    fn network_policy_precedence() {
        assert_eq!(NetworkPolicy::resolve(None, false), NetworkPolicy::Allow);
        assert_eq!(NetworkPolicy::resolve(None, true), NetworkPolicy::Block);
        assert_eq!(
            NetworkPolicy::resolve(Some("block"), false),
            NetworkPolicy::Block
        );
        assert_eq!(
            NetworkPolicy::resolve(Some("BLOCK"), true),
            NetworkPolicy::Block
        );
        assert_eq!(
            NetworkPolicy::resolve(Some("allow"), true),
            NetworkPolicy::Allow
        );
        assert_eq!(
            NetworkPolicy::resolve(Some("something-else"), true),
            NetworkPolicy::Allow
        );
    }

    /// The read-only base has no write entry and still validates: that
    /// combination is the whole point. "No write entry" must never be read as
    /// "allow all" by a backend.
    #[test]
    fn read_only_profile_has_no_write_entry_and_still_validates() {
        let cwd = temp_dir("read-only");
        let state = state(Vec::new(), Vec::new());

        let read_only = resolved(&cwd, &state, SandboxProfile::ReadOnly);
        assert!(read_only.write.is_empty());
        assert!(!read_only.read.is_empty());
        assert_eq!(read_only.kind, SandboxKind::Restricted);

        let policy =
            SandboxPolicy::for_session(&cwd, &state, SandboxProfile::ReadOnly).expect("policy");
        policy.validate().expect("read-only policy must validate");

        // The normal profile is unaffected.
        let workspace_write = resolved(&cwd, &state, SandboxProfile::WorkspaceWrite);
        assert!(!workspace_write.write.is_empty());
    }

    /// Profile switching must not change the deny set.
    ///
    /// A profile-dependent list would mean a different (and differently
    /// expensive) sensitive-file walk for the same session whenever it enters
    /// or leaves plan mode, so `deny_scan_roots` is computed from the session's
    /// normal write set in both profiles.
    #[test]
    fn both_profiles_scan_the_same_roots() {
        let cwd = temp_dir("profiles");
        let extra = cwd.join("extra-rw");
        std::fs::create_dir_all(&extra).unwrap();
        let state = state(Vec::new(), vec![extra.clone()]);

        let workspace_write =
            SandboxPolicy::for_session(&cwd, &state, SandboxProfile::WorkspaceWrite)
                .expect("policy");
        let read_only =
            SandboxPolicy::for_session(&cwd, &state, SandboxProfile::ReadOnly).expect("policy");

        assert_eq!(
            workspace_write.deny_scan_roots, read_only.deny_scan_roots,
            "the deny scan set must be profile-independent"
        );
        assert!(workspace_write.deny_scan_roots.contains(&cwd));
        assert!(read_only.deny_scan_roots.contains(&cwd));
        // Only the *grants* differ.
        assert!(
            workspace_write
                .entries
                .iter()
                .any(|entry| entry.access == SandboxAccess::Write)
        );
        assert!(
            !read_only
                .entries
                .iter()
                .any(|entry| entry.access == SandboxAccess::Write)
        );
    }

    /// A restricted policy with no entry at all is a configuration mistake: it
    /// would ask every backend to enforce nothing.
    #[test]
    fn an_empty_policy_is_rejected() {
        let cwd = temp_dir("empty-roots");
        let state = state(Vec::new(), Vec::new());
        let mut policy = SandboxPolicy::for_session(&cwd, &state, SandboxProfile::WorkspaceWrite)
            .expect("policy");
        policy.entries.clear();

        let error = policy.validate().expect_err("must reject an empty policy");
        assert!(error.to_string().contains("at least one entry"), "{error}");
    }

    /// A rule table that cannot be compiled must fail the **tool** veto view
    /// closed: dropping every configured denial would let `Read` bypass rules
    /// the shell launcher refuses to run with. An unaffected path is allowed
    /// again once the table compiles.
    #[test]
    fn a_malformed_rule_table_denies_instead_of_dropping_the_veto() {
        let cwd = temp_dir("malformed-rules");
        let state = state(Vec::new(), Vec::new());
        let mut settings = SandboxSettings::new(Vec::new());
        settings.filesystem.insert(
            ":minimal".to_string(),
            pig_protocol::SandboxFsPermission::Access(pig_protocol::SandboxFsAccess::Deny),
        );
        let view = DenialView::for_settings(&settings, &state, &cwd);
        let reason = view
            .reason(&cwd.join("notes.txt"))
            .expect("a broken rule table must fail closed");
        assert!(reason.contains("misconfigured"), "{reason}");
        assert!(
            view.has_denials(),
            "an unsandboxed escalation must still see restrictions"
        );

        let valid = SandboxSettings::new(Vec::new());
        assert!(
            DenialView::for_settings(&valid, &state, &cwd)
                .reason(&cwd.join("notes.txt"))
                .is_none(),
            "a compilable table keeps answering per path"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
