//! Windows policy translation: the abstract launch request becomes the concrete
//! lists a native sandbox can enforce.
//!
//! Two things happen here that Seatbelt gets for free from its access-time
//! regexes: platform/toolchain read roots are added, and sensitive files are
//! materialized into concrete deny paths. A native Windows deny is a path, not a
//! pattern, so the deny list is a bounded snapshot: files created after it are
//! not covered (recorded in docs/PLAN.md).

use std::path::Path;
use std::path::PathBuf;

use mxc_sdk::mxc_common::filesystem_object::ExistingObjectComparison;
use mxc_sdk::mxc_common::filesystem_object::compare_existing_filesystem_objects;

use super::super::AdditionalPermissions;
use super::super::NetworkPolicy;
use super::super::ResolvedPolicy;
use super::super::SandboxPolicy;
use super::super::resolve;
use super::transport::LaunchRequest;

/// Bound on directory entries visited while materializing sensitive paths.
/// Exceeding it fails the launch instead of running with an incomplete deny
/// list. The walk itself is parallel and has no depth cap or name-based
/// pruning (Codex walks the same trees); this counter is the only safety valve,
/// raised so a large generated tree does not turn into a false "cannot run".
pub(crate) const MAX_SCAN_ENTRIES: usize = 1_000_000;
pub(crate) const MAX_DENIED_PATHS: usize = 4096;

/// Everything the native launcher needs, already normalized and deduplicated.
#[derive(Clone, Debug)]
pub(crate) struct LaunchPlan {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) working_directory: PathBuf,
    /// Explicit policy roots: the launcher must grant them or fail closed.
    pub(crate) readwrite_paths: Vec<PathBuf>,
    pub(crate) readonly_paths: Vec<PathBuf>,
    /// System roots the workload needs but does not own (`C:\Windows`, Program
    /// Files, the Git install root). They are part of the native policy but are
    /// never granted by hand: the user cannot `WRITE_DAC` there, and the
    /// well-known AppContainer SIDs already carry read/execute.
    pub(crate) platform_read_paths: Vec<PathBuf>,
    /// Toolchain caches. Granted when possible so `cargo`/`rustc` work, but a
    /// host that refuses the ACE does not fail the whole command.
    pub(crate) optional_read_paths: Vec<PathBuf>,
    pub(crate) denied_paths: Vec<PathBuf>,
    /// Paths under a writable root that the workload may read but not modify:
    /// the workspace metadata (`.git`, `.agents`, `.aws` and pig-code's own
    /// `.pigcode`, see [`metadata_carve_outs`]) plus any read grant a policy
    /// placed inside a writable root. The native policy carries them as
    /// read-only entries deeper than the writable root, so the shallow-to-deep
    /// plan lets them win.
    pub(crate) metadata_readonly_paths: Vec<PathBuf>,
    pub(crate) network_allow: bool,
}

impl LaunchPlan {
    /// Concrete paths a native grant would stamp, used for diagnostics and
    /// tests.
    #[cfg(test)]
    pub(crate) fn granted_paths(&self) -> usize {
        self.readwrite_paths.len() + self.readonly_paths.len()
    }

    /// Full read-only policy list, for the native request.
    pub(crate) fn all_readonly_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.readonly_paths.clone();
        for path in &self.metadata_readonly_paths {
            push_unique(&mut paths, path.clone());
        }
        for path in &self.platform_read_paths {
            push_unique(&mut paths, path.clone());
        }
        for path in &self.optional_read_paths {
            push_unique(&mut paths, path.clone());
        }
        paths
    }
}

