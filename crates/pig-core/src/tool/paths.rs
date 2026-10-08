use super::*;

/// Core of resolve_checked: pure path resolution + boundary/symlink checks, returning the out-of-bounds shape in a structured form.
fn resolve_core(cwd: &Path, path: &str, create_parents: bool) -> Result<PathBuf, BoundFailure> {
    let raw = Path::new(path);
    let full = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let cwd_canonical = cwd.canonicalize().map_err(|e| {
        BoundFailure::Other(format!("Invalid working directory {}: {e}", cwd.display()))
    })?;
    let parent = full.parent().unwrap_or(cwd);
    if create_parents {
        std::fs::create_dir_all(parent).map_err(|e| {
            BoundFailure::Other(format!(
                "Failed to create directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    let parent_canonical = parent.canonicalize().map_err(|e| {
        BoundFailure::Other(format!(
            "Directory does not exist {}: {e}",
            parent.display()
        ))
    })?;
    if !parent_canonical.starts_with(&cwd_canonical) {
        let file_name = full
            .file_name()
            .ok_or_else(|| BoundFailure::Other(format!("Invalid path: {path}")))?;
        return Err(BoundFailure::ParentOutside {
            resolved: parent_canonical.join(file_name),
        });
    }
    let file_name = full
        .file_name()
        .ok_or_else(|| BoundFailure::Other(format!("Invalid path: {path}")))?;
    let resolved = parent_canonical.join(file_name);
    let is_symlink = std::fs::symlink_metadata(&resolved)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink || resolved.exists() {
        match resolved.canonicalize() {
            Ok(full_canonical) => {
                if !full_canonical.starts_with(&cwd_canonical) {
                    return Err(BoundFailure::TargetOutside { resolved });
                }
            }
            Err(_) if is_symlink => return Err(BoundFailure::DanglingSymlink),
            Err(e) => {
                return Err(BoundFailure::Other(format!(
                    "Invalid path {}: {e}",
                    resolved.display()
                )));
            }
        }
    }
    Ok(resolved)
}

/// Resolve a path relative to cwd and prevent escaping the working directory.
/// Beyond canonicalizing the parent directory, the target itself is checked for symlinks: targets pointing outside the workspace are rejected;
/// dangling links (target does not exist, cannot canonicalize) are rejected fail-closed — otherwise Write would
/// follow the link and create a file outside the workspace.
pub fn resolve_checked(cwd: &Path, path: &str, create_parents: bool) -> Result<PathBuf, String> {
    resolve_core(cwd, path, create_parents).map_err(|failure| match failure {
        BoundFailure::ParentOutside { .. } => format!("Path escapes the working directory: {path}"),
        BoundFailure::TargetOutside { .. } => {
            format!("Path escapes the working directory (symlink): {path}")
        }
        BoundFailure::DanglingSymlink => {
            format!(
                "Symlink points to a nonexistent target; cannot confirm safety, refused: {path}"
            )
        }
        BoundFailure::Other(message) => message,
    })
}

/// kimi writesOnlyPlanFile: whether a Write/Edit target path lands in the plans directory
/// (an `.md` file inside `.pigcode/plans/`; the prompt steers the model to write `plan-<session_id>.md`,
/// which matches naturally — the check does not pin the file name). Pure lexical-normalized comparison (no filesystem access — the plans
/// directory may not exist yet, and resolve_checked's parent canonicalize would fail); both relative and absolute
/// paths are accepted; comparing absolute paths through a symlinked cwd is not supported yet (an accepted boundary)
pub fn is_plan_file_write(cwd: &Path, arguments: &str) -> bool {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let Some(path) = args["path"].as_str() else {
        return false;
    };
    let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let raw = Path::new(path);
    let full = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd_canonical.join(raw)
    };
    // The check is relaxed to any md file inside the plans directory (no pinned file name: kimi's slug mechanism likewise
    // means "the plans directory is writable"; the prompt-guided plan-<sid>.md matches naturally)
    let plans_dir = cwd_canonical.join(".pigcode/plans");
    let full = lexical_normalize(&full);
    full.starts_with(lexical_normalize(&plans_dir))
        && full.extension().is_some_and(|ext| ext == "md")
}

/// Lexical normalization (drop `.`, pop one level for `..`; no symlink resolution, no filesystem access)
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Outside-workspace read/write policy: Read consults the fs_read_outside toggle, Write consults fs_write_outside
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FsAccess {
    Read,
    Write,
}

/// Whether a path falls under the system tmp (macOS /tmp→/private/tmp; both sides canonicalized before comparing).
/// Applies only to **absolute-path** requests (a relative ../ escape landing in tmp still counts as out-of-bounds, preventing
/// directories next to a tempdir workspace from becoming a backdoor).
fn is_tmp_absolute_path(raw: &str) -> bool {
    let raw_path = Path::new(raw);
    if !raw_path.is_absolute() {
        return false;
    }
    // Canonical comparison: the requested path may not exist (a Write of a new file), so fall back to canonicalizing the parent directory
    let candidate = raw_path
        .canonicalize()
        .or_else(|_| raw_path.parent().unwrap_or(raw_path).canonicalize())
        .unwrap_or_else(|_| raw_path.to_path_buf());
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(tmp) = std::env::temp_dir().canonicalize() {
        roots.push(tmp);
    }
    if let Ok(tmp) = Path::new("/tmp").canonicalize() {
        roots.push(tmp);
    }
    roots.iter().any(|root| candidate.starts_with(root))
}

/// Policy entry point of resolve_checked (all file tools go through here): paths inside the workspace and explicit tmp absolute paths
/// are always allowed; other outside-workspace paths follow the session toggles; a per-call approval grant (the gate sets it after an
/// approved out-of-workspace request) lets this single call through. Dangling links are always rejected.
/// The sensitive-file check (is_sensitive_file) is performed unconditionally by the tool after resolve, unaffected by the toggles.
pub fn resolve_with_access(
    ctx: &ToolContext<'_>,
    path: &str,
    create_parents: bool,
    access: FsAccess,
) -> Result<PathBuf, String> {
    let (cwd, state) = (ctx.cwd, ctx.state);
    match resolve_core(cwd, path, create_parents) {
        Ok(resolved) => Ok(resolved),
        Err(BoundFailure::Other(message)) => Err(message),
        Err(BoundFailure::DanglingSymlink) => Err(format!(
            "Symlink points to a nonexistent target; cannot confirm safety, refused: {path}"
        )),
        Err(
            BoundFailure::ParentOutside { resolved } | BoundFailure::TargetOutside { resolved },
        ) => {
            if is_tmp_absolute_path(path) {
                return Ok(resolved);
            }
            // Extra read-only roots (data_dir/sessions: subagent result.md/context records) are always readable —
            // an exemption parallel to tmp, likewise effective only for absolute-path requests (relative ../ escapes do not count);
            // Read is allowed, Write is not. resolved is already the path determined by resolve_core
            // (canonical parent + file name), so compare prefixes directly against the construction-time canonicalized roots.
            // The sensitive-file check (is_sensitive_file) still runs in the tool after resolve, unaffected by the
            // exemption; config.toml sits at the data_dir root, not under sessions/, so it is naturally excluded.
            if access == FsAccess::Read
                && Path::new(path).is_absolute()
                && state
                    .extra_read_roots
                    .iter()
                    .any(|root| resolved.starts_with(root))
            {
                return Ok(resolved);
            }
            let allowed = match access {
                FsAccess::Read => state.fs_read_outside.load(Ordering::Relaxed),
                FsAccess::Write => state.fs_write_outside.load(Ordering::Relaxed),
            } || ctx.fs_grant == Some(access);
            if allowed {
                // Same gate: with the toggle on, symlinks pointing outside the workspace are also allowed
                Ok(resolved)
            } else {
                let toggle = match access {
                    FsAccess::Read => "outside-workspace read access",
                    FsAccess::Write => "outside-workspace write access",
                };
                Err(format!(
                    "Path escapes the working directory: {path} ({toggle} needs the user's approval — they can enable it in the input box's mode menu or approve it when asked)"
                ))
            }
        }
    }
}

/// Which tools touch the filesystem and in which mode, for the out-of-workspace
/// approval (Bash is deliberately absent — the shell is not path-bound and has
/// its own dangerous-command gate).
pub fn fs_intent_of_tool(name: &str) -> Option<FsAccess> {
    match name {
        "Read" | "Glob" | "Grep" | "ReadMediaFile" => Some(FsAccess::Read),
        "Write" | "Edit" => Some(FsAccess::Write),
        _ => None,
    }
}

/// Gate-side out-of-workspace pre-check (no side effects: parents are never
/// created, nothing is touched): Some((access, path)) when this call targets
/// outside the workspace, is not exempt (tmp / extra read roots), and the
/// session toggle is off — the gate turns that into an approval request
/// instead of the hard error resolve_with_access used to return. Mirrors
/// resolve_with_access's classification: resolve_core runs with
/// create_parents=false, and its "directory does not exist" outcome falls back
/// to a lexical check so a Write into a not-yet-existing outside directory
/// also pops.
pub fn fs_outside_intent(
    state: &SessionToolState,
    cwd: &Path,
    tool: &str,
    arguments: &str,
) -> Option<(FsAccess, String)> {
    let access = fs_intent_of_tool(tool)?;
    let path = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get("path")?
        .as_str()?
        .to_string();
    let toggle = match access {
        FsAccess::Read => state.fs_read_outside.load(Ordering::Relaxed),
        FsAccess::Write => state.fs_write_outside.load(Ordering::Relaxed),
    };
    if toggle {
        return None;
    }
    let blocked = match resolve_core(cwd, &path, false) {
        Ok(_) => None,
        Err(
            BoundFailure::ParentOutside { resolved } | BoundFailure::TargetOutside { resolved },
        ) => Some(resolved),
        // Always rejected regardless of toggles; no popup
        Err(BoundFailure::DanglingSymlink) => return None,
        Err(BoundFailure::Other(message)) => {
            // A Write into a not-yet-existing directory: lexical fallback so
            // new outside directories still trigger the request
            if !message.starts_with("Directory does not exist") {
                return None;
            }
            let cwd_can = cwd.canonicalize().ok()?;
            let raw = Path::new(&path);
            let full = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                cwd_can.join(raw)
            };
            let full = lexical_normalize(&full);
            (!full.starts_with(&cwd_can)).then_some(full)
        }
    }?;
    if is_tmp_absolute_path(&path) {
        return None;
    }
    if access == FsAccess::Read
        && Path::new(&path).is_absolute()
        && state
            .extra_read_roots
            .iter()
            .any(|root| blocked.starts_with(root))
    {
        return None;
    }
    Some((access, path))
}

/// Sensitive-file check (case-insensitive, file name only): .env family / SSH private keys / cloud credentials.
pub fn is_sensitive_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    // .env and .env.* (template variants exempted)
    if (lower == ".env" || lower.starts_with(".env."))
        && !matches!(
            lower.as_str(),
            ".env.example" | ".env.sample" | ".env.template"
        )
    {
        return true;
    }
    // SSH private keys: exact names or id_xxx[-_.] variants; .pub public keys are always exempted
    if !lower.ends_with(".pub") {
        for prefix in ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"] {
            if lower == prefix {
                return true;
            }
            if let Some(rest) = lower.strip_prefix(prefix)
                && rest
                    .chars()
                    .next()
                    .is_some_and(|c| matches!(c, '-' | '_' | '.'))
            {
                return true;
            }
        }
    }
    // Cloud credentials: ~/.aws/credentials, ~/.gcp/credentials
    if lower == "credentials" {
        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_ascii_lowercase);
        if parent.as_deref() == Some(".aws") || parent.as_deref() == Some(".gcp") {
            return true;
        }
    }
    false
}

/// Sensitive-file rejection message (shared by Read/Write/Edit)
pub(crate) fn sensitive_file_error(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    format!(
        "Refused access to sensitive file: {name} (.env / private keys / cloud credentials never enter the model context)"
    )
}
