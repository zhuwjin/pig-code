use super::registry::SessionToolState;
use super::sandbox;

/// Whether the sandbox rules for this session deny reading `path`.
///
/// A veto, not a permit: `None` means "no rule denies it", not "the kernel
/// allows it". The file tools use it so a path the model shell cannot read is
/// also refused by Read/Grep/Glob.
pub fn sandbox_read_denial(
    state: &SessionToolState,
    cwd: &std::path::Path,
    path: &std::path::Path,
) -> Option<String> {
    sandbox::read_denial(state, cwd, path)
}

/// The session's read denials compiled once, for tools that visit many paths.
pub struct SandboxDenialView {
    pub(super) inner: sandbox::DenialView,
    /// An active network block is a second restriction an unsandboxed run
    /// would silently drop; the escalation decision must keep the sandbox on
    /// for it too (docs/WINDOWS_SANDBOX.md, `require_escalated`).
    pub(super) network_blocked: bool,
}

impl SandboxDenialView {
    pub fn new(state: &SessionToolState, cwd: &std::path::Path) -> Self {
        Self {
            inner: sandbox::DenialView::for_state(state, cwd),
            network_blocked: sandbox::network_blocked_for(state),
        }
    }

    /// `Some(reason)` when the sandbox would refuse reading `path`.
    pub fn reason(&self, path: &std::path::Path) -> Option<String> {
        self.inner.reason(path)
    }

    /// Whether an unsandboxed `require_escalated` run would drop a restriction
    /// that cannot be re-created outside the sandbox: explicit read denials
    /// (Codex's `has_denied_read_restrictions`) or an active network block.
    /// Approval keeps the sandbox on in either case.
    pub fn requires_sandbox_for_denials(&self) -> bool {
        self.inner.has_denials() || self.network_blocked
    }

    /// Record the directories an approval allows this call to list.
    pub fn with_listing_roots(mut self, roots: Vec<std::path::PathBuf>) -> Self {
        self.inner = self.inner.with_listing_roots(roots);
        self
    }

    /// Whether `path` is an entry whose name this call may see because the
    /// containing directory was approved for listing (content reads stay
    /// denied).
    pub fn listable_entry(&self, path: &std::path::Path) -> bool {
        self.inner.listable_entry(path)
    }
}

/// Metadata paths inside a writable root that the model must never write: the
/// same list the sandbox enforces for the model shell.
pub fn metadata_carve_outs(
    roots: &[std::path::PathBuf],
    cwd: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    sandbox::metadata_carve_outs(roots, cwd)
}

/// How an approved write path relates to the protected credential directories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialGrant {
    /// Outside every protected directory: an ordinary grant.
    Outside,
    /// A file inside one: approvable, but never for the whole session.
    File,
    /// The directory itself or a directory inside it: never approvable.
    Directory,
}

/// Classify one requested write path against a session's protected credential
/// directories.
pub fn credential_grant(state: &SessionToolState, path: &std::path::Path) -> CredentialGrant {
    match sandbox::credential_grant(path, &sandbox::credential_roots(state)) {
        sandbox::CredentialGrant::Outside => CredentialGrant::Outside,
        sandbox::CredentialGrant::File => CredentialGrant::File,
        sandbox::CredentialGrant::Directory => CredentialGrant::Directory,
    }
}

/// Whether writing this file inside a credential store is code execution on the
/// next launch (`authorized_keys`), so the dialog has to say so.
pub fn credential_write_is_high_risk(path: &std::path::Path) -> bool {
    sandbox::is_high_risk_credential(path)
}