pub(crate) fn build_plan(request: &LaunchRequest) -> Result<LaunchPlan, String> {
    let working_directory = PathBuf::from(&request.working_directory);
    if !working_directory.is_absolute() {
        return Err("the sandbox working directory must be absolute".into());
    }
    if !working_directory.is_dir() {
        return Err(format!(
            "the sandbox working directory does not exist: {}",
            working_directory.display()
        ));
    }
    if request.program.is_empty() {
        return Err("the sandbox command program cannot be empty".into());
    }

    // Resolve the policy the parent sent — materialize `:root` into this host's
    // volumes, apply the overlay (already merged into `entries`), then the
    // access precedence — exactly the resolution Codex's sandbox performs.
    let policy = SandboxPolicy {
        kind: request.kind,
        entries: request.entries.clone(),
        deny_scan_roots: normalize_roots(&request.deny_scan_roots)?,
        network: if request.network_allow {
            NetworkPolicy::Allow
        } else {
            NetworkPolicy::Block
        },
        deny_sensitive_files: request.deny_sensitive_files,
        deny_groups: request.deny_groups.clone(),
        credential_dirs: request.credential_dirs.clone(),
    };
    let resolved: ResolvedPolicy = resolve(
        &policy,
        &AdditionalPermissions::default(),
        &working_directory,
    )
    .map_err(|error| error.to_string())?;
    if resolved.kind == super::super::SandboxKind::Unrestricted {
        return Err("the Windows sandbox helper never receives an unrestricted policy".into());
    }

    let mut readwrite = resolved.write.clone();
    // Codex's `expand_volume_roots`: an MXC volume-root grant is nonrecursive,
    // so a root write (`EscalatedSandboxWithRestrictions`) must snapshot each
    // volume's immediate children as recursive grants. Keep the original list
    // for the deny-scan roots: an expanded `C:\Users` must not become a scan
    // root (that would walk the user profile on every command).
    let approved_write_roots = readwrite.clone();
    readwrite = expand_volume_roots(readwrite)?;
    // Machine-wide roots (volume roots from the full-disk read policy, plus the
    // system directories) are carried separately: the native launcher needs them
    // in its read-only list, and they are read through the well-known SIDs
    // rather than a hand-made grant.
    let (machine_wide, mut readonly): (Vec<PathBuf>, Vec<PathBuf>) = resolved
        .read
        .iter()
        .cloned()
        .partition(|path| super::is_volume_root(path) || is_platform_root(path));
    let mut platform = machine_wide;

    // Read denials are materialized once per launch: literal paths need no
    // walk at all, and globs are expanded with the `ignore` walker over the
    // policy's writable roots plus each glob's literal prefix. The readable set
    // is full-disk, so scanning it is not an option — the same shape Codex uses
    // (patterns and known directories, never a full-volume materialization).
    let mut denied = Vec::new();
    if resolved.deny_sensitive_files {
        // The parent compiled the rule groups (built-ins + configuration) and
        // carried them as data; this side only compiles matchers.
        let rules =
            super::deny::from_groups(&resolved.deny_groups).map_err(|error| error.to_string())?;
        // Literal deny entries from the policy itself.
        for path in &resolved.deny {
            push_unique(&mut denied, normalize_root(path));
        }
        let mut scan_roots: Vec<PathBuf> = resolved
            .deny_scan_roots
            .iter()
            .map(|root| normalize_root(root))
            .collect();
        // Roots approved for this one command are writable, so their sensitive
        // files need the same treatment as the workspace's: an approval to
        // write in a directory is not an approval to read its credentials.
        // Volume roots are skipped: an EscalatedSandboxWithRestrictions root
        // write makes every volume writable, and walking the whole disk for
        // every command is not a policy (Codex resolves its globs against the
        // policy cwd instead of materializing the readable set).
        for root in &approved_write_roots {
            if !super::is_volume_root(root) {
                push_unique(&mut scan_roots, normalize_root(root));
            }
        }
        if scan_roots.is_empty() {
            push_unique(&mut scan_roots, working_directory.clone());
        }
        for root in rules.extra_scan_roots() {
            let root = normalize_root(root);
            // A user glob's literal prefix is usually inside the workspace,
            // where the workspace walk already covers it. Walking it again
            // would double the cost (and the entry budget) for nothing.
            if scan_roots.iter().any(|existing| is_under(&root, existing)) {
                continue;
            }
            // Conversely, a new prefix can contain an already-listed root.
            scan_roots.retain(|existing| !is_under(existing, &root));
            push_unique(&mut scan_roots, root);
        }
        for root in &scan_roots {
            // Scratch directories are not a credential store; see the module
            // note in `deny`. Exact identity only, so a workspace nested under
            // %TEMP% is still scanned.
            if is_scratch_root(root) {
                continue;
            }
            let outcome = super::deny::scan(root, &rules)?;
            if outcome.truncated {
                return Err(format!(
                    "the sensitive-file scan under {} exceeded its bounds; refusing to run with an \
                     incomplete deny list",
                    root.display()
                ));
            }
            for path in outcome.paths {
                push_unique(&mut denied, path);
            }
        }
        // Credential directories are denied whole, EXCEPT when this command
        // holds an approved grant for a file inside one, or an approved listing
        // of the directory itself: then the root is expressed as its siblings
        // instead, so the approved file stays reachable (and the root stays
        // listable) while every sibling stays denied.
        let credential_roots = super::super::credential::roots(
            user_profile().as_deref(),
            request.credential_dirs.as_deref(),
        )?;
        // Either access opens a path through the directory: a granted write for
        // a write, a granted read for a read. The others stay denied.
        let credentialed_files: Vec<PathBuf> = readwrite
            .iter()
            .chain(readonly.iter())
            .filter(|path| {
                super::super::credential::credential_grant(path, &credential_roots)
                    == super::super::credential::CredentialGrant::File
            })
            .cloned()
            .collect();
        for root in &credential_roots {
            // A read grant naming a directory inside the store (or the store
            // itself) is a listing grant: it keeps the root enumerable and
            // denies its children, so names are visible and contents are not.
            let listing = readonly
                .iter()
                .any(|path| path.is_dir() && super::super::model::within(path, root));
            for path in super::super::credential::credential_sibling_denials(
                root,
                &credentialed_files,
                listing,
            ) {
                push_unique(&mut denied, path);
            }
        }
        // `.cargo` is not one of the credential directories above (the registry
        // cache under it must stay readable), so its two token files are denied
        // by name.
        for path in cargo_credential_paths() {
            push_unique(&mut denied, path);
        }
    }

    for root in platform_read_roots() {
        push_unique(&mut platform, root);
    }
    let mut optional = Vec::new();
    for root in toolchain_read_roots() {
        push_unique(&mut optional, root);
    }

    // Workspace metadata the model may read but must not modify: derived from
    // the same roots as the deny scan, so a read-only launch protects exactly
    // the same files as a writable one.
    let mut metadata_readonly = Vec::new();
    // Carve-outs can be files (`.git/config`) as well as directories; the model
    // already skipped roots that do not exist.
    for path in &resolved.carve_outs {
        push_unique(&mut metadata_readonly, normalize_root(path));
    }

    // Denied access wins over every grant; a writable grant wins over a
    // read-only one for the same path identity. The read-only carve-outs are
    // part of "every grant": a path a rule denies must not stay in the list
    // that re-grants it read access.
    let denied_keys: Vec<String> = denied.iter().map(|path| path_key(path)).collect();
    metadata_readonly.retain(|path| !denied_keys.contains(&path_key(path)));
    readwrite.retain(|path| !denied_keys.contains(&path_key(path)));
    readonly.retain(|path| {
        let key = path_key(path);
        !denied_keys.contains(&key) && !readwrite.iter().any(|rw| path_key(rw) == key)
    });
    platform.retain(|path| {
        let key = path_key(path);
        !denied_keys.contains(&key)
            && !readwrite.iter().any(|granted| path_key(granted) == key)
            && !readonly.iter().any(|granted| path_key(granted) == key)
    });
    optional.retain(|path| {
        let key = path_key(path);
        !denied_keys.contains(&key)
            && !readwrite.iter().any(|granted| path_key(granted) == key)
            && !readonly.iter().any(|granted| path_key(granted) == key)
    });

    // A carve-out must not also appear as a grant of the same path: the deepest
    // entry wins, and a duplicate would be ambiguous. Removing the shallow
    // duplicate is what MXC's own normalizer does for exact same-path entries.
    let metadata_keys: Vec<String> = metadata_readonly.iter().map(|p| path_key(p)).collect();
    readwrite.retain(|path| !metadata_keys.contains(&path_key(path)));
    readonly.retain(|path| !metadata_keys.contains(&path_key(path)));
    platform.retain(|path| !metadata_keys.contains(&path_key(path)));
    optional.retain(|path| !metadata_keys.contains(&path_key(path)));

    Ok(LaunchPlan {
        program: request.program.clone(),
        args: request.args.clone(),
        working_directory,
        readwrite_paths: readwrite,
        readonly_paths: readonly,
        platform_read_paths: platform,
        optional_read_paths: optional,
        denied_paths: denied,
        metadata_readonly_paths: metadata_readonly,
        network_allow: resolved.network == NetworkPolicy::Allow,
    })
}

