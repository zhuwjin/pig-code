//! The user-profile directories that stay unreadable under full-disk read.
//!
//! The full-disk read grant covers every volume, so a credential store under the
//! profile cannot be protected by materializing files one by one. These
//! directories are therefore denied **whole** — the same shape Codex uses
//! (`USERPROFILE_ROOT_EXCLUSIONS`).
//!
//! A whole-directory denial is also a wall for approvals: a native denied path
//! is absolute, so a deeper grant does not win. That is the right default
//! (approving `~/.ssh` is a blank cheque) but it also blocks the legitimate
//! "let me touch this one file" case. The plan builder therefore expresses a
//! credential root as a set of sibling denials once a **file** inside it has
//! been approved — see `credential_sibling_denials`.
//!
//! One approval granularity is allowed: a file. Approving anything else inside
//! one of these directories is refused by the gate (`credential_grant`), because
//! a directory grant is an aggregate whose contents the user approving it
//! cannot see, and writing some of its files (`authorized_keys`, an SSH
//! `config`, a shell rc) is code execution on the next launch.

use std::path::Path;
use std::path::PathBuf;

/// Directories under the user profile that are denied whole, relative to the
/// profile. The list is part of the configuration surface: `[sandbox]
/// credential_dirs` replaces it.
///
/// `.cargo` is deliberately absent (its registry cache must stay readable);
/// the two Cargo token files are denied by name instead.
pub(crate) const DEFAULT_CREDENTIAL_DIRS: &[&str] = &[
    ".ssh",
    ".tsh",
    ".brev",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config",
    ".npm",
    ".pki",
    ".terraform.d",
    "AppData/Roaming/gcloud",
];

/// How an approved write root relates to the protected directories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialGrant {
    /// Not inside any protected directory: an ordinary grant.
    Outside,
    /// A file strictly inside one. The only approvable shape; the file is
    /// remembered only for this command (see [`is_high_risk`]).
    File,
    /// The directory itself, or a directory inside it. Refused: a directory
    /// grant is an aggregate the user cannot audit.
    Directory,
}

/// Validate one configured entry: relative to the user profile, no `..`, no
/// root, non-empty. Failing closed here keeps a malformed list from turning
/// into a grant or a silent no-op.
pub(crate) fn validate_dir_entry(entry: &str) -> Result<(), String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Err("a credential directory entry cannot be empty".to_string());
    }
    if Path::new(entry).is_absolute() || entry.starts_with('~') || entry.starts_with('/') {
        return Err(format!(
            "the credential directory `{entry}` must be relative to the user profile"
        ));
    }
    // A drive prefix is absolute in intent on every platform, even where the
    // host parser does not recognize `C:` as absolute.
    let bytes = entry.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(format!(
            "the credential directory `{entry}` must be relative to the user profile"
        ));
    }
    if entry
        .split(['/', '\\'])
        .any(|component| component == ".." || component.is_empty())
    {
        return Err(format!(
            "the credential directory `{entry}` must stay inside the user profile"
        ));
    }
    Ok(())
}

/// The user profile directory: `USERPROFILE` first (Windows, and the tests
/// redirect it), then `HOME`. Without one the credential list silently
/// protects nothing.
pub(crate) fn profile_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Resolve the configured list (or the defaults) against the user profile.
///
/// Only directories that exist are returned: a missing one has nothing to
/// protect and denying a path the host does not have would fail the launch on
/// some backends.
pub(crate) fn roots(
    profile: Option<&Path>,
    configured: Option<&[String]>,
) -> Result<Vec<PathBuf>, String> {
    let Some(profile) = profile else {
        return Ok(Vec::new());
    };
    let entries: Vec<String> = match configured {
        Some(entries) => {
            for entry in entries {
                validate_dir_entry(entry)?;
            }
            entries
                .iter()
                .map(|entry| entry.trim().to_string())
                .collect()
        }
        None => DEFAULT_CREDENTIAL_DIRS
            .iter()
            .map(|entry| (*entry).to_string())
            .collect(),
    };
    let mut roots = Vec::new();
    for entry in entries {
        let path = profile.join(entry.replace('\\', "/"));
        if path.is_dir() {
            push_unique(&mut roots, canonical(&path));
        }
    }
    Ok(roots)
}

/// Whether `path` is inside one of `roots`, and in which shape.
///
/// `roots` are the canonicalized directories from [`roots`].
pub(crate) fn credential_grant(path: &Path, roots: &[PathBuf]) -> CredentialGrant {
    let path = canonical(path);
    for root in roots {
        if same(&path, root) {
            return CredentialGrant::Directory;
        }
        if !is_under(&path, root) {
            continue;
        }
        // A file is the one approvable shape. A directory — or a path that does
        // not exist, whose kind cannot be proven — is an aggregate grant.
        return if path.is_file() {
            CredentialGrant::File
        } else {
            CredentialGrant::Directory
        };
    }
    CredentialGrant::Outside
}

