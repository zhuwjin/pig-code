//! Windows model-bash sandbox.
//!
//! The parent process never launches a model command directly. It serializes the
//! exact argv plus the filesystem policy into a bounded environment payload and
//! launches the `pig-windows-sandbox` helper, which validates the request,
//! materializes it (platform roots, sensitive-file denies) and starts the
//! workload behind MXC. See `docs/WINDOWS_SANDBOX.md` and the Windows sandbox
//! entry in `docs/PLAN.md` for the design record and its limitations.

mod availability;
mod deny;
mod native;
mod policy;
mod readiness;
mod transport;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use super::AdditionalPermissions;
use super::NetworkPolicy;
use super::SandboxCommand;
use super::SandboxPolicy;

/// Deny-rule validation for the settings editor (same code path as launch).
pub use deny::DenyRuleKind;
pub use deny::classify_deny_rule;
pub use deny::validate_deny_rule;
/// Read-only readiness snapshot (settings page; never mutates state).
pub use readiness::WindowsSandboxReadiness;
pub use readiness::readiness as sandbox_readiness;

/// Helper executable shipped next to `pig-code.exe`.
pub(crate) const HELPER_BIN: &str = "pig-windows-sandbox.exe";

pub(crate) fn prepare_command(
    policy: &SandboxPolicy,
    overlay: &AdditionalPermissions,
    cwd: &Path,
    command: SandboxCommand,
) -> io::Result<SandboxCommand> {
    prepare_command_with_helper(policy, overlay, cwd, command, None)
}

/// `helper` lets lower-level tests pin the helper path instead of relying on
/// executable-relative discovery.
pub(crate) fn prepare_command_with_helper(
    policy: &SandboxPolicy,
    overlay: &AdditionalPermissions,
    cwd: &Path,
    command: SandboxCommand,
    helper: Option<&Path>,
) -> io::Result<SandboxCommand> {
    // The policy travels as itself: the helper materializes `:root`, walks the
    // deny globs, joins the configured deny rules and applies the access
    // precedence, exactly as Codex's sandbox does. Only argv and the working
    // directory are resolved here.
    let request = transport::LaunchRequest {
        version: transport::VERSION,
        program: unicode_os_string(command.program, "program")?,
        args: command
            .args
            .into_iter()
            .map(|arg| unicode_os_string(arg, "argument"))
            .collect::<io::Result<Vec<_>>>()?,
        working_directory: unicode_path(&normalized_working_directory(cwd), "working directory")?,
        kind: policy.kind,
        entries: policy
            .entries
            .iter()
            .chain(overlay.entries.iter())
            .cloned()
            .collect(),
        deny_sensitive_files: policy.deny_sensitive_files,
        network_allow: policy.network == NetworkPolicy::Allow,
        deny_groups: policy.deny_groups.clone(),
        credential_dirs: policy.credential_dirs.clone(),
        deny_scan_roots: unicode_paths(&policy.deny_scan_roots)?,
    };
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    transport::encode(&request, &mut env)?;

    let helper = match helper {
        Some(path) => path.to_path_buf(),
        None => helper_path()?,
    };
    if !helper.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "the Windows sandbox helper {} does not exist; package or build it next to pig-code.exe",
                helper.display()
            ),
        ));
    }
    Ok(SandboxCommand {
        program: helper.into_os_string(),
        args: Vec::new(),
        env,
    })
}

/// Read-only root of the detected shell, when it is a Git installation distinct
/// from the platform roots: `...\Git\bin\bash.exe` needs `...\Git` readable for
/// its own DLLs and helper executables.
pub(crate) fn shell_read_root() -> Option<PathBuf> {
    match crate::task::windows_shell() {
        crate::task::WindowsShell::GitBash(bash) => bash
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .filter(|root| root.is_dir()),
        crate::task::WindowsShell::Cmd => None,
    }
}

/// Strip the `\\?\` verbatim prefix before it reaches the native policy: the
/// granted roots are canonicalized without it, and the launcher's shell-level
/// `Set-Location`/`cd` must use the same spelling the policy granted.
fn normalized_working_directory(cwd: &Path) -> PathBuf {
    dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

pub(crate) fn helper_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(directory) = exe.parent() {
        candidates.push(directory.join(HELPER_BIN));
        // `cargo test` runs from `target/<profile>/deps`, one level below the
        // directory that holds the sibling helper binary.
        if let Some(parent) = directory.parent() {
            candidates.push(parent.join(HELPER_BIN));
        }
    }
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "the Windows sandbox helper {HELPER_BIN} was not found next to {}; \
             build it with `cargo build -p pig-core --bin pig-windows-sandbox`",
            exe.display()
        ),
    ))
}