/// Platform roots the sandboxed command needs to run system binaries. Read-only
/// and never widened to the user profile (see the handoff document's warning
/// about granting `APPDATA`/`LOCALAPPDATA`).
pub(crate) fn platform_read_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for key in [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
    ] {
        if let Some(value) = std::env::var_os(key) {
            push_existing(&mut roots, PathBuf::from(value));
        }
    }
    roots
}

/// Whether `path` is one of the machine-wide platform roots, which are read
/// through the well-known SIDs rather than a hand-made grant.
fn is_platform_root(path: &Path) -> bool {
    platform_read_roots()
        .iter()
        .any(|root| path_key(root) == path_key(path))
}

/// The user profile the credential list is resolved against.
fn user_profile() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// Cargo credential files under the profile. `.cargo` sits inside the profile,
/// which the full-disk read grant covers, so these two are denied explicitly.
pub(crate) fn cargo_credential_paths() -> Vec<PathBuf> {
    let Some(profile) = std::env::var_os("USERPROFILE") else {
        return Vec::new();
    };
    let cargo = PathBuf::from(profile).join(".cargo");
    let mut paths = Vec::new();
    for name in ["credentials.toml", "credentials"] {
        let path = cargo.join(name);
        if path.is_file() {
            paths.push(dunce::canonicalize(&path).unwrap_or(path));
        }
    }
    paths
}

