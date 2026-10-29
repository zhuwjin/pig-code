//! The sandbox policy model, in Codex's vocabulary.
//!
//! A policy is a **kind** plus a list of **entries**; grants and denials live in
//! the same list, and precedence is resolved once here (`Deny` beats `Write`
//! beats `Read`) instead of being re-implemented by every backend. That is
//! Codex's `FileSystemSandboxPolicy { kind, entries }`, and the per-command
//! escalation is its `AdditionalPermissionProfile`: an overlay of the *same*
//! entry type, so "read-only plus one approved directory" is one policy with an
//! extra `Write` entry rather than a fourth profile variant.
//!
//! Resolution runs in three steps, named after Codex's `policy_transforms`:
//!
//! 1. **materialize** — expand `Special::Root` into this host's filesystem
//!    roots (every mounted volume on Windows, `/` elsewhere), and reject what a
//!    backend cannot express instead of silently dropping it;
//! 2. **merge** — append the overlay's entries to the base policy's;
//! 3. **effective** — apply the access precedence and split the result into the
//!    concrete lists a backend consumes.
//!
//! Two things stay outside the aligned shape on purpose, because they describe
//! *how* the deny list is computed rather than what it contains:
//! `deny_scan_roots` (the session write set the sensitive-file snapshot walks)
//! and the built-in deny groups (the switchable `env`/`ssh`/`cloud_credentials`
//! tables), whose enforcement differs per
//! backend — access-time regexes on Seatbelt, a launch-time walk on Windows.
//! `docs/WINDOWS_SANDBOX.md` records what is deliberately deferred.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::NetworkPolicy;
use super::SessionToolState;
use super::canonical_root;
use super::read_roots;
use super::windows_sandbox_settings;

/// What a policy does to the filesystem. Codex's `FileSystemSandboxKind`, with
/// `ExternalSandbox` deferred until something enforces the policy outside this
/// process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// This process owns construction: the entries are the whole policy.
    #[default]
    Restricted,
    /// No filesystem policy at all. Only `Full` mode uses it.
    Unrestricted,
}

/// Codex's `FileSystemAccessMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxAccess {
    Read,
    Write,
    Deny,
}

/// Codex's `FileSystemSpecialPath`, reduced to what we materialize today.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxSpecialPath {
    /// The filesystem root: `:root` in Codex, the full-disk read grant here.
    Root,
}

/// Codex's `FileSystemPath`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxPath {
    Path {
        path: PathBuf,
    },
    Special {
        value: SandboxSpecialPath,
    },
    /// A git-style glob. Deny-only, exactly as in Codex (`GlobPattern` entries
    /// "currently support FileSystemAccessMode::Deny only").
    GlobPattern {
        pattern: String,
    },
}

/// Codex's `FileSystemSandboxEntryMissingPathBehavior`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingPathBehavior {
    /// Ignore an entry whose path does not exist yet.
    Skip,
}

/// Codex's `FileSystemSandboxEntry`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxEntry {
    pub path: SandboxPath,
    pub access: SandboxAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_path_behavior: Option<MissingPathBehavior>,
}

impl SandboxEntry {
    pub(crate) fn new(path: SandboxPath, access: SandboxAccess) -> Self {
        Self {
            path,
            access,
            missing_path_behavior: None,
        }
    }

    pub(crate) fn read(path: PathBuf) -> Self {
        Self::new(
            SandboxPath::Path {
                path: canonical_root(path),
            },
            SandboxAccess::Read,
        )
    }

    pub(crate) fn write(path: PathBuf) -> Self {
        Self::new(
            SandboxPath::Path {
                path: canonical_root(path),
            },
            SandboxAccess::Write,
        )
    }

    pub(crate) fn deny(path: PathBuf) -> Self {
        Self::new(
            SandboxPath::Path {
                path: canonical_root(path),
            },
            SandboxAccess::Deny,
        )
    }

    #[allow(dead_code)]
    pub(crate) fn deny_glob(pattern: String) -> Self {
        Self::new(SandboxPath::GlobPattern { pattern }, SandboxAccess::Deny)
    }

    pub(crate) fn read_root() -> Self {
        Self::new(
            SandboxPath::Special {
                value: SandboxSpecialPath::Root,
            },
            SandboxAccess::Read,
        )
    }

    pub(crate) fn write_root() -> Self {
        Self::new(
            SandboxPath::Special {
                value: SandboxSpecialPath::Root,
            },
            SandboxAccess::Write,
        )
    }
}

/// Codex's `AdditionalPermissionProfile`: the per-command overlay of extra
/// permissions, in the same entry vocabulary as the base policy. A later
/// session- or turn-scoped grant is the same type held somewhere else, not a
/// new mechanism.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdditionalPermissions {
    pub entries: Vec<SandboxEntry>,
}