/// Roots of every mounted local volume, in the form the sandbox policy expects.
///
/// Windows has no single filesystem root: a read grant has to name each drive
/// (`C:\`, `D:\`, ...) plus the volume that contains the workspace. Used for
/// the full-disk read policy, which the native backend turns into
/// `readonly_paths`.
///
/// Enumeration is `GetLogicalDrives` + `GetDriveTypeW` (Codex's
/// `volume_roots`), **not** an A–Z `is_dir()` probe. Probing a disconnected
/// network share or an empty optical drive can block for seconds, and this runs
/// on every command; `GetLogicalDrives` reports the letters and the drive type
/// tells us which ones a full-disk read can actually reach. `DRIVE_NO_ROOT_DIR`,
/// `DRIVE_UNKNOWN` and `DRIVE_REMOTE` are skipped (a remote share is not part
/// of "the disk" and MXC cannot inspect its object identity), matching Codex's
/// `implicit_read_roots` filter.
///
/// A UNC share has no drive letter, so it never appears in the enumeration.
/// Codex covers that case by always adding `cwd.ancestors().last()` to the
/// volume list; the same step here makes a workspace on `\\server\share` an
/// explicit read grant instead of relying on the share's inherited `Users`
/// ACL (which matters in ReadOnly mode, where the workspace is not a write
/// root either). For an ordinary drive path the last ancestor is the drive
/// root, which is already enumerated and deduplicated here.
pub(crate) fn volume_roots_for(cwd: &Path) -> Vec<PathBuf> {
    volume_roots_including(Some(cwd))
}

/// The volume that contains `cwd`: its last ancestor. `C:\` for a drive path,
/// `\\server\share\` for a UNC path.
pub(crate) fn volume_root_for(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().last().map(Path::to_path_buf)
}

fn volume_roots_including(cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
        use windows::Win32::System::WindowsProgramming::{DRIVE_NO_ROOT_DIR, DRIVE_REMOTE};
        use windows::core::PCWSTR;

        let mask = unsafe { GetLogicalDrives() };
        for index in 0..26u32 {
            if mask & (1 << index) == 0 {
                continue;
            }
            let root = PathBuf::from(format!("{}:\\", (b'A' + index as u8) as char));
            let wide: Vec<u16> = root
                .as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let kind = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
            if kind == DRIVE_NO_ROOT_DIR {
                continue;
            }
            if kind == DRIVE_REMOTE
                && !cwd.is_some_and(|cwd| path_key(cwd).starts_with(&path_key(&root)))
            {
                continue;
            }
            // `GetLogicalDrives` also lists an empty optical drive, an ejected
            // removable volume and a disconnected share that still has a drive
            // letter. They are enumerated cheaply but cannot be inspected, and
            // handing one to MXC makes the whole launch fail or stall. One
            // `read_dir` is the same availability probe Codex's
            // `prune_unavailable_volume_roots` uses.
            if !volume_is_readable(&root) {
                continue;
            }
            roots.push(root);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cwd;
        for letter in b'A'..=b'Z' {
            let root = PathBuf::from(format!("{}:\\", letter as char));
            if root.is_dir() {
                roots.push(root);
            }
        }
    }
    // Codex-parity UNC coverage: the workspace's own volume is always named,
    // even when it has no drive letter. A normal drive root is already in the
    // list and deduplicates here by the case-insensitive key.
    if let Some(root) = cwd.and_then(volume_root_for)
        && !roots
            .iter()
            .any(|existing| path_key(existing) == path_key(&root))
    {
        roots.push(root);
    }
    if roots.is_empty() {
        // Never hand an empty read set to the launcher: fall back to the drive
        // holding the working directory.
        let cwd = cwd
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        if let Some(prefix) = cwd
            .as_deref()
            .and_then(|cwd| cwd.components().next())
            .map(|prefix| prefix.as_os_str().to_string_lossy().into_owned())
        {
            roots.push(PathBuf::from(format!("{prefix}\\")));
        }
    }
    roots
}

/// Whether a volume root can actually be enumerated right now.
///
/// A drive letter can be present while the volume is not: an empty optical
/// drive, an ejected USB stick, a share whose server went away. A single
/// `read_dir` distinguishes "ready" from those. `PermissionDenied` counts as
/// readable — the volume exists and a grant can still be expressed; only
/// "no medium / not connected / not ready" disqualify it.
fn volume_is_readable(root: &Path) -> bool {
    match std::fs::read_dir(root) {
        Ok(_) => true,
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Interrupted
        ),
    }
}