/// Toolchain caches a real command needs, mirroring the macOS backend's
/// `HOME_RUNTIME_READ_ROOTS`.
pub(crate) fn toolchain_read_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("USERPROFILE") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let mut roots = Vec::new();
    for sub in [".cargo\\bin", ".cargo\\registry", ".cargo\\git", ".rustup"] {
        push_existing(&mut roots, home.join(sub));
    }
    roots
}

fn normalize_roots(roots: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut normalized: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        let path = Path::new(root);
        if !path.is_absolute() {
            return Err(format!("sandbox root is not absolute: {root}"));
        }
        push_unique(&mut normalized, normalize_root(path));
    }
    Ok(normalized)
}

/// Canonicalize when possible and always strip the `\\?\` verbatim prefix so
/// native object identity sees the same spelling the shell uses.
/// MXC volume-root grants are nonrecursive: snapshot each volume's immediate
/// children as recursive grants (Codex's `expand_volume_roots`). A child
/// created after this snapshot is not granted; an unavailable volume is
/// skipped, not fatal.
fn expand_volume_roots(roots: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    let mut expanded = Vec::new();
    for root in roots {
        if super::is_volume_root(&root) {
            match std::fs::read_dir(&root) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let child = entry.path();
                        if child.to_str().is_none() {
                            continue;
                        }
                        // Codex's object-identity probe: generated grants may
                        // be skipped when MXC cannot inspect them (a protected
                        // system file such as `C:\DumpStack.log` would otherwise
                        // fail the whole policy when denies are present).
                        if compare_existing_filesystem_objects(&child, &child)
                            != ExistingObjectComparison::Same
                        {
                            continue;
                        }
                        push_unique(&mut expanded, normalize_root(&child));
                    }
                }
                Err(_) => {
                    // GetLogicalDrives includes disconnected and empty
                    // removable volumes; they must not fail a command on a
                    // healthy volume.
                }
            }
        }
        push_unique(&mut expanded, normalize_root(&root));
    }
    Ok(expanded)
}