impl AdditionalPermissions {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The overlay an approved `with_additional_permissions` request produces:
    /// write access to exactly the directories the user approved.
    pub fn write_roots(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            entries: roots.into_iter().map(SandboxEntry::write).collect(),
        }
    }

    /// An overlay granting read access to exactly these paths (an approved
    /// listing of a credential directory, or one file inside it).
    pub fn read_roots(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            entries: roots.into_iter().map(SandboxEntry::read).collect(),
        }
    }

    /// The write roots this overlay grants, in entry order (deduplicated).
    pub fn write_roots_list(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for entry in &self.entries {
            if entry.access != SandboxAccess::Write {
                continue;
            }
            if let SandboxPath::Path { path } = &entry.path
                && !roots.contains(path)
            {
                roots.push(path.clone());
            }
        }
        roots
    }

    /// Add write roots to this overlay (the roots an approved widening grants
    /// for this one command). Existing roots are kept once.
    pub(crate) fn extend_write_roots(&mut self, roots: impl IntoIterator<Item = PathBuf>) {
        let mut existing = self.write_roots_list();
        for root in roots {
            let root = canonical_root(root);
            if existing.contains(&root) {
                continue;
            }
            existing.push(root.clone());
            self.entries.push(SandboxEntry::write(root));
        }
    }

    /// Add read roots for one command (an approved read of a credential file).
    ///
    /// A read entry lifts a credential denial without making the file writable:
    /// the write set is unchanged, so the OS still refuses writes there.
    pub(crate) fn extend_read_roots(&mut self, roots: impl IntoIterator<Item = PathBuf>) {
        let mut existing = self.read_roots_list();
        for root in roots {
            let root = canonical_root(root);
            if existing.contains(&root) {
                continue;
            }
            existing.push(root.clone());
            self.entries.push(SandboxEntry::read(root));
        }
    }

    /// The read roots this overlay grants, in entry order (deduplicated).
    pub(crate) fn read_roots_list(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for entry in &self.entries {
            if entry.access != SandboxAccess::Read {
                continue;
            }
            if let SandboxPath::Path { path } = &entry.path
                && !roots.contains(path)
            {
                roots.push(path.clone());
            }
        }
        roots
    }
}

/// Which base policy a command starts from.
///
/// This is the caller-facing selector (execution mode / plan mode), the role
/// Codex's `PermissionProfile` plays. It is deliberately *not* the policy: the
/// policy is built from it, and the approved-write state is an overlay rather
/// than another variant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxProfile {
    /// Full-disk read, writes confined to the session's writable roots.
    #[default]
    WorkspaceWrite,
    /// Full-disk read, no writes at all. Used where the work is research only
    /// (plan mode): the reason a policy exists at all is that a tool-layer
    /// allowlist cannot cover every write path, while an empty write set is
    /// enforced by the kernel.
    ReadOnly,
    /// No OS sandbox at all. Only the `Full` execution mode uses it: the
    /// session layer rejects dangerous commands before spawning, and file tools
    /// still refuse sensitive names, but the shell itself is unrestricted.
    Unrestricted,
}

/// The base policy: entries plus the inputs the deny list is computed from.
#[derive(Clone, Debug)]
pub(crate) struct SandboxPolicy {
    pub(crate) kind: SandboxKind,
    pub(crate) entries: Vec<SandboxEntry>,
    /// Roots the sensitive-file walk covers. The session's normal write set
    /// **regardless of profile**, so entering or leaving plan mode does not
    /// change which files are protected (or re-walk the workspace).
    pub(crate) deny_scan_roots: Vec<PathBuf>,
    pub(crate) network: NetworkPolicy,
    pub(crate) deny_sensitive_files: bool,
    /// The compiled read-deny rules, as scoped groups: each group's globs deny,
    /// and its own exceptions lift only that group. Built from the shared,
    /// platform-neutral rule table plus configuration, then enforced by both
    /// backends (access-time regexes on Seatbelt, a launch-time walk on
    /// Windows).
    pub(crate) deny_groups: Vec<super::deny_rules::DenyRuleGroup>,
    /// `[sandbox] credential_dirs`: `None` keeps the built-in profile list.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) credential_dirs: Option<Vec<String>>,
}