/// Case-insensitive path prefix key (`C:\` compares equal to `c:\`).
fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_ascii_lowercase()
}

/// Whether `path` is a volume root (`C:\`). A volume root is not an ordinary
/// grant target: it is a whole disk, and the well-known SIDs already carry read
/// there.
pub(crate) fn is_volume_root(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::Prefix(_)))
        && matches!(components.next(), Some(std::path::Component::RootDir))
        && components.next().is_none()
}

fn unicode_os_string(value: OsString, what: &str) -> io::Result<String> {
    value.into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the Windows sandbox requires a Unicode command {what}"),
        )
    })
}

fn unicode_path(path: &Path, what: &str) -> io::Result<String> {
    unicode_os_string(path.as_os_str().to_os_string(), what)
}

fn unicode_paths(paths: &[PathBuf]) -> io::Result<Vec<String>> {
    paths
        .iter()
        .map(|path| unicode_path(path, "policy path"))
        .collect()
}

/// Entry point for the `pig-windows-sandbox` helper binary.
///
/// Errors are printed to the helper's stderr, which the parent already forwards
/// to the model as the command result.
///
/// Flags:
/// * `--probe` — report what this process can do without launching anything;
/// * no arguments — the launch path: decode the transport payload the parent
///   left in the environment and start the workload behind MXC.
pub fn helper_main() -> i32 {
    match std::env::args().nth(1).as_deref() {
        Some("--probe") => probe_main(),
        _ => match run_helper() {
            Ok(code) => code,
            Err(message) => {
                eprintln!("pig-windows-sandbox: {message}");
                1
            }
        },
    }
}

fn probe_main() -> i32 {
    println!("sandbox: windows (psec)");
    let psec = native::process_security_environment_usable();
    let deny_paths = native::deny_paths_supported();
    println!("psec_usable: {psec}");
    println!("deny_paths_supported: {deny_paths}");
    println!("data_dir: {}", pig_utils::data_dir().display());
    for key in ["LOCALAPPDATA", "TEMP", "USERPROFILE"] {
        println!(
            "env {key}: {}",
            std::env::var(key).unwrap_or_else(|_| "<unset>".to_string())
        );
    }
    match std::env::current_dir() {
        Ok(cwd) => println!(
            "cwd: {} (writable: {})",
            cwd.display(),
            native::directory_writable(&cwd)
        ),
        Err(error) => println!("cwd: <unavailable: {error}>"),
    }
    if psec && deny_paths {
        println!("probe: ok");
        0
    } else {
        println!("probe: failed");
        1
    }
}

