//! Linux model-bash sandbox backed by Bubblewrap (`bwrap`).
//!
//! Enforces read-only root, explicit writable workspace roots, protected
//! metadata carve-outs, literal/glob read denials (masked with `--tmpfs` for
//! directories and `--ro-bind /dev/null` for files), PID namespace isolation,
//! and optional `--unshare-net` network isolation.
//!
//! Fail-closed: if OS sandboxing is enabled and `bwrap` is missing or blocked
//! (for example when unprivileged user namespaces are disabled by AppArmor or
//! kernel sysctl), commands are refused with an explicit error instead of
//! silently running unsandboxed.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::deny_rules;
use super::{NetworkPolicy, ResolvedPolicy, SandboxCommand, SandboxKind};

#[cfg(target_os = "linux")]
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
fn find_bwrap() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("bwrap");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for fallback in ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"] {
        let path = PathBuf::from(fallback);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Probe whether `bwrap` can actually create a mount/PID namespace on this host.
/// Cached per process; never falls back to unsandboxed execution.
#[cfg(target_os = "linux")]
pub(crate) fn probe() -> Result<PathBuf, String> {
    static PROBE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let Some(bwrap) = find_bwrap() else {
                return Err(
                    "Linux bubblewrap (`bwrap`) is not installed. Install `bubblewrap` or set \
                     `[sandbox] enabled = false` in user config to run without an OS sandbox."
                        .to_string(),
                );
            };
            let output = std::process::Command::new(&bwrap)
                .args([
                    "--ro-bind",
                    "/",
                    "/",
                    "--dev",
                    "/dev",
                    "--proc",
                    "/proc",
                    "--unshare-pid",
                    "--die-with-parent",
                    "--",
                    "/bin/true",
                ])
                .output();
            match output {
                Ok(out) if out.status.success() => Ok(bwrap),
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    let detail = if stderr.is_empty() {
                        format!("exit status {}", out.status)
                    } else {
                        stderr
                    };
                    Err(format!(
                        "Linux bubblewrap (`{}`) failed its namespace probe ({detail}). Enable \
                         unprivileged user namespaces or set `[sandbox] enabled = false` in user \
                         config to run without an OS sandbox.",
                        bwrap.display()
                    ))
                }
                Err(error) => Err(format!(
                    "Linux bubblewrap (`{}`) could not be executed ({error}). Set \
                     `[sandbox] enabled = false` in user config to run without an OS sandbox.",
                    bwrap.display()
                )),
            }
        })
        .clone()
}

#[cfg(target_os = "linux")]
pub(crate) fn is_available() -> bool {
    probe().is_ok()
}

/// Collect concrete existing files/directories matching configured deny groups
/// under `deny_scan_roots` so bwrap can mask them at mount time.
fn scan_denied_paths(policy: &ResolvedPolicy, cwd: &Path) -> Vec<PathBuf> {
    if policy.deny_groups.is_empty() || policy.deny_scan_roots.is_empty() {
        return Vec::new();
    }
    let Ok(compiled) = deny_rules::compile(&policy.deny_groups, &[], cwd) else {
        return Vec::new();
    };
    let mut matched = BTreeSet::new();
    for root in &policy.deny_scan_roots {
        if !root.exists() {
            continue;
        }
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .max_depth(Some(8))
            .build();
        for entry in walker.flatten() {
            let path = entry.path();
            if path == root {
                continue;
            }
            if compiled.rules.denies_path(path) {
                matched.insert(path.to_path_buf());
            }
        }
    }
    matched.into_iter().collect()
}

/// Pure bwrap argument builder, compiled on Linux and in tests across all hosts.
pub(crate) fn build_bwrap_args(
    policy: &ResolvedPolicy,
    cwd: &Path,
    command: &SandboxCommand,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let push = |args: &mut Vec<OsString>, values: &[&str]| {
        for v in values {
            args.push(OsString::from(v));
        }
    };

    // Start with a read-only view of the root filesystem (or read-write if `/`
    // itself is explicitly granted write via escalation).
    let root_writable = policy
        .write
        .iter()
        .any(|p| p == Path::new("/"));
    if root_writable {
        push(&mut args, &["--bind", "/", "/"]);
    } else {
        push(&mut args, &["--ro-bind", "/", "/"]);
    }
    push(
        &mut args,
        &[
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--unshare-pid",
            "--die-with-parent",
            "--new-session",
        ],
    );

    if policy.network == NetworkPolicy::Block {
        args.push(OsString::from("--unshare-net"));
    }

    // Bind explicit writable roots (workspace, temp dir, approved escalations).
    for root in &policy.write {
        if root == Path::new("/") || !root.exists() {
            continue;
        }
        args.push(OsString::from("--bind"));
        args.push(root.as_os_str().to_os_string());
        args.push(root.as_os_str().to_os_string());
    }

    // Re-bind protected workspace metadata read-only inside writable roots.
    for carve_out in &policy.carve_outs {
        if !carve_out.exists() {
            continue;
        }
        args.push(OsString::from("--ro-bind"));
        args.push(carve_out.as_os_str().to_os_string());
        args.push(carve_out.as_os_str().to_os_string());
    }

    // Mask denied literal paths and matched sensitive files/directories.
    let mut deny_paths: BTreeSet<PathBuf> = policy.deny.iter().cloned().collect();
    deny_paths.extend(scan_denied_paths(policy, cwd));
    for denied in deny_paths {
        if !denied.exists() {
            continue;
        }
        if denied.is_dir() {
            args.push(OsString::from("--tmpfs"));
            args.push(denied.as_os_str().to_os_string());
        } else {
            args.push(OsString::from("--ro-bind"));
            args.push(OsString::from("/dev/null"));
            args.push(denied.as_os_str().to_os_string());
        }
    }

    if cwd.is_dir() {
        args.push(OsString::from("--chdir"));
        args.push(cwd.as_os_str().to_os_string());
    }

    args.push(OsString::from("--"));
    args.push(command.program.clone());
    args.extend(command.args.iter().cloned());
    args
}