impl SandboxPolicy {
    /// The base policy for a session command (test convenience; product paths
    /// call [`Self::for_session_with_workdir`] with the command's workdir).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn for_session(
        cwd: &Path,
        state: &SessionToolState,
        profile: SandboxProfile,
    ) -> Result<Self, String> {
        Self::for_session_with_workdir(cwd, None, state, profile)
    }

    /// Codex's `volume_roots(policy_cwd, command_cwd)`: a per-command `workdir`
    /// on another volume stays readable even though the policy root (and every
    /// write grant) remains the session workspace. Read-only, so the workdir
    /// can never widen writes; denies still win in `resolve`.
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    pub(crate) fn for_session_with_workdir(
        cwd: &Path,
        command_workdir: Option<&Path>,
        state: &SessionToolState,
        profile: SandboxProfile,
    ) -> Result<Self, String> {
        let mut entries = vec![SandboxEntry::read_root()];
        for root in &state.extra_read_roots {
            entries.push(SandboxEntry::read(root.clone()));
        }
        #[cfg(target_os = "windows")]
        if let Some(root) = command_workdir.and_then(super::windows::volume_root_for) {
            // The command's own volume is named explicitly: drive-letter
            // enumeration covers local drives, but a workdir on a UNC share or
            // another mapped remote drive would otherwise be missing.
            entries.push(SandboxEntry::read(root));
        }
        let session_roots = session_write_roots(cwd, state);
        // The Windows model shell lives outside the workspace and needs its own
        // install root readable when it is a Git installation.
        #[cfg(target_os = "windows")]
        if let Some(root) = super::windows::shell_read_root() {
            entries.push(SandboxEntry::read(root));
        }
        if profile != SandboxProfile::ReadOnly {
            for root in &session_roots {
                entries.push(SandboxEntry::write(root.clone()));
            }
        }

        let settings = windows_sandbox_settings().with_project(&project_layer(state));
        // Configured rules fail closed here: a rule that cannot be compiled
        // must stop the command before a backend is involved.
        let compiled = super::deny_rules::compile(&settings.deny, &settings.deny_read, cwd)
            .map_err(|error| {
                format!(
                    "{error} [cwd={} rules={:?}]",
                    cwd.display(),
                    settings.deny_read
                )
            })?;
        // Literal rules need no walk: they travel as plain deny entries.
        for path in compiled.literal_denials {
            entries.push(SandboxEntry::deny(path));
        }
        // `[sandbox.filesystem]` entries: the same vocabulary as the overlay,
        // so a configured grant and an approved one resolve identically.
        let configured =
            super::config_entries::compile(&settings.filesystem, cwd).map_err(|error| {
                format!(
                    "{error} [cwd={} filesystem={:?}]",
                    cwd.display(),
                    settings.filesystem
                )
            })?;
        entries.extend(configured);
        Ok(Self {
            kind: SandboxKind::Restricted,
            entries,
            deny_scan_roots: session_roots,
            network: NetworkPolicy::configured_for(project_layer(state).network_block),
            deny_sensitive_files: true,
            deny_groups: compiled.groups,
            credential_dirs: settings.credential_dirs.clone(),
        })
    }

    pub(crate) fn validate(&self) -> Result<(), ResolveError> {
        if self.kind == SandboxKind::Unrestricted {
            return Ok(());
        }
        if self.entries.is_empty() {
            return Err(ResolveError::NoEntries);
        }
        for entry in &self.entries {
            if let SandboxPath::Path { path } = &entry.path
                && !path.is_absolute()
            {
                return Err(ResolveError::RelativePath(path.clone()));
            }
        }
        Ok(())
    }
}

/// The workspace's own sandbox layer, as installed on the session state.
///
/// A poisoned lock yields the empty layer: the user-level settings are the more
/// restrictive side, so losing a project's additions is the safe direction.
pub(crate) fn project_layer(state: &SessionToolState) -> crate::config::ProjectSandboxLayer {
    state
        .sandbox_project
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// A policy after materialization, merge and precedence: exactly what a backend
/// consumes.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedPolicy {
    pub(crate) kind: SandboxKind,
    /// Read grants, carve-outs included.
    pub(crate) read: Vec<PathBuf>,
    /// Write grants.
    pub(crate) write: Vec<PathBuf>,
    /// Read-only entries inside a write grant: readable, never writable. The
    /// backend must express these *on top of* the write grant, so they are
    /// kept apart from `read` (Codex's `protected_metadata_names`).
    pub(crate) carve_outs: Vec<PathBuf>,
    /// Literal denials.
    pub(crate) deny: Vec<PathBuf>,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) deny_scan_roots: Vec<PathBuf>,
    /// Scoped deny rules (see [`SandboxPolicy::deny_groups`]).
    pub(crate) deny_groups: Vec<super::deny_rules::DenyRuleGroup>,
    pub(crate) network: NetworkPolicy,
    /// Whether the backend materializes the rule table at all. The rules
    /// themselves are configuration (`[[sandbox.deny]]` + `deny_read`); this is
    /// the Windows walker's switch, so only that backend reads it.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) deny_sensitive_files: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// A `Special` path this build does not materialize yet.
    UnsupportedSpecial(SandboxSpecialPath),
    /// A glob entry with an access mode globs cannot express.
    UnsupportedGlobAccess(SandboxAccess),
    NoEntries,
    RelativePath(PathBuf),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSpecial(path) => write!(
                formatter,
                "the sandbox policy names a special path this build cannot express: {path:?}"
            ),
            Self::UnsupportedGlobAccess(access) => write!(
                formatter,
                "glob entries are deny-only; a {access:?} glob entry cannot be enforced"
            ),
            Self::NoEntries => {
                write!(
                    formatter,
                    "a restricted sandbox policy needs at least one entry"
                )
            }
            Self::RelativePath(path) => write!(
                formatter,
                "sandbox policy paths must be absolute: {}",
                path.display()
            ),
        }
    }
}