fn run_helper() -> Result<i32, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        let key = key.into_string().map_err(|_| {
            "the Windows sandbox requires Unicode environment variable names".to_string()
        })?;
        let value = value.into_string().map_err(|_| {
            "the Windows sandbox requires Unicode environment variable values".to_string()
        })?;
        env.insert(key, value);
    }
    let request = transport::decode_and_scrub(&mut env)?;
    let plan = policy::build_plan(&request)?;
    native::launch(&plan, &env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join("test-tmp");
        std::fs::create_dir_all(&root).expect("create command test root");
        let dir = root.join(format!("pig-sandbox-command-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create command test directory");
        dunce::canonicalize(&dir).expect("canonicalize command test directory")
    }

    fn policy(cwd: &Path) -> SandboxPolicy {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let state =
            crate::task::SessionToolState::new("windows-sandbox-test".into(), tx, vec![], vec![]);
        SandboxPolicy::for_session(cwd, &state, crate::task::SandboxProfile::WorkspaceWrite)
            .expect("policy")
    }

    fn overlay() -> AdditionalPermissions {
        AdditionalPermissions::default()
    }

    #[test]
    fn prepare_wraps_the_command_in_the_helper() {
        let cwd = temp_dir("wrap");
        let helper = cwd.join(HELPER_BIN);
        std::fs::write(&helper, "stub").unwrap();

        let command = prepare_command_with_helper(
            &policy(&cwd),
            &overlay(),
            &cwd,
            SandboxCommand::new(
                "C:\\Program Files\\Git\\bin\\bash.exe".into(),
                vec!["-c".into(), "printf '%s' \"$1\"".into()],
            ),
            Some(&helper),
        )
        .expect("prepare");

        assert_eq!(command.program, helper.into_os_string());
        assert!(
            command.args.is_empty(),
            "argv travels through the environment"
        );

        let mut map: BTreeMap<String, String> = command
            .env
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        let decoded = transport::decode_and_scrub(&mut map).expect("decode");
        assert_eq!(decoded.program, "C:\\Program Files\\Git\\bin\\bash.exe");
        assert_eq!(decoded.args, ["-c", "printf '%s' \"$1\""]);
        assert!(decoded.deny_sensitive_files);
        assert!(decoded.network_allow);
        assert!(map.is_empty());
    }

    #[test]
    fn missing_helper_fails_closed() {
        let cwd = temp_dir("missing-helper");
        let missing = cwd.join("does-not-exist.exe");

        let error = prepare_command_with_helper(
            &policy(&cwd),
            &overlay(),
            &cwd,
            SandboxCommand::new("cmd".into(), Vec::new()),
            Some(&missing),
        )
        .expect_err("a missing helper must fail");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("helper"));
    }

    #[test]
    fn unicode_arguments_round_trip_and_non_unicode_is_rejected() {
        let cwd = temp_dir("unicode");
        let helper = cwd.join(HELPER_BIN);
        std::fs::write(&helper, "stub").unwrap();

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let invalid = OsString::from_wide(&[0xD800]);
            let error = prepare_command_with_helper(
                &policy(&cwd),
                &overlay(),
                &cwd,
                SandboxCommand::new(invalid, Vec::new()),
                Some(&helper),
            )
            .expect_err("non-Unicode argv must be rejected");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }

        let command = prepare_command_with_helper(
            &policy(&cwd),
            &overlay(),
            &cwd,
            SandboxCommand::new("cmd".into(), vec!["/C".into(), "echo 你好".into()]),
            Some(&helper),
        )
        .expect("prepare");
        let mut map: BTreeMap<String, String> = command
            .env
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(
            transport::decode_and_scrub(&mut map).expect("decode").args,
            ["/C", "echo 你好"]
        );
    }

    /// A mapped network drive is excluded from the implicit full-disk read set
    /// but kept when the workspace lives on it: otherwise the model could not
    /// read the folder it was pointed at.
    #[cfg(windows)]
    #[test]
    fn a_workspace_on_a_remote_drive_keeps_that_drive_readable() {
        let roots = volume_roots_for(std::path::Path::new(r"C:\"));
        let remote_cwd = std::path::Path::new(r"Z:\projects\app");
        let with_cwd = volume_roots_for(remote_cwd);
        // Whatever the host's drive layout, the cwd volume is never dropped by
        // the "remote" filter when it contains the workspace.
        assert!(!with_cwd.is_empty());
        for root in &roots {
            assert!(with_cwd.iter().any(|kept| path_key(kept) == path_key(root)));
        }
        if roots.iter().any(|root| path_key(root) == r"z:\") {
            assert!(with_cwd.iter().any(|root| path_key(root) == r"z:\"));
        }
    }

    /// A UNC workspace has no drive letter, so `GetLogicalDrives` can never
    /// name its share; the workspace's own volume (its last ancestor) must be
    /// added explicitly (Codex's `volume_roots`).
    #[cfg(windows)]
    #[test]
    fn a_unc_workspace_contributes_its_share_root() {
        let roots = volume_roots_for(Path::new(r"\\server\share\project"));
        assert!(
            roots
                .iter()
                .any(|root| path_key(root) == path_key(Path::new(r"\\server\share\"))),
            "the UNC share root must be in the read set: {roots:?}"
        );
        // The same path through `ancestors().last()` is the share root, with or
        // without the trailing separator.
        let root = volume_root_for(Path::new(r"\\server\share\project")).expect("ancestor");
        assert_eq!(path_key(&root), path_key(Path::new(r"\\server\share\")));
    }

    #[test]
    fn volume_root_is_the_drive_root_for_normal_paths() {
        assert_eq!(
            path_key(&volume_root_for(Path::new(r"C:\work\app")).expect("ancestor")),
            r"c:\"
        );
    }

    #[test]
    fn volume_root_detection_is_exact() {
        assert!(super::is_volume_root(Path::new(r"C:\")));
        // A UNC share root is a root of the same kind: the whole share is one
        // machine-wide object, not a directory inside a grant.
        assert!(super::is_volume_root(Path::new(r"\\server\share")));
        assert!(!super::is_volume_root(Path::new(r"C:\work")));
        assert!(!super::is_volume_root(Path::new(r"\\server\share\work")));
    }
}