fn normalize_root(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn push_existing(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if root.is_dir() {
        push_unique(roots, normalize_root(&root));
    }
}

fn push_unique(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !roots
        .iter()
        .any(|existing| path_key(existing) == path_key(&root))
    {
        roots.push(root);
    }
}

/// Windows path identity is case-insensitive.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_ascii_lowercase()
}

/// Whether `path` is `root` or lives under it (case-insensitive, separator
/// aware). Used to avoid walking a nested scan root twice.
fn is_under(path: &Path, root: &Path) -> bool {
    let path = path_key(path);
    let root = path_key(root);
    let root = root.trim_end_matches('\\');
    path == root || path.starts_with(&format!("{root}\\"))
}

/// Snapshot the sensitive files under `roots` (`.env` variants, SSH private
/// keys, cloud credential files; see `tool::is_sensitive_file` for the shared
/// predicate and its template/public-key exceptions).
///
/// **Workspace roots** are walked without a depth cap; every matching file is
/// denied, including files under build and dependency outputs (Codex's resolver
/// does not prune them either). The walk fails closed when the entry or
/// denied-path bound is hit, or when a directory cannot be read.
///
/// **Temporary roots are not scanned.** They are scratch space rather than a
/// credential store, they are already writable by the workload (so denying a
/// file there protects nothing the model cannot recreate), and the host
/// `%TEMP%` is enormous: 221k entries on the machine this was measured on,
/// with 62k files directly in its top level. A full walk cost 1.5s + 0.8s and
/// even the top-level-only variant still cost ~80ms on *every* command, which
/// dominated a model `ls` users perceived as slow. Codex does not snapshot
/// temporary trees either: its Windows deny list is the declared deny-read
/// roots/globs.
///
/// Directory symlinks and junctions are not traversed: the resolver stays inside
/// the granted tree, and an alias into it is still covered because the deny is
/// attached to the target object.
/// Whether `path` is one of the scratch roots the snapshot skips. Exact
/// identity only: a workspace nested under `%TEMP%` is still a workspace.
fn is_scratch_root(path: &Path) -> bool {
    temp_scan_roots()
        .iter()
        .any(|scratch| path_key(scratch) == path_key(path))
}