/// Materialize a path against this host: concrete paths pass through, `:root`
/// expands to every filesystem root, globs stay unresolved for the backend.
fn materialize(path: &SandboxPath, cwd: &Path) -> Result<Vec<SandboxPath>, ResolveError> {
    match path {
        SandboxPath::Path { path } => Ok(vec![SandboxPath::Path {
            path: canonical_root(path.clone()),
        }]),
        SandboxPath::Special {
            value: SandboxSpecialPath::Root,
        } => Ok(read_roots(cwd)
            .into_iter()
            .map(|path| SandboxPath::Path { path })
            .collect()),
        SandboxPath::GlobPattern { pattern } => Ok(vec![SandboxPath::GlobPattern {
            pattern: pattern.clone(),
        }]),
    }
}

/// Materialize, merge the overlay, then apply precedence.
pub(crate) fn resolve(
    policy: &SandboxPolicy,
    overlay: &AdditionalPermissions,
    cwd: &Path,
) -> Result<ResolvedPolicy, ResolveError> {
    policy.validate()?;
    let mut entries: Vec<SandboxEntry> = Vec::new();
    for entry in policy.entries.iter().chain(overlay.entries.iter()) {
        for path in materialize(&entry.path, cwd)? {
            entries.push(SandboxEntry {
                path,
                access: entry.access,
                missing_path_behavior: entry.missing_path_behavior,
            });
        }
    }

    // Precedence: deny wins over write, write over read.
    let mut read: Vec<PathBuf> = Vec::new();
    let mut write: Vec<PathBuf> = Vec::new();
    let mut deny: Vec<PathBuf> = Vec::new();
    let mut entry_globs: Vec<String> = Vec::new();
    for entry in &entries {
        match (&entry.path, entry.access) {
            (SandboxPath::Path { path }, SandboxAccess::Read) => {
                push_unique(&mut read, path.clone())
            }
            (SandboxPath::Path { path }, SandboxAccess::Write) => {
                push_unique(&mut write, path.clone())
            }
            (SandboxPath::Path { path }, SandboxAccess::Deny) => {
                push_unique(&mut deny, path.clone())
            }
            (SandboxPath::GlobPattern { pattern }, SandboxAccess::Deny) => {
                if !entry_globs.contains(pattern) {
                    entry_globs.push(pattern.clone());
                }
            }
            (SandboxPath::GlobPattern { .. }, access) => {
                return Err(ResolveError::UnsupportedGlobAccess(access));
            }
            (SandboxPath::Special { value }, _) => {
                return Err(ResolveError::UnsupportedSpecial(*value));
            }
        }
    }
    // Depth precedence (Codex's `resolve_access`): a deeper entry wins over a
    // shallower one. A denial with deeper grants inside it therefore cannot stay
    // a plain path denial — every backend refuses a denied directory as a unit,
    // which would swallow the exception — so it becomes a scoped rule whose
    // exceptions are the deeper grants and the directories leading to them. The
    // directory itself stays listed: without it the walk could not reach the
    // children it has to refuse.
    let mut literal_deny: Vec<PathBuf> = Vec::new();
    let mut deny_groups = policy.deny_groups.clone();

    // Credential directories fold into the deny set here (macOS/Linux; the
    // Windows helper expresses them itself, with its sibling-denial dance), so
    // the rewrite below turns an approved file inside one into a scoped-rule
    // exception automatically.
    #[cfg(not(target_os = "windows"))]
    if policy.kind == SandboxKind::Restricted {
        fold_credential_denies(
            &policy_credential_roots(policy),
            &mut read,
            &mut write,
            &mut deny,
            &mut deny_groups,
        );
    }

    write.retain(|path| !deny.contains(path));
    read.retain(|path| !deny.contains(path) && !write.contains(path));

    for path in &deny {
        let deeper: Vec<&PathBuf> = read
            .iter()
            .chain(write.iter())
            .filter(|grant| is_under(grant, path))
            .collect();
        if deeper.is_empty() {
            push_unique(&mut literal_deny, path.clone());
            continue;
        }
        let mut exceptions: Vec<String> = Vec::new();
        let mut write_exceptions: Vec<String> = Vec::new();
        for grant in deeper {
            // The access of the grant is the access the exception restores: a
            // read entry inside a denied subtree stays read-only.
            let restores_write = write.contains(grant);
            for path in exception_chain(path, grant) {
                let key = super::deny_rules::normalize_candidate(&path);
                if exceptions.contains(&key) || write_exceptions.contains(&key) {
                    continue;
                }
                if restores_write {
                    write_exceptions.push(key);
                } else {
                    exceptions.push(key);
                }
            }
        }
        deny_groups.push(super::deny_rules::DenyRuleGroup {
            globs: vec![format!(
                "{}/**",
                glob_escape(&super::deny_rules::normalize_candidate(path))
            )],
            exceptions,
            write_exceptions,
        });
    }

    // Deny globs that arrived as entries join the same scoped-group path as
    // configured rules: one representation for every backend.
    if !entry_globs.is_empty() {
        deny_groups.push(super::deny_rules::DenyRuleGroup {
            globs: entry_globs,
            exceptions: Vec::new(),
            write_exceptions: Vec::new(),
        });
    }

    // A read grant inside a write grant is read-only: the backend has to deny
    // writes *inside* the writable root, which is what a carve-out is (Codex's
    // protected-metadata mechanism, here driven by the configured entries).
    let mut carve_outs = workspace_carve_outs(&policy.deny_scan_roots, cwd);
    for path in &read {
        if write.iter().any(|root| is_under(path, root)) {
            push_unique(&mut carve_outs, path.clone());
        }
    }

    Ok(ResolvedPolicy {
        kind: policy.kind,
        read,
        write,
        carve_outs,
        deny: literal_deny,
        deny_scan_roots: policy.deny_scan_roots.clone(),
        deny_groups,
        network: policy.network,
        deny_sensitive_files: policy.deny_sensitive_files,
    })
}

