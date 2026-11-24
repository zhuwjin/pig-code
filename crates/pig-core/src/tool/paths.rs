use super::*;

/// Core of resolve_checked: pure path resolution + boundary/symlink checks, returning the out-of-bounds shape in a structured form.
fn resolve_core(cwd: &Path, path: &str, create_parents: bool) -> Result<PathBuf, BoundFailure> {
    resolve_core_with_fs(&crate::task::LocalFileSystem, cwd, path, create_parents)
}

fn resolve_core_with_fs(
    fs: &dyn crate::task::ExecutorFileSystem,
    cwd: &Path,
    path: &str,
    create_parents: bool,
) -> Result<PathBuf, BoundFailure> {
    let raw = Path::new(path);
    let full = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let cwd_canonical = fs.canonicalize(cwd).map_err(|e| {
        BoundFailure::Other(format!("Invalid working directory {}: {e}", cwd.display()))
    })?;
    let parent = full.parent().unwrap_or(cwd);
    if create_parents {
        fs.create_dir_all(parent).map_err(|e| {
            BoundFailure::Other(format!(
                "Failed to create directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    let parent_canonical = fs.canonicalize(parent).map_err(|e| {
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
    let is_symlink = fs
        .symlink_metadata(&resolved)
        .map(|m| m.is_symlink)
        .unwrap_or(false);
    if is_symlink || fs.exists(&resolved) {
        match fs.canonicalize(&resolved) {
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

/// Canonical final object of a **read**.
///
/// `resolve_core` deliberately returns the model-spelled final component so a
/// write through a symlink keeps operating on the link entry. Reads must not do
/// that: the sensitive-name, deny-rule and credential checks have to look at
/// the object actually opened, otherwise a link called `notes` could carry
/// `.env` or a credential file past every veto, and an approval granted for a
/// link name would not describe the target. `fs.canonicalize` resolves a
/// final symlink; a missing non-link path is returned as-is so the caller can
/// report "File not found" exactly as before.
fn canonical_read_target_with_fs(
    fs: &dyn crate::task::ExecutorFileSystem,
    resolved: PathBuf,
    raw_path: &str,
) -> Result<PathBuf, String> {
    match fs.canonicalize(&resolved) {
        Ok(target) => Ok(target),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let is_link = fs
                .symlink_metadata(&resolved)
                .map(|meta| meta.is_symlink)
                .unwrap_or(false);
            if is_link {
                Err(format!(
                    "Symlink points to a nonexistent target; cannot confirm safety, refused: {raw_path}"
                ))
            } else {
                Ok(resolved)
            }
        }
        Err(error) => Err(format!("Failed to resolve {}: {error}", resolved.display())),
    }
}

/// Refuse a `Write`/`Edit` whose **read-through** target is denied.
///
/// The write itself replaces the named entry (a rename over a final symlink),
/// so authorization is decided on that spelling. But both tools read the file
/// before writing it (encoding probe, edit match, change diff), and that read
/// follows a final symlink. The read target must therefore pass the same
/// sensitive-name and deny-rule vetoes as `Read`; otherwise a link repointed
/// after the read could feed a denied file's content into the diff.
///
/// No approval lifts this: a write approval names the visible entry, not a
/// target hidden behind it (the dedicated single-file credential approval is
/// handled by the credential checks before this point and never reads content).
pub(crate) fn read_through_rejection(ctx: &ToolContext<'_>, full: &Path) -> Option<String> {
    let target = ctx.state.env.fs.canonicalize(full).ok()?;
    if target == *full {
        return None;
    }
    if let Some(message) = crate::task::sandbox_read_denial(ctx.state, ctx.cwd, &target) {
        return Some(message);
    }
    if is_sensitive_file(&target) {
        return Some(sensitive_file_error(&target));
    }
    None
}

/// The lexical entry a Write/Edit acts on for an already-read spelling: the
/// canonical parent plus the model-spelled final component. A final symlink
/// stays a link (the write is a rename over that entry), while a symlinked
/// parent resolves to the real directory. Used to key what a Read showed when
/// it opened the canonical target, so a later Read of that same spelling can
/// answer "file unchanged".
pub(crate) fn write_spelling(
    fs: &dyn crate::task::ExecutorFileSystem,
    cwd: &Path,
    path: &str,
) -> Option<PathBuf> {
    resolve_core_with_fs(fs, cwd, path, false).ok()
}

/// The object a read-tool path names, for the approval gate's credential
/// classification. Follows a final symlink (and a symlinked parent) like the
/// tool does; falls back to the lexical path when the target does not exist.
/// Used for `Read`/`ReadMediaFile`/`Grep`/`Glob` only — `Write`/`Edit`
/// intentionally keep acting on the spelling the model named (their write is a
/// rename over that entry, not a write through it).
pub(crate) fn canonical_read_target_for_gate(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    })
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
    is_plan_file_path(cwd, path)
}

/// The plan-directory exemption for a raw `path` argument (shared by the gate
/// and the file tools).
pub fn is_plan_file_path(cwd: &Path, path: &str) -> bool {
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

/// Whether this exact path was approved for this one call (the credential
/// single-file approval). The dialog names the file, so the approval lifts
/// both the deny-rule veto and the sensitive-name refusal for exactly it.
pub fn approved_this_call(ctx: &ToolContext<'_>, target: &Path) -> bool {
    let permissions = &ctx.additional_permissions;
    permissions
        .read_roots_list()
        .into_iter()
        .chain(permissions.write_roots_list())
        .any(|root| path_equal(&root, target))
}

/// Refuse a file-tool read the session's sandbox rules deny.
///
/// Same veto philosophy as `metadata_write_rejection`: it only ever subtracts,
/// so the kernel remains the authority on what is allowed. Without it the model
/// shell's denies could be bypassed by asking Read instead of `cat`.
pub fn sandbox_read_rejection(
    ctx: &ToolContext<'_>,
    raw_path: &str,
    target: &Path,
) -> Option<String> {
    let _ = raw_path;
    // An approved per-file read for this one call lifts the veto: this is how a
    // credential file is read.
    if approved_this_call(ctx, target) {
        return None;
    }
    crate::task::sandbox_read_denial(ctx.state, ctx.cwd, target)
}

/// Refuse a file-tool write the session's sandbox rules deny.
///
/// The write counterpart of [`sandbox_read_rejection`]: without it an approved
/// out-of-workspace write would silently override a deny rule the model shell
/// must obey. The credential single-file approval names exactly that file and
/// lifts the veto for this call only.
pub fn sandbox_write_rejection(ctx: &ToolContext<'_>, target: &Path) -> Option<String> {
    if approved_this_call(ctx, target) {
        return None;
    }
    crate::task::sandbox_read_denial(ctx.state, ctx.cwd, target)
}

/// Refuse a file-tool write into workspace metadata.
///
/// The sandbox already makes these paths read-only for the model shell, but
/// Write/Edit do not run under it; without this check the model could write
/// `.pigcode/hooks.toml` or `.git/hooks/pre-commit` and get code execution with
/// the user's rights on the next launch. This is a **veto only**: it can refuse
/// a path the tools would otherwise accept and never widens one, so it stays
/// correct even where the kernel is stricter than the policy (see
/// docs/WINDOWS_SANDBOX.md).
///
/// `.pigcode/plans` is the deliberate exception — the plan-mode prompt tells the
/// model to write there, and nothing under it is executed.
pub fn metadata_write_rejection(
    cwd: &Path,
    state: &crate::task::SessionToolState,
    raw_path: &str,
    target: &Path,
) -> Option<String> {
    if is_plan_file_path(cwd, raw_path) {
        return None;
    }
    let mut roots = vec![cwd.to_path_buf()];
    roots.extend(state.extra_rw_roots.iter().cloned());
    let blocked = crate::task::metadata_carve_outs(&roots, cwd)
        .into_iter()
        .find(|carve| path_is_under(target, carve))?;
    Some(format!(
        "Refusing to write workspace metadata: {} is inside {}, which is read-only to the model. \
         Those paths are consumed outside the sandbox (hooks, MCP servers, git hooks and config, \
         prompt files), so writing them is code execution with the user's rights on the next \
         launch. Ask the user to change it instead.",
        target.display(),
        blocked.display()
    ))
}

/// Whether two paths name the same file, ignoring the Windows verbatim prefix
/// and case (the spellings a canonicalized tool path and a `dunce`-canonicalized
/// grant produce differ).
pub(crate) fn path_equal(left: &Path, right: &Path) -> bool {
    path_is_under(left, right) && path_is_under(right, left)
}

/// Whether `path` is `root` or below it.
///
/// Comparison is textual on purpose: carve-outs are canonicalized with `dunce`
/// (which strips the Windows verbatim prefix) while a resolved tool target keeps
/// the prefix `std::fs::canonicalize` produces, so both the prefix and the case
/// are normalized here instead of assuming one spelling.
pub(crate) fn path_is_under(path: &Path, root: &Path) -> bool {
    let normalize = |value: &Path| {
        let text = value
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let text = text.strip_prefix("//?/").unwrap_or(&text).to_string();
        #[cfg(target_os = "windows")]
        {
            text.to_ascii_lowercase()
        }
        #[cfg(not(target_os = "windows"))]
        {
            text
        }
    };
    let path = normalize(path);
    let root = normalize(root);
    let root = root.trim_end_matches('/');
    path == root || path.starts_with(&format!("{root}/"))
}

/// `lexical_normalize` for callers outside this module (the gate classifies a
/// Write/Edit target before the tool runs).
pub fn lexical_normalize_public(path: &Path) -> PathBuf {
    lexical_normalize(path)
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

/// Outside-workspace read/write policy.
///
/// Follows Codex: **reads are allowed anywhere** (the sandbox grants full-disk
/// read), while **writes** are limited to the workspace, tmp and the extra
/// roots; a write outside needs a per-call approval (`ctx.fs_grant`). There is
/// no persistent switch — a workspace boundary is a per-action decision, not a
/// session setting.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FsAccess {
    Read,
    Write,
}

/// Whether a path falls under the system tmp (macOS /tmp→/private/tmp; both sides canonicalized before comparing).
/// Applies only to **absolute-path** requests (a relative ../ escape landing in tmp still counts as out-of-bounds, preventing
/// directories next to a tempdir workspace from becoming a backdoor).
pub(super) fn is_tmp_absolute_path(raw: &str) -> bool {
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

/// Policy entry point of resolve_checked (all file tools go through here).
///
/// Reads resolve anywhere: Codex's sandbox grants full-disk read, and refusing
/// paths the model can be pointed at only produces hard failures it cannot
/// recover from. Writes stay inside the workspace, tmp and the extra roots; a
/// write outside is allowed for exactly one call when the approval gate set
/// the matching `fs_grant`. The sensitive-file check (`is_sensitive_file`) is
/// performed unconditionally by the tool after resolve. Dangling links are
/// always rejected.
pub fn resolve_with_access(
    ctx: &ToolContext<'_>,
    path: &str,
    create_parents: bool,
    access: FsAccess,
) -> Result<PathBuf, String> {
    let (cwd, state) = (ctx.cwd, ctx.state);
    let fs = state.env.fs.as_ref();
    let resolved = match resolve_core_with_fs(fs, cwd, path, create_parents) {
        Ok(resolved) => resolved,
        Err(BoundFailure::Other(message)) => return Err(message),
        Err(BoundFailure::DanglingSymlink) => {
            return Err(format!(
                "Symlink points to a nonexistent target; cannot confirm safety, refused: {path}"
            ));
        }
        Err(
            BoundFailure::ParentOutside { resolved } | BoundFailure::TargetOutside { resolved },
        ) => {
            if access == FsAccess::Read {
                // Full-disk read: outside the workspace is not a boundary.
                return canonical_read_target_with_fs(fs, resolved, path);
            }
            if state.env.is_local() && is_tmp_absolute_path(path) {
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
                return canonical_read_target_with_fs(fs, resolved, path);
            }
            // Read-write roots (the workspace memory directory while memory
            // is on): the model manages its memory files with the plain
            // Write/Edit tools, so both directions pass approval-free; same
            // absolute-path-only rule as above. Sensitive-file filtering
            // still applies after resolve (memory file names never match it)
            if Path::new(path).is_absolute()
                && state
                    .extra_rw_roots
                    .iter()
                    .any(|root| resolved.starts_with(root))
            {
                return Ok(resolved);
            }
            if ctx.fs_grant == Some(access) {
                // The approval gate granted exactly this call's outside write.
                return Ok(resolved);
            }
            return Err(format!(
                "Path escapes the working directory: {path} (writing outside the workspace needs the user's approval; ask them to approve this call)"
            ));
        }
    };
    if access == FsAccess::Read {
        // The checks that follow (sensitive name, deny rules, credential veto,
        // one-call approval) must name the object that is actually opened.
        canonical_read_target_with_fs(fs, resolved, path)
    } else {
        Ok(resolved)
    }
}

/// Which tools may write outside the workspace for a per-call approval (Bash is
/// deliberately absent — the shell is not path-bound and has its own
/// dangerous-command gate). Reads are never gated: full-disk read is the
/// sandbox policy.
pub fn fs_intent_of_tool(name: &str) -> Option<FsAccess> {
    match name {
        "Write" | "Edit" | APPLY_PATCH_TOOL => Some(FsAccess::Write),
        _ => None,
    }
}

fn check_single_path_outside(
    state: &SessionToolState,
    cwd: &Path,
    path: &str,
) -> Option<String> {
    let fs = state.env.fs.as_ref();
    let blocked = match resolve_core_with_fs(fs, cwd, path, false) {
        Ok(_) => None,
        Err(
            BoundFailure::ParentOutside { resolved } | BoundFailure::TargetOutside { resolved },
        ) => Some(resolved),
        // Always rejected regardless of policy; no popup
        Err(BoundFailure::DanglingSymlink) => return None,
        Err(BoundFailure::Other(message)) => {
            // A Write into a not-yet-existing directory: lexical fallback so
            // new outside directories still trigger the request
            if !message.starts_with("Directory does not exist") {
                return None;
            }
            let cwd_can = fs.canonicalize(cwd).ok()?;
            let raw = Path::new(path);
            let full = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                cwd_can.join(raw)
            };
            let full = lexical_normalize(&full);
            (!full.starts_with(&cwd_can)).then_some(full)
        }
    }?;
    if state.env.is_local() && is_tmp_absolute_path(path) {
        return None;
    }
    // Read-write roots (workspace memory): no approval request either — the
    // resolve-side exemption lets the call through without a grant
    if Path::new(path).is_absolute()
        && state
            .extra_rw_roots
            .iter()
            .any(|root| blocked.starts_with(root))
    {
        return None;
    }
    Some(blocked.display().to_string())
}

/// Gate-side out-of-workspace write pre-check (no side effects: parents are
/// never created, nothing is touched): Some((access, path)) when a Write/Edit/
/// ApplyPatch targets outside the workspace and is not exempt (tmp / extra roots).
pub fn fs_outside_intent(
    state: &SessionToolState,
    cwd: &Path,
    tool: &str,
    arguments: &str,
) -> Option<(FsAccess, String)> {
    let access = fs_intent_of_tool(tool)?;
    if tool == APPLY_PATCH_TOOL {
        for path in patch_target_paths(arguments) {
            if let Some(outside) = check_single_path_outside(state, cwd, &path) {
                return Some((access, outside));
            }
        }
        return None;
    }
    let path = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get("path")?
        .as_str()?
        .to_string();
    check_single_path_outside(state, cwd, &path).map(|outside| (access, outside))
}

/// Sensitive-file check (case-insensitive, file name only): .env family / SSH private keys / cloud credentials.
///
/// On Windows a file name may carry an alternate data stream suffix
/// (`notes.txt:secret`). The stream is a different object on disk but lives on
/// the same file name; checking only the full name would let `.env:set` or
/// `id_rsa:key` past the veto while the stream still carries the credential.
pub fn is_sensitive_file(path: &Path) -> bool {
    // A stream belongs to its base file: decide on the base name so the
    // suffix can neither hide a protected name nor defeat a template
    // exception (`.env.example:readable` stays readable).
    #[cfg(target_os = "windows")]
    if let Some(base) = windows_base_path(path) {
        return is_sensitive_name(&base);
    }
    is_sensitive_name(path)
}

fn is_sensitive_name(path: &Path) -> bool {
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

/// The file-name portion of a Windows alternate-data-stream path
/// (`dir/.env:secret` → `dir/.env`). `None` when there is no stream suffix or
/// the name is not Unicode.
#[cfg(target_os = "windows")]
pub(crate) fn windows_base_path(path: &Path) -> Option<std::path::PathBuf> {
    let name = path.file_name()?.to_str()?;
    let (base, stream) = name.split_once(':')?;
    if base.is_empty() || stream.is_empty() {
        return None;
    }
    Some(path.with_file_name(base))
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

#[cfg(all(test, target_os = "windows"))]
mod ads_tests {
    use super::*;

    #[test]
    fn an_alternate_data_stream_keeps_the_sensitive_base_name() {
        for path in [
            r"C:\work\.env:secret",
            r"C:\work\.env.local:stream",
            r"C:\work\id_rsa:key",
            r"C:\Users\u\.aws\credentials:data",
        ] {
            assert!(is_sensitive_file(Path::new(path)), "{path} must be refused");
        }
        assert!(!is_sensitive_file(Path::new(
            r"C:\work\.env.example:readable"
        )));
        assert!(!is_sensitive_file(Path::new(r"C:\work\notes.txt:stream")));
    }
}