/// Scratch-directory roots: the sensitive-file snapshot skips these whole.
fn temp_scan_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for key in ["TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            push_unique(&mut roots, normalize_root(Path::new(&value)));
        }
    }
    push_unique(&mut roots, normalize_root(&std::env::temp_dir()));
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join("test-tmp");
        std::fs::create_dir_all(&root).expect("create plan test root");
        let dir = root.join(format!("pig-sandbox-plan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create plan test directory");
        dunce::canonicalize(&dir).expect("canonicalize plan test directory")
    }

    /// The groups a fresh install is seeded with (`[[sandbox.deny]]`), in the
    /// compiled wire shape.
    fn seeded_groups() -> Vec<crate::task::sandbox::deny_rules::DenyRuleGroup> {
        crate::config::default_deny_groups()
            .iter()
            .map(crate::task::sandbox::deny_rules::DenyRuleGroup::from)
            .collect()
    }

    fn request(work: &Path) -> LaunchRequest {
        LaunchRequest {
            version: super::super::transport::VERSION,
            program: "cmd".into(),
            args: vec!["/C".into(), "echo hello".into()],
            working_directory: work.display().to_string(),
            kind: crate::task::sandbox::SandboxKind::Restricted,
            entries: vec![
                crate::task::sandbox::SandboxEntry::read_root(),
                crate::task::sandbox::SandboxEntry::write(work.to_path_buf()),
            ],
            deny_sensitive_files: true,
            network_allow: true,
            deny_groups: super::super::super::deny_rules::compile(&seeded_groups(), &[], work)
                .expect("rules")
                .groups,
            credential_dirs: Some(Vec::new()),
            deny_scan_roots: vec![work.display().to_string()],
        }
    }

    #[test]
    fn grants_workspace_and_platform_roots() {
        let work = temp_dir("grants");
        let plan = build_plan(&request(&work)).expect("plan");

        assert!(plan.readwrite_paths.contains(&work));
        assert!(
            plan.readonly_paths
                .iter()
                .all(|path| path_key(path) != path_key(&work)),
            "a writable root must not be duplicated as read-only"
        );
        assert!(plan.network_allow);
    }

    /// The executable surfaces of `.git` and pig-code's project state are
    /// read-only: they are consumed by processes running *outside* the sandbox,
    /// so a model-written copy would be code execution on the next launch.
    #[test]
    fn workspace_metadata_is_read_only_inside_a_writable_root() {
        let work = temp_dir("metadata");
        std::fs::create_dir_all(work.join(".git").join("hooks")).unwrap();
        std::fs::create_dir_all(work.join(".git").join("objects")).unwrap();
        std::fs::write(work.join(".git").join("config"), "[core]\n").unwrap();
        std::fs::create_dir_all(work.join(".agents")).unwrap();
        std::fs::create_dir_all(work.join(".aws")).unwrap();
        std::fs::write(work.join(".aws").join("config"), "x").unwrap();

        let plan = build_plan(&request(&work)).expect("plan");
        let readonly: Vec<String> = plan
            .all_readonly_paths()
            .iter()
            .map(|path| path_key(path))
            .collect();
        for relative in [".git/hooks", ".git/config", ".agents", ".aws", ".pigcode"] {
            let path = dunce::canonicalize(work.join(relative)).unwrap_or(work.join(relative));
            assert!(
                readonly.contains(&path_key(&path)),
                "{relative} must be read-only: {readonly:?}"
            );
        }
        // `.pigcode` is created so the carve-out can be expressed even though
        // nothing under it exists yet.
        assert!(work.join(".pigcode").is_dir());
        // `.git/objects` is data, not an executable surface: a whole-`.git`
        // carve-out would make `git add`/`commit` impossible in the sandbox.
        let objects = dunce::canonicalize(work.join(".git").join("objects")).unwrap();
        assert!(
            !readonly.contains(&path_key(&objects)),
            ".git/objects must stay writable: {readonly:?}"
        );
        // Carve-outs are not grants.
        assert!(
            plan.readwrite_paths
                .iter()
                .all(|path| path_key(path) != path_key(&work.join(".git").join("hooks"))),
            "{:?}",
            plan.readwrite_paths
        );
        assert!(plan.readwrite_paths.contains(&work));
    }

    /// A worktree/submodule `.git` is a pointer file; the object directory it
    /// names holds the hooks and config git actually reads.
    #[test]
    fn worktree_gitdir_pointer_target_is_read_only() {
        let work = temp_dir("worktree");
        let real = work.join("real-gitdir");
        std::fs::create_dir_all(real.join("hooks")).unwrap();
        std::fs::write(real.join("config"), "[core]\n").unwrap();
        std::fs::write(work.join(".git"), format!("gitdir: {}\n", real.display())).unwrap();

        let plan = build_plan(&request(&work)).expect("plan");
        let readonly: Vec<String> = plan
            .all_readonly_paths()
            .iter()
            .map(|path| path_key(path))
            .collect();
        for path in [real.join("hooks"), real.join("config")] {
            let path = dunce::canonicalize(&path).unwrap();
            assert!(
                readonly.contains(&path_key(&path)),
                "{} must be read-only: {readonly:?}",
                path.display()
            );
        }
    }

    #[test]
    fn materializes_sensitive_files_and_keeps_exceptions() {
        let work = temp_dir("sensitive");
        std::fs::create_dir_all(work.join("nested")).unwrap();
        std::fs::write(work.join(".env"), "SECRET=1").unwrap();
        std::fs::write(work.join("nested").join(".env.local"), "SECRET=2").unwrap();
        std::fs::write(work.join(".env.example"), "SECRET=3").unwrap();
        std::fs::write(work.join("id_rsa"), "key").unwrap();
        std::fs::write(work.join("id_rsa.pub"), "pub").unwrap();
        std::fs::create_dir_all(work.join(".aws")).unwrap();
        std::fs::write(work.join(".aws").join("credentials"), "cloud").unwrap();

        let plan = build_plan(&request(&work)).expect("plan");

        let denied: Vec<String> = plan.denied_paths.iter().map(|p| path_key(p)).collect();
        assert!(denied.iter().any(|path| path.ends_with("\\.env")));
        assert!(denied.iter().any(|path| path.ends_with("\\.env.local")));
        assert!(denied.iter().any(|path| path.ends_with("\\id_rsa")));
        assert!(
            denied
                .iter()
                .any(|path| path.ends_with("\\.aws\\credentials"))
        );
        assert!(!denied.iter().any(|path| path.ends_with(".env.example")));
        assert!(!denied.iter().any(|path| path.ends_with("id_rsa.pub")));
    }

    /// "Deny this directory, except one file inside it" — the shape Codex's
    /// config can express. The denial becomes a scoped rule, so the walk
    /// records the siblings and leaves the excepted file alone; the excepted
    /// file keeps its own read grant.
    #[test]
    fn a_deeper_grant_keeps_one_file_inside_a_denied_directory() {
        let work = temp_dir("deny-except");
        let vault = work.join("vault");
        std::fs::create_dir_all(vault.join("nested")).unwrap();
        std::fs::write(vault.join("token.txt"), "SECRET").unwrap();
        std::fs::write(vault.join("nested").join("deep.txt"), "SECRET").unwrap();
        std::fs::write(vault.join("public.txt"), "PUBLIC").unwrap();

        let mut request = request(&work);
        request.entries = vec![
            crate::task::sandbox::SandboxEntry::read_root(),
            crate::task::sandbox::SandboxEntry::write(work.clone()),
            crate::task::sandbox::SandboxEntry::deny(vault.clone()),
            crate::task::sandbox::SandboxEntry::read(vault.join("public.txt")),
        ];
        let plan = build_plan(&request).expect("plan");
        let denied: Vec<String> = plan
            .denied_paths
            .iter()
            .map(|path| path_key(path))
            .collect();

        let token = path_key(&vault.join("token.txt"));
        // A denied subdirectory is recorded as a unit, which covers everything
        // under it — the walk prunes it rather than listing its children.
        let nested = path_key(&vault.join("nested"));
        let public = path_key(&vault.join("public.txt"));

        assert!(denied.contains(&token), "{denied:?}");
        assert!(denied.contains(&nested), "{denied:?}");
        assert!(
            !denied.contains(&public),
            "the exception must survive the walk: {denied:?}"
        );
        // The exception sits inside the writable workspace, so it is a
        // read-only carve-out rather than a plain read grant: readable, and not
        // writable through the workspace grant that covers it.
        assert!(
            plan.metadata_readonly_paths
                .iter()
                .any(|path| path_key(path) == public),
            "the excepted file keeps its read grant"
        );
    }

    /// A configured `deny_read` rule (and a configured group) beats a
    /// `[sandbox.filesystem]` grant, even when the grant is deeper: depth
    /// precedence orders the entry list, while the rule groups are a separate
    /// mechanism that is applied last.
    #[test]
    fn a_deny_rule_beats_a_filesystem_grant() {
        let work = temp_dir("rule-vs-entry");
        let vault = work.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(vault.join("notes.md"), "NOTES").unwrap();

        let mut request = request(&work);
        // A rule (the `deny_read` shorthand and a structured group share this
        // path) plus a deeper read entry for the same file.
        let compiled =
            super::super::super::deny_rules::compile(&[], &["**/notes.md".to_string()], &work)
                .expect("rules");
        assert!(
            compiled.literal_denials.is_empty(),
            "a glob rule has to travel as a group"
        );
        request.deny_groups = compiled.groups;
        request.entries = vec![
            crate::task::sandbox::SandboxEntry::read_root(),
            crate::task::sandbox::SandboxEntry::write(work.clone()),
            crate::task::sandbox::SandboxEntry::read(vault.join("notes.md")),
        ];

        let plan = build_plan(&request).expect("plan");
        let notes = path_key(&vault.join("notes.md"));
        assert!(
            plan.denied_paths.iter().any(|path| path_key(path) == notes),
            "the rule wins: {:?}",
            plan.denied_paths
        );
        assert!(
            plan.metadata_readonly_paths
                .iter()
                .all(|path| path_key(path) != notes),
            "the grant must not survive the denial"
        );
    }

    #[test]
    fn deny_wins_over_grants() {
        let work = temp_dir("precedence");
        std::fs::write(work.join(".env"), "SECRET=1").unwrap();
        let mut request = request(&work);
        request.entries = vec![
            crate::task::sandbox::SandboxEntry::read_root(),
            crate::task::sandbox::SandboxEntry::write(work.clone()),
            crate::task::sandbox::SandboxEntry::read(work.join(".env")),
        ];

        let plan = build_plan(&request).expect("plan");
        let denied = path_key(&work.join(".env"));

        assert!(
            plan.denied_paths
                .iter()
                .any(|path| path_key(path) == denied)
        );
        assert!(
            plan.readwrite_paths
                .iter()
                .all(|path| path_key(path) != denied)
        );
        assert!(
            plan.readonly_paths
                .iter()
                .all(|path| path_key(path) != denied)
        );
    }

    /// Codex parity: the deny walk does not skip `.git`; a matching file there
    /// is a real path the sandbox must not read.
    #[test]
    fn walks_git_directories() {
        let work = temp_dir("walk-git");
        std::fs::create_dir_all(work.join(".git").join("objects")).unwrap();
        std::fs::write(work.join(".git").join(".env"), "scanned").unwrap();

        let plan = build_plan(&request(&work)).expect("plan");

        assert!(
            plan.denied_paths
                .iter()
                .any(|path| path_key(path).contains("\\.git\\")),
            "a .env under .git must be denied: {:?}",
            plan.denied_paths
        );
    }

    #[test]
    fn rejects_relative_or_missing_working_directory() {
        let work = temp_dir("bad-cwd");
        let mut relative = request(&work);
        relative.working_directory = "relative".into();
        assert!(build_plan(&relative).is_err());

        let mut missing = request(&work);
        missing.working_directory = work.join("missing").display().to_string();
        assert!(build_plan(&missing).is_err());
    }

    #[test]
    fn rejected_relative_root() {
        let work = temp_dir("bad-root");
        let mut request = request(&work);
        request.entries = vec![crate::task::sandbox::SandboxEntry::new(
            crate::task::sandbox::SandboxPath::Path {
                path: PathBuf::from("relative"),
            },
            crate::task::sandbox::SandboxAccess::Write,
        )];

        assert!(build_plan(&request).is_err());
    }

    /// Scratch directories are skipped whole; a workspace that merely lives
    /// under one is still scanned (the exact-match rule).
    #[test]
    fn scratch_roots_are_skipped_but_their_children_are_not() {
        let scratch = temp_scan_roots();
        assert!(!scratch.is_empty());
        for root in &scratch {
            assert!(
                is_scratch_root(root),
                "{} should be scratch",
                root.display()
            );
            let child = root.join("some-workspace");
            assert!(
                !is_scratch_root(&child),
                "{} is under scratch but must still be scanned",
                child.display()
            );
        }
    }

    #[test]
    fn scan_errors_fail_closed() {
        let work = temp_dir("scan-error");
        let missing = work.join("missing");
        let mut request = request(&work);
        request.entries = vec![
            crate::task::sandbox::SandboxEntry::read_root(),
            crate::task::sandbox::SandboxEntry::write(missing),
        ];

        assert!(build_plan(&request).is_err());
    }

    #[test]
    fn test_plan_counts_grants() {
        let work = temp_dir("grant-count");
        let plan = build_plan(&request(&work)).expect("plan");
        assert!(plan.granted_paths() >= 1);
    }
}