/// Whether `path` is strictly inside `root`.
fn is_under(path: &Path, root: &Path) -> bool {
    within(path, root) && !within(root, path)
}

/// Whether `path` is `root` or anywhere below it.
///
/// Windows paths compare case-insensitively (the filesystem does), and
/// `normalize_candidate` already removed the verbatim prefix, so a path resolved
/// by `std::fs::canonicalize` matches a `dunce`-canonicalized policy root.
pub(crate) fn within(path: &Path, root: &Path) -> bool {
    let key = |value: &Path| {
        let text = super::deny_rules::normalize_candidate(value);
        #[cfg(target_os = "windows")]
        {
            text.to_ascii_lowercase()
        }
        #[cfg(not(target_os = "windows"))]
        {
            text
        }
    };
    let (path, root) = (key(path), key(root));
    let root = root.trim_end_matches('/');
    path == root || path.starts_with(&format!("{root}/"))
}

/// The user profile's credential directories (the configured list or the
/// defaults), canonicalized. A malformed configured list falls back to the
/// defaults rather than protecting nothing.
#[cfg(not(target_os = "windows"))]
fn policy_credential_roots(policy: &SandboxPolicy) -> Vec<PathBuf> {
    let profile = super::credential::profile_dir();
    super::credential::roots(profile.as_deref(), policy.credential_dirs.as_deref())
        .unwrap_or_else(|_| super::credential::roots(profile.as_deref(), None).unwrap_or_default())
}

/// Fold the credential directories into the deny set (the `resolve` call site
/// explains the three shapes). Not used on Windows: the helper expresses them
/// with its sibling-denial computation instead.
#[cfg(not(target_os = "windows"))]
#[allow(clippy::ptr_arg)] // read/write/deny are pushed and retained on
fn fold_credential_denies(
    credential_roots: &[PathBuf],
    read: &mut Vec<PathBuf>,
    write: &mut Vec<PathBuf>,
    deny: &mut Vec<PathBuf>,
    deny_groups: &mut Vec<super::deny_rules::DenyRuleGroup>,
) {
    for root in credential_roots {
        if read.contains(root) || write.contains(root) {
            // A grant naming the store itself is the listing approval: the
            // directory stays readable (enumerable) while every child stays
            // denied. A same-path write grant on a credential store is refused
            // at the gate; drop it here rather than widen.
            write.retain(|path| path != root);
            deny_groups.push(super::deny_rules::DenyRuleGroup {
                globs: vec![format!(
                    "{}/**",
                    glob_escape(&super::deny_rules::normalize_candidate(root))
                )],
                exceptions: Vec::new(),
                write_exceptions: Vec::new(),
            });
        } else {
            push_unique(deny, root.clone());
        }
    }
}

///
/// A scoped rule denies by pattern, and the walk prunes a directory it has
/// recorded. Without the intermediate directories the pruned parent would take
/// the exception down with it.
fn exception_chain(root: &Path, grant: &Path) -> Vec<PathBuf> {
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut current = Some(grant);
    while let Some(path) = current {
        if path == root || !path.starts_with(root) {
            break;
        }
        chain.push(path.to_path_buf());
        current = path.parent();
    }
    chain.reverse();
    chain
}