/// Whether approving this inside a protected directory must never be offered
/// as "for the rest of the session".
///
/// Every file in a credential store qualifies: it configures a tool that later
/// runs outside the sandbox (an SSH client, `gpg`, a package manager), so a
/// standing grant is a standing capability the user approved for one command.
pub(crate) fn is_high_risk(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(name.as_str(), "authorized_keys" | "authorized_keys2")
}

/// The denials for one protected directory.
///
/// With no grant the root is denied as a **single path**, which also takes
/// enumeration away: the whole directory is opaque until the user approves
/// something in it. That is deliberate — reaching a file means traversing its
/// parents, Windows grants enumeration together with traverse, and the model has
/// no "traverse but do not list" right, so "can list" cannot be granted on its
/// own. Listing therefore follows from an approval, it is not a separate right.
///
/// `grants` are the files this command was approved for. Each one is skipped and
/// every directory on the path to it stays traversable with its own siblings
/// denied — one `read_dir` per level, so the cost follows the path's depth. A
/// grant that is not a file is not exception-shaped and leaves the root denied
/// whole (the gate refuses it; this is the backend's own guard).
///
/// `listing` is a separate, narrower approval: the user allowed **the names** in
/// this directory to be read. The root then stays enumerable while every child
/// is denied, so a listing shows what exists and reading any of it still fails.
///
/// A path that cannot be enumerated keeps the root denied as one path: an
/// incomplete list is a hole, so this fails closed.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn credential_sibling_denials(
    root: &Path,
    grants: &[PathBuf],
    listing: bool,
) -> Vec<PathBuf> {
    let mut denied = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut unhonorable = false;
    for grant in grants {
        let grant = canonical(grant);
        if !is_under(&grant, root) {
            continue;
        }
        if grant.is_file() {
            if !files.contains(&grant) {
                files.push(grant);
            }
        } else {
            unhonorable = true;
        }
    }
    // No approved file and no listing approval: deny the root itself. Nothing
    // inside is reachable and the directory cannot be listed.
    if unhonorable || (files.is_empty() && !listing) || !deny_siblings(root, &files, &mut denied) {
        return vec![root.to_path_buf()];
    }
    denied
}

/// Bound on the entries one protected directory may contribute. Exceeding it
/// denies the root as one path instead: that is narrower (no enumeration), so
/// the fallback cannot widen access.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const MAX_ENUMERATED_DENIALS: usize = 2048;

/// Walk one level and deny every child the grants do not need. Returns `false`
/// when the directory could not be read, so the caller can fall back to denying
/// the root whole.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn deny_siblings(dir: &Path, grants: &[PathBuf], denied: &mut Vec<PathBuf>) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        if denied.len() >= MAX_ENUMERATED_DENIALS {
            return false;
        }
        let child = canonical(&entry.path());
        if grants.iter().any(|grant| same(grant, &child)) {
            continue;
        }
        if grants.iter().any(|grant| is_under(grant, &child)) {
            // A directory on the way to a grant: keep it traversable and
            // restrict its own children.
            if !deny_siblings(&child, grants, denied) {
                return false;
            }
            continue;
        }
        push_unique(denied, entry.path());
    }
    true
}

fn same(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

/// Whether `path` is strictly inside `root`.
fn is_under(path: &Path, root: &Path) -> bool {
    if same(path, root) {
        return false;
    }
    let root = normalize(root).trim_end_matches('/').to_string();
    normalize(path).starts_with(&format!("{root}/"))
}

fn path_key(path: &Path) -> String {
    normalize(path)
}

/// Windows paths are case-insensitive; the sandbox compares them that way too.
fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    // The verbatim `\\?\` prefix is stripped so a canonicalized path and the
    // non-verbatim spelling a caller handed in compare equal. `?` in a path is
    // not a wildcard here, it is the marker Windows uses for long paths.
    let text = text.strip_prefix("//?/").unwrap_or(&text);
    #[cfg(target_os = "windows")]
    {
        text.to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        text.to_string()
    }
}