#[cfg(target_os = "linux")]
pub(crate) fn prepare_command(
    command: SandboxCommand,
    policy: &ResolvedPolicy,
    cwd: &Path,
) -> std::io::Result<SandboxCommand> {
    if policy.kind == SandboxKind::Unrestricted {
        return Ok(command);
    }
    let bwrap = probe().map_err(|detail| {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("command cannot be sandboxed: {detail}"),
        )
    })?;
    let args = build_bwrap_args(policy, cwd, &command);
    Ok(SandboxCommand {
        program: bwrap.into_os_string(),
        args,
        env: command.env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pig-linux-sandbox-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn as_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn bwrap_args_enforce_read_only_root_writable_workspace_carve_outs_and_denies() {
        let root = temp_dir("policy");
        let hooks_dir = root.join(".git").join("hooks");
        let secret_dir = root.join("secrets");
        let env_file = root.join(".env");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::write(&env_file, "TOKEN=secret").unwrap();

        let policy = ResolvedPolicy {
            kind: SandboxKind::Restricted,
            read: vec![PathBuf::from("/")],
            write: vec![root.clone()],
            carve_outs: vec![hooks_dir.clone()],
            deny: vec![secret_dir.clone(), env_file.clone()],
            deny_scan_roots: vec![],
            deny_groups: vec![],
            network: NetworkPolicy::Block,
            deny_sensitive_files: true,
        };
        let command = SandboxCommand::new(
            OsString::from("/bin/sh"),
            vec![OsString::from("-c"), OsString::from("echo ok")],
        );
        let args = as_strings(&build_bwrap_args(&policy, &root, &command));

        // Read-only root + namespaces + network isolation
        assert!(args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(args.contains(&"--unshare-pid".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        assert!(args.contains(&"--unshare-net".to_string()));

        // Workspace bound read-write, then carve-out re-bound read-only AFTER it
        let root_str = root.to_string_lossy().into_owned();
        let hooks_str = hooks_dir.to_string_lossy().into_owned();
        let bind_pos = args
            .windows(3)
            .position(|w| w == ["--bind", root_str.as_str(), root_str.as_str()])
            .expect("workspace --bind");
        let carve_pos = args
            .windows(3)
            .position(|w| w == ["--ro-bind", hooks_str.as_str(), hooks_str.as_str()])
            .expect("carve-out --ro-bind");
        assert!(
            carve_pos > bind_pos,
            "metadata carve-outs must come after writable root bind"
        );

        // Denied directory masked via --tmpfs; denied file masked via /dev/null
        let secret_str = secret_dir.to_string_lossy().into_owned();
        let env_str = env_file.to_string_lossy().into_owned();
        assert!(args.windows(2).any(|w| w == ["--tmpfs", secret_str.as_str()]));
        assert!(
            args.windows(3)
                .any(|w| w == ["--ro-bind", "/dev/null", env_str.as_str()])
        );

        // Original command preserved after `--`
        let sep = args.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(&args[sep + 1..], &["/bin/sh", "-c", "echo ok"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bwrap_args_omit_unshare_net_when_network_is_allowed() {
        let root = temp_dir("net-allow");
        let policy = ResolvedPolicy {
            kind: SandboxKind::Restricted,
            read: vec![PathBuf::from("/")],
            write: vec![],
            carve_outs: vec![],
            deny: vec![],
            deny_scan_roots: vec![],
            deny_groups: vec![],
            network: NetworkPolicy::Allow,
            deny_sensitive_files: false,
        };
        let command = SandboxCommand::new(OsString::from("ls"), vec![]);
        let args = as_strings(&build_bwrap_args(&policy, &root, &command));
        assert!(!args.contains(&"--unshare-net".to_string()));
        assert!(args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        let _ = std::fs::remove_dir_all(&root);
    }
}