/// Escape glob metacharacters in a literal path so it can be used as a pattern.
fn glob_escape(path: &str) -> String {
    glob::Pattern::escape(path)
}

/// Workspace metadata that must stay read-only even though it lives inside a
/// writable root.
///
/// Every entry here is consumed by something **outside** the sandbox:
///
/// * `.git/hooks`, `.git/config`, `.git/config.worktree`, `.git/info/attributes`
///   — git executes hooks and reads command-valued configuration
///   (`core.fsmonitor`, `core.pager`, `core.sshCommand`, filters, credential
///   helpers), so a model-written copy runs with the user's full rights the next
///   time git is invoked. Only these entries are protected, **not** `.git` as a
///   whole: `git add`/`commit` write the index, objects and refs, so a
///   whole-directory carve-out would remove committing from the model's toolkit.
/// * `.agents` — agent/skill definitions injected into the system prompt.
/// * `.aws` — the AWS CLI executes credential helpers configured there.
/// * `.pigcode` — this app's own project state and the most direct path of all:
///   `hooks.toml` (core runs those commands outside the sandbox), `mcp.json`
///   (core spawns those servers outside the sandbox), `permissions.toml` (the
///   tool allow/deny rules), `agents/` and `skills/` (prompt injection). The
///   whole directory is protected rather than its known children, so a file this
///   version does not know about cannot become executable state. Plan files
///   under `.pigcode/plans` are written by the Write tool, which is not
///   sandboxed.
///
/// A worktree/submodule `.git` is a pointer file; the entries above are resolved
/// inside its `gitdir:` target as well.
///
/// The session workspace is protected even before `.pigcode` exists, so the
/// first write through the model cannot create executable state; the other
/// roots only get the metadata they already have. This is Codex's
/// `protected_metadata_names`, with our `.pigcode` added to its list.
pub(crate) fn workspace_carve_outs(roots: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
    // Compare canonical spellings: `session_write_roots` canonicalizes (symlink
    // aliases resolved) while other callers pass the entry-point spelling, so a
    // textual comparison misses the workspace when the two differ (macOS
    // `/var` → `/private/var`, a symlinked project). The workspace then loses
    // its pre-creation `.pigcode` carve-out and the prompt/gate cannot name it.
    let working_key = path_key(&canonical_root(cwd.to_path_buf()));
    let mut paths: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let is_workspace = path_key(&canonical_root(root.clone())) == working_key;
        for path in carve_outs_under(root, /*protect_project_dir*/ is_workspace) {
            push_unique(&mut paths, path);
        }
    }
    paths
}

fn carve_outs_under(workspace: &Path, protect_project_dir: bool) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let git = workspace.join(".git");
    if git.exists() {
        let gitdir = if git.is_file() {
            gitdir_from_pointer(&git)
        } else {
            Some(git.clone())
        };
        if let Some(gitdir) = gitdir {
            for relative in ["hooks", "config", "config.worktree", "info/attributes"] {
                let path = gitdir.join(relative);
                if path.exists() {
                    paths.push(path);
                }
            }
        }
    }
    for name in [".agents", ".aws"] {
        let path = workspace.join(name);
        if path.is_dir() {
            paths.push(path);
        }
    }
    let project = workspace.join(".pigcode");
    if protect_project_dir || project.is_dir() {
        let _ = std::fs::create_dir_all(&project);
        paths.push(project);
    }
    paths.into_iter().map(canonical_root).collect()
}

/// Read the `gitdir:` pointer a worktree/submodule `.git` file holds and
/// resolve it against the file's directory. Returns `None` for anything that is
/// not a valid pointer file.
fn gitdir_from_pointer(git_file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git_file).ok()?;
    let target = text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("gitdir:")
            .map(|rest| rest.trim().to_string())
    })?;
    if target.is_empty() {
        return None;
    }
    let target = PathBuf::from(target);
    Some(if target.is_absolute() {
        target
    } else {
        git_file.parent()?.join(target)
    })
}