fn canonical(path: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
    #[cfg(not(target_os = "windows"))]
    {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| same(existing, &path)) {
        paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pig-credential-{}-{tag}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("fixture");
            let fixture = Self {
                dir: dir.canonicalize().expect("canonical fixture"),
            };
            std::fs::create_dir_all(fixture.dir.join(".ssh").join("keys")).expect("ssh dir");
            std::fs::write(fixture.dir.join(".ssh").join("known_hosts"), "host").expect("file");
            std::fs::write(fixture.dir.join(".ssh").join("keys").join("id_one"), "key")
                .expect("file");
            std::fs::write(fixture.dir.join(".ssh").join("keys").join("id_two"), "key")
                .expect("file");
            std::fs::write(fixture.dir.join(".ssh").join("not_keys"), "x").expect("file");
            fixture
        }

        fn profile(&self) -> PathBuf {
            self.dir.clone()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn the_default_list_resolves_only_existing_directories() {
        let fixture = Fixture::new("defaults");
        let roots = roots(Some(&fixture.profile()), None).expect("resolve");
        let ssh = fixture.profile().join(".ssh");
        assert!(roots.iter().any(|root| same(root, &ssh)));
        // `.aws` does not exist in the fixture and is not denied.
        assert!(
            !roots
                .iter()
                .any(|root| root.file_name().is_some_and(|name| name == ".aws"))
        );
    }

    #[test]
    fn a_configured_list_replaces_the_defaults() {
        let fixture = Fixture::new("configured");
        let configured = vec![".ssh".to_string()];
        let roots = roots(Some(&fixture.profile()), Some(&configured)).expect("resolve");
        assert_eq!(roots.len(), 1);
        assert!(same(&roots[0], &fixture.profile().join(".ssh")));

        let empty: Vec<String> = Vec::new();
        assert!(
            super::roots(Some(&fixture.profile()), Some(&empty))
                .expect("resolve")
                .is_empty(),
            "an empty list turns the protection off"
        );
    }

    #[test]
    fn a_malformed_entry_is_refused() {
        for entry in ["", "../elsewhere", "C:\\Windows", "/etc", "~/.ssh"] {
            let configured = vec![entry.to_string()];
            assert!(
                roots(Some(Path::new("/profile")), Some(&configured)).is_err(),
                "`{entry}` must not be accepted"
            );
        }
    }

    #[test]
    fn a_file_is_the_only_approvable_shape() {
        let fixture = Fixture::new("classify");
        let roots = roots(Some(&fixture.profile()), None).expect("resolve");
        let ssh = fixture.profile().join(".ssh");
        assert_eq!(
            credential_grant(&ssh.join("known_hosts"), &roots),
            CredentialGrant::File
        );
        assert_eq!(credential_grant(&ssh, &roots), CredentialGrant::Directory);
        assert_eq!(
            credential_grant(&ssh.join("keys"), &roots),
            CredentialGrant::Directory,
            "a directory inside a protected one is still an aggregate"
        );
        assert_eq!(
            credential_grant(&fixture.dir.join("project.txt"), &roots),
            CredentialGrant::Outside
        );
    }

    /// The single-file exception: siblings are denied, the path to the grant
    /// stays traversable, the grant itself is not denied.
    #[test]
    fn a_granted_file_denies_its_siblings_only() {
        let fixture = Fixture::new("siblings");
        let ssh = fixture.profile().join(".ssh");
        let grant = ssh.join("keys").join("id_one");
        let denied = credential_sibling_denials(&ssh, std::slice::from_ref(&grant), false);
        let keys = |path: &PathBuf| denied.iter().any(|entry| same(entry, path));

        assert!(!keys(&ssh), "the root stays traversable");
        assert!(!keys(&ssh.join("keys")), "the path to the grant stays open");
        assert!(!keys(&grant), "the grant is never denied");
        assert!(keys(&ssh.join("known_hosts")), "siblings are denied");
        assert!(keys(&ssh.join("not_keys")));
        assert!(keys(&ssh.join("keys").join("id_two")));
    }

    /// Without an approval the root itself is denied, so the directory cannot be
    /// listed and nothing inside it is reachable. Listing is not a separate
    /// right: it becomes possible only as the side effect of approving a file,
    /// because traverse and enumeration come together on Windows.
    #[test]
    fn without_a_grant_the_root_is_denied_whole() {
        let fixture = Fixture::new("no-grant");
        let ssh = fixture.profile().join(".ssh");
        let denied = credential_sibling_denials(&ssh, &[], false);
        assert_eq!(denied.len(), 1);
        assert!(same(&denied[0], &ssh), "{denied:?}");
    }

    /// A listing approval is narrower than a file grant: the root becomes
    /// enumerable and every child is still denied, so names are visible and
    /// contents are not.
    #[test]
    fn a_listing_approval_makes_names_visible_only() {
        let fixture = Fixture::new("listing");
        let ssh = fixture.profile().join(".ssh");
        let denied = credential_sibling_denials(&ssh, &[], true);
        let hit = |path: &PathBuf| denied.iter().any(|entry| same(entry, path));

        assert!(!hit(&ssh), "the root stays listable");
        assert!(hit(&ssh.join("known_hosts")), "children are denied");
        assert!(hit(&ssh.join("not_keys")), "children are denied");
        assert!(
            hit(&ssh.join("keys")),
            "a subdirectory is denied as one path"
        );
    }

    #[test]
    fn a_directory_grant_leaves_the_root_denied_whole() {
        let fixture = Fixture::new("dir-grant");
        let ssh = fixture.profile().join(".ssh");
        let denied = credential_sibling_denials(&ssh, &[ssh.join("keys")], false);
        assert_eq!(denied.len(), 1);
        assert!(same(&denied[0], &ssh));
    }

    #[test]
    fn authorized_keys_is_high_risk() {
        assert!(is_high_risk(Path::new("/home/u/.ssh/authorized_keys")));
        assert!(is_high_risk(Path::new("/home/u/.ssh/AUTHORIZED_KEYS")));
        assert!(!is_high_risk(Path::new("/home/u/.ssh/known_hosts")));
    }
}