/// Windows path identity is case-insensitive; on unix the canonical spelling is
/// already exact.
fn path_key(path: &Path) -> String {
    #[cfg(target_os = "windows")]
    {
        path.to_string_lossy().to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        path.to_string_lossy().into_owned()
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths
        .iter()
        .any(|existing| path_key(existing) == path_key(&path))
    {
        paths.push(path);
    }
}

fn push_root(roots: &mut Vec<PathBuf>, root: PathBuf) {
    push_unique(roots, canonical_root(root));
}

/// The writable roots a session command gets without any overlay: the
/// workspace, the temp roots and the extra rw roots. `for_session` turns them
/// into write entries (unless the profile is read-only) and always scans them
/// for sensitive files; the approval gate reads the same set to drop redundant
/// widening requests.
pub(crate) fn session_write_roots(cwd: &Path, state: &SessionToolState) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    push_root(&mut roots, cwd.to_path_buf());
    // The POSIX literal is meaningless on Windows, and an absolute-path
    // validation failure would take the whole sandbox down with it.
    #[cfg(not(target_os = "windows"))]
    push_root(&mut roots, PathBuf::from("/tmp"));
    #[cfg(target_os = "windows")]
    for key in ["TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            push_root(&mut roots, PathBuf::from(value));
        }
    }
    push_root(&mut roots, std::env::temp_dir());
    for root in &state.extra_rw_roots {
        push_root(&mut roots, root.clone());
    }
    roots
}

#[cfg(test)]
mod precedence_tests {
    use super::*;

    /// An absolute path on the host running the test.
    fn host(relative: &str) -> PathBuf {
        let root = if cfg!(windows) {
            PathBuf::from("C:\\")
        } else {
            PathBuf::from("/")
        };
        root.join(relative)
    }

    fn workspace() -> PathBuf {
        host("work/repo")
    }

    fn policy(entries: Vec<SandboxEntry>) -> SandboxPolicy {
        SandboxPolicy {
            kind: SandboxKind::Restricted,
            entries,
            deny_scan_roots: vec![workspace()],
            network: NetworkPolicy::Allow,
            deny_sensitive_files: false,
            deny_groups: Vec::new(),
            // Off in policy tests: `None` would resolve the defaults against
            // the real host profile and leak `~/.ssh` & co. into every
            // assertion on macOS.
            credential_dirs: Some(Vec::new()),
        }
    }

    fn resolved(entries: Vec<SandboxEntry>) -> ResolvedPolicy {
        resolve(
            &policy(entries),
            &AdditionalPermissions::default(),
            &workspace(),
        )
        .expect("resolve")
    }

    fn candidate(path: &Path) -> String {
        super::super::deny_rules::normalize_candidate(path)
    }

    /// Codex's `resolve_access`: a deeper grant wins over a shallower denial.
    /// The denial cannot stay a plain path (a denied directory is refused as a
    /// unit, which would swallow the exception), so it becomes a scoped rule
    /// whose exceptions name the grant and every directory leading to it.
    #[test]
    fn a_deeper_grant_lifts_a_shallower_denial() {
        let vault = host("vault");
        let nested = vault.join("notes");
        let note = nested.join("todo.md");
        let resolved = resolved(vec![
            SandboxEntry::deny(vault.clone()),
            SandboxEntry::read(note.clone()),
        ]);
        assert!(
            resolved.deny.is_empty(),
            "the denial is expressed as a scoped rule: {:?}",
            resolved.deny
        );
        assert_eq!(resolved.deny_groups.len(), 1);
        let group = &resolved.deny_groups[0];
        assert_eq!(group.globs, vec![format!("{}/**", candidate(&vault))]);
        assert_eq!(
            group.exceptions,
            vec![candidate(&nested), candidate(&note)],
            "the chain has to include the directory the walk must descend into"
        );
        assert!(resolved.read.contains(&note));
    }

    /// A tool path resolved by `std::fs::canonicalize` carries the Windows
    /// verbatim prefix while policy roots are `dunce`-canonicalized without it.
    /// The comparison has to treat them as the same path: when it did not,
    /// `Read ~/.ssh/known_hosts` succeeded without any approval.
    #[cfg(target_os = "windows")]
    #[test]
    fn within_ignores_the_verbatim_prefix_and_case() {
        let verbatim = Path::new(r"\\?\C:\Users\jin\.ssh\known_hosts");
        let plain = Path::new("C:/Users/jin/.ssh");
        assert_eq!(
            super::super::deny_rules::normalize_candidate(verbatim),
            "C:/Users/jin/.ssh/known_hosts",
            "the verbatim prefix has to be stripped before comparing"
        );
        assert!(
            within(verbatim, plain),
            "a verbatim-prefixed tool path is inside the policy root"
        );
        assert!(within(Path::new("c:/users/JIN/.ssh/other"), plain));
        assert!(!within(Path::new("C:/Users/jin/.ssh-other/x"), plain));
    }

    /// A denial with nothing deeper stays a literal path: no walk, no pattern.
    #[test]
    fn a_denial_without_deeper_grants_stays_literal() {
        let vault = host("vault");
        let resolved = resolved(vec![SandboxEntry::deny(vault.clone())]);
        assert_eq!(resolved.deny, vec![vault]);
        assert!(resolved.deny_groups.is_empty());
    }

    /// Credential stores fold into the deny set in three shapes (macOS/Linux;
    /// Windows expresses them in the helper).
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn credential_stores_fold_into_denies() {
        let store = host("profile").join(".ssh");
        let file = store.join("known_hosts");

        // No grant anywhere near: the store is denied whole, not even
        // listable, and no rule is compiled.
        let (mut read, mut write, mut deny, mut groups) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        super::fold_credential_denies(
            std::slice::from_ref(&store),
            &mut read,
            &mut write,
            &mut deny,
            &mut groups,
        );
        assert_eq!(deny, vec![store.clone()]);
        assert!(groups.is_empty());

        // A grant naming the store itself (the listing approval): the
        // directory stays readable while every child is denied by a rule.
        let (mut read, mut write, mut deny, mut groups) =
            (vec![store.clone()], Vec::new(), Vec::new(), Vec::new());
        super::fold_credential_denies(
            std::slice::from_ref(&store),
            &mut read,
            &mut write,
            &mut deny,
            &mut groups,
        );
        assert!(deny.is_empty(), "the store itself is not denied");
        assert!(read.contains(&store), "the listing grant survives");
        assert_eq!(groups.len(), 1);
        assert!(groups[0].exceptions.is_empty() && groups[0].write_exceptions.is_empty());

        // A grant naming a file inside: the store goes to `deny`, where the
        // deeper-grant rewrite turns it into a scoped rule excepting the file.
        let (mut read, mut write, mut deny, mut groups) =
            (vec![file.clone()], Vec::new(), Vec::new(), Vec::new());
        super::fold_credential_denies(
            std::slice::from_ref(&store),
            &mut read,
            &mut write,
            &mut deny,
            &mut groups,
        );
        assert_eq!(deny, vec![store]);
        assert!(groups.is_empty());
        assert!(
            read.contains(&file),
            "the file grant is left for the rewrite"
        );
    }

    /// A deeper denial inside a grant is not lifted: depth decides, and a denial
    /// wins at equal depth.
    #[test]
    fn a_deeper_denial_inside_a_grant_stays_literal() {
        let root = host("build");
        let denied = root.join("secrets");
        let resolved = resolved(vec![
            SandboxEntry::write(root.clone()),
            SandboxEntry::deny(denied.clone()),
        ]);
        assert_eq!(resolved.deny, vec![denied]);
        assert_eq!(resolved.write, vec![root]);
        assert!(resolved.deny_groups.is_empty());
    }

    /// A symlinked workspace spelling still receives the pre-creation `.pigcode`
    /// carve-out when the writable root uses the canonical spelling.
    #[cfg(unix)]
    #[test]
    fn workspace_carve_outs_match_a_symlinked_cwd_to_its_canonical_root() {
        let base = std::env::temp_dir().join(format!(
            "pig-workspace-carve-out-symlink-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let canonical = real.canonicalize().unwrap();

        let carve_outs = workspace_carve_outs(std::slice::from_ref(&canonical), &alias);

        assert_eq!(carve_outs, vec![canonical.join(".pigcode")]);
        assert!(canonical.join(".pigcode").is_dir());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A read grant inside a write grant is read-only: the backend has to deny
    /// writes *inside* the writable root, which is what a carve-out is.
    #[test]
    fn a_read_grant_inside_a_write_grant_becomes_a_carve_out() {
        let root = host("shared");
        let readonly = root.join("keep.txt");
        let resolved = resolved(vec![
            SandboxEntry::write(root.clone()),
            SandboxEntry::read(readonly.clone()),
        ]);
        assert!(resolved.carve_outs.contains(&readonly));
        // The grant itself is still there: the carve-out is what makes it
        // read-only, not a second write entry.
        assert!(resolved.read.contains(&readonly));
        assert_eq!(resolved.write, vec![root]);
    }

    /// A grant deeper than a denial does not change the denial's own path: the
    /// directory itself stays refused for listing.
    #[test]
    fn the_denied_directory_is_not_granted_by_its_exception() {
        let vault = host("vault");
        let note = vault.join("todo.md");
        let resolved = resolved(vec![
            SandboxEntry::deny(vault.clone()),
            SandboxEntry::read(note.clone()),
        ]);
        assert!(!resolved.read.contains(&vault));
        assert_eq!(resolved.deny_groups[0].globs.len(), 1);
    }

    /// Glob metacharacters in a denied path are escaped, so a directory named
    /// `notes[1]` cannot turn into a pattern.
    #[test]
    fn a_literal_denial_with_metacharacters_is_escaped() {
        let odd = host("odd[1]");
        let inside = odd.join("keep");
        let resolved = resolved(vec![
            SandboxEntry::deny(odd.clone()),
            SandboxEntry::read(inside.clone()),
        ]);
        let glob = &resolved.deny_groups[0].globs[0];
        assert!(glob.contains("[[]1[]]"), "{glob}");
    }
}
