//! Multi-file, multi-hunk atomic patch tool (`ApplyPatch`), modeled on Codex's
//! `apply_patch` grammar.
//!
//! Grammar:
//! ```text
//! *** Begin Patch
//! *** Add File: <path>
//! +<line>
//! *** Update File: <path>
//! [*** Move to: <new_path>]
//! @@ [optional context anchor]
//!  <context line>
//! -<removed line>
//! +<added line>
//! [*** End of File]
//! *** Delete File: <path>
//! *** End Patch
//! ```
//!
//! All paths, policy gates, and hunk matches are validated in
//! memory before any file is written or removed on disk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use pig_utils::text::{FileEncoding, LineEnding};

use super::*;

pub const APPLY_PATCH_TOOL: &str = "ApplyPatch";

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File:";
const DELETE_FILE: &str = "*** Delete File:";
const UPDATE_FILE: &str = "*** Update File:";
const MOVE_TO: &str = "*** Move to:";
const END_OF_FILE: &str = "*** End of File";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchHunk {
    AddFile {
        path: String,
        content: String,
    },
    DeleteFile {
        path: String,
    },
    UpdateFile {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateChunk {
    /// Optional header after `@@` used as a context anchor.
    pub change_context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub is_end_of_file: bool,
}

/// Extract all target paths (source and move destinations) named by an
/// `ApplyPatch` tool call's JSON arguments without touching the filesystem.
pub fn patch_target_paths(arguments: &str) -> Vec<String> {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return Vec::new();
    };
    let Some(patch) = args["patch"].as_str() else {
        return Vec::new();
    };
    let Ok(hunks) = parse_patch(patch) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for hunk in hunks {
        match hunk {
            PatchHunk::AddFile { path, .. } | PatchHunk::DeleteFile { path } => {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
            PatchHunk::UpdateFile {
                path, move_path, ..
            } => {
                if !paths.contains(&path) {
                    paths.push(path);
                }
                if let Some(dest) = move_path
                    && !paths.contains(&dest)
                {
                    paths.push(dest);
                }
            }
        }
    }
    paths
}

/// Parse Codex-style patch text into structured file hunks.
pub fn parse_patch(raw: &str) -> Result<Vec<PatchHunk>, String> {
    let normalized = raw.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.trim().lines().collect();
    if lines.len() < 2 {
        return Err(
            "Invalid patch: must start with `*** Begin Patch` and end with `*** End Patch`"
                .to_string(),
        );
    }
    if lines[0].trim() != BEGIN_PATCH {
        return Err(format!(
            "Invalid patch: first line must be `{BEGIN_PATCH}`, got `{}`",
            lines[0]
        ));
    }
    if lines[lines.len() - 1].trim() != END_PATCH {
        return Err(format!(
            "Invalid patch: last line must be `{END_PATCH}`, got `{}`",
            lines[lines.len() - 1]
        ));
    }

    let body = &lines[1..lines.len() - 1];
    let mut index = 0usize;
    let mut hunks = Vec::new();
    let mut touched = HashSet::new();

    while index < body.len() {
        let header = body[index].trim();
        if header.is_empty() {
            index += 1;
            continue;
        }
        if let Some(rest) = header.strip_prefix(ADD_FILE) {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err("Invalid patch: `*** Add File:` requires a path".to_string());
            }
            if !touched.insert(path.clone()) {
                return Err(format!(
                    "Invalid patch: path `{path}` appears multiple times in the same patch"
                ));
            }
            index += 1;
            let mut added_lines = Vec::new();
            while index < body.len() && !is_file_header(body[index]) {
                let line = body[index];
                let Some(content_line) = line.strip_prefix('+') else {
                    return Err(format!(
                        "Invalid Add File line for `{path}`: every content line must start with `+`, got `{line}`"
                    ));
                };
                added_lines.push(content_line);
                index += 1;
            }
            let content = if added_lines.is_empty() {
                String::new()
            } else {
                format!("{}\n", added_lines.join("\n"))
            };
            hunks.push(PatchHunk::AddFile { path, content });
        } else if let Some(rest) = header.strip_prefix(DELETE_FILE) {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err("Invalid patch: `*** Delete File:` requires a path".to_string());
            }
            if !touched.insert(path.clone()) {
                return Err(format!(
                    "Invalid patch: path `{path}` appears multiple times in the same patch"
                ));
            }
            index += 1;
            hunks.push(PatchHunk::DeleteFile { path });
        } else if let Some(rest) = header.strip_prefix(UPDATE_FILE) {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err("Invalid patch: `*** Update File:` requires a path".to_string());
            }
            if !touched.insert(path.clone()) {
                return Err(format!(
                    "Invalid patch: path `{path}` appears multiple times in the same patch"
                ));
            }
            index += 1;
            let move_path = if index < body.len()
                && let Some(dest) = body[index].trim().strip_prefix(MOVE_TO)
            {
                let dest = dest.trim().to_string();
                if dest.is_empty() {
                    return Err(format!(
                        "Invalid patch: `*** Move to:` for `{path}` requires a destination path"
                    ));
                }
                if !touched.insert(dest.clone()) {
                    return Err(format!(
                        "Invalid patch: destination path `{dest}` conflicts with another hunk"
                    ));
                }
                index += 1;
                Some(dest)
            } else {
                None
            };

            let mut chunks = Vec::new();
            while index < body.len() && !is_file_header(body[index]) {
                let line = body[index];
                if line.trim().is_empty() && chunks.is_empty() {
                    index += 1;
                    continue;
                }
                let change_context = if line == "@@" {
                    index += 1;
                    None
                } else if let Some(ctx) = line.strip_prefix("@@ ") {
                    index += 1;
                    let trimmed = ctx.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                } else if chunks.is_empty() {
                    // Allow implicit first chunk when the model omits `@@`.
                    None
                } else {
                    return Err(format!(
                        "Invalid Update File hunk for `{path}`: expected `@@`, got `{line}`"
                    ));
                };

                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut is_end_of_file = false;
                let mut parsed_any = false;

                while index < body.len() && !is_file_header(body[index]) {
                    let current = body[index];
                    if current == "@@" || current.starts_with("@@ ") {
                        break;
                    }
                    if current.trim() == END_OF_FILE {
                        is_end_of_file = true;
                        index += 1;
                        break;
                    }
                    if let Some(rest) = current.strip_prefix(' ') {
                        old_lines.push(rest.to_string());
                        new_lines.push(rest.to_string());
                        parsed_any = true;
                    } else if let Some(rest) = current.strip_prefix('-') {
                        old_lines.push(rest.to_string());
                        parsed_any = true;
                    } else if let Some(rest) = current.strip_prefix('+') {
                        new_lines.push(rest.to_string());
                        parsed_any = true;
                    } else if current.is_empty() {
                        old_lines.push(String::new());
                        new_lines.push(String::new());
                        parsed_any = true;
                    } else {
                        return Err(format!(
                            "Invalid hunk line in `{path}`: lines must start with ` `, `+`, or `-`, got `{current}`"
                        ));
                    }
                    index += 1;
                }

                if !parsed_any {
                    return Err(format!(
                        "Invalid Update File hunk for `{path}`: empty `@@` chunk"
                    ));
                }
                chunks.push(UpdateChunk {
                    change_context,
                    old_lines,
                    new_lines,
                    is_end_of_file,
                });
            }

            if chunks.is_empty() && move_path.is_none() {
                return Err(format!(
                    "Invalid Update File hunk for `{path}`: no changes or move specified"
                ));
            }
            hunks.push(PatchHunk::UpdateFile {
                path,
                move_path,
                chunks,
            });
        } else {
            return Err(format!(
                "Invalid patch header `{header}`: expected `{ADD_FILE}`, `{UPDATE_FILE}`, or `{DELETE_FILE}`"
            ));
        }
    }

    if hunks.is_empty() {
        return Err("Invalid patch: patch contains no file operations".to_string());
    }
    Ok(hunks)
}

fn is_file_header(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with(ADD_FILE)
        || trimmed.starts_with(DELETE_FILE)
        || trimmed.starts_with(UPDATE_FILE)
}

/// Apply ordered update chunks to LF-normalized text, returning the updated LF
/// text. Chunks are located sequentially using 4-tier fuzzy matching.
pub fn apply_update_chunks(
    path: &str,
    original_lf: &str,
    chunks: &[UpdateChunk],
) -> Result<String, String> {
    if chunks.is_empty() {
        return Ok(original_lf.to_string());
    }
    let had_trailing_newline = original_lf.ends_with('\n');
    let mut lines: Vec<String> = if original_lf.is_empty() {
        Vec::new()
    } else {
        original_lf.lines().map(str::to_string).collect()
    };

    // Compute all replacement ranges against the original line list first so
    // chunks never interfere with each other's indices, then apply in reverse.
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::with_capacity(chunks.len());
    let mut search_from = 0usize;

    for (chunk_ix, chunk) in chunks.iter().enumerate() {
        if let Some(context) = &chunk.change_context {
            let anchor = seek_context_anchor(&lines, context, search_from).ok_or_else(|| {
                format!(
                    "Failed to find context anchor `@@ {context}` in `{path}` (hunk #{})",
                    chunk_ix + 1
                )
            })?;
            search_from = anchor + 1;
        }

        if chunk.old_lines.is_empty() {
            // Pure insertion at EOF (or right after the anchor if not EOF).
            let insert_at = if chunk.is_end_of_file || chunk.change_context.is_none() {
                lines.len()
            } else {
                search_from.min(lines.len())
            };
            replacements.push((insert_at, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;

        let found = seek_sequence(&lines, pattern, search_from, chunk.is_end_of_file).or_else(
            || {
                // Tolerate a trailing blank line in the pattern when the file
                // doesn't carry one at EOF.
                if pattern.last().is_some_and(String::is_empty) {
                    pattern = &pattern[..pattern.len() - 1];
                    if new_slice.last().is_some_and(String::is_empty) {
                        new_slice = &new_slice[..new_slice.len() - 1];
                    }
                    seek_sequence(&lines, pattern, search_from, chunk.is_end_of_file)
                } else {
                    None
                }
            },
        );

        let start = found.ok_or_else(|| {
            format!(
                "Failed to locate hunk #{} in `{path}`:\n{}",
                chunk_ix + 1,
                chunk.old_lines.join("\n")
            )
        })?;

        replacements.push((start, pattern.len(), new_slice.to_vec()));
        search_from = start + pattern.len();
    }

    replacements.sort_by_key(|(start, _, _)| *start);
    for (start, old_len, new_lines) in replacements.into_iter().rev() {
        lines.splice(start..start + old_len, new_lines);
    }

    let mut out = lines.join("\n");
    if had_trailing_newline || (!out.is_empty() && chunks.iter().any(|c| !c.new_lines.is_empty())) {
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
    }
    Ok(out)
}

/// Locate a `@@ <context>` header anchor: try full-line fuzzy match first,
/// then prefix match, then substring match (models often write `@@ fn foo()`
/// for a line `fn foo() {`).
fn seek_context_anchor(lines: &[String], context: &str, start: usize) -> Option<usize> {
    if let Some(ix) = seek_sequence(lines, &[context.to_string()], start, false) {
        return Some(ix);
    }
    let needle = normalize_unicode_line(context.trim());
    if needle.is_empty() {
        return Some(start.min(lines.len()));
    }
    for (i, line) in lines.iter().enumerate().skip(start) {
        if normalize_unicode_line(line.trim()).starts_with(&needle) {
            return Some(i);
        }
    }
    for (i, line) in lines.iter().enumerate().skip(start) {
        if normalize_unicode_line(line.trim()).contains(&needle) {
            return Some(i);
        }
    }
    None
}

/// 4-tier fuzzy line-sequence matcher (exact -> trim_end -> trim -> unicode
/// punctuation/quote normalization).
fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start.min(lines.len()));
    }
    if pattern.len() > lines.len() {
        return None;
    }

    let search_start = if eof {
        lines.len().saturating_sub(pattern.len()).max(start)
    } else {
        start
    };
    let last_possible = lines.len() - pattern.len();
    if search_start > last_possible {
        return None;
    }

    // Tier 1: exact
    for i in search_start..=last_possible {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    // Tier 2: ignore trailing whitespace
    for i in search_start..=last_possible {
        if lines[i..i + pattern.len()]
            .iter()
            .zip(pattern)
            .all(|(a, b)| a.trim_end() == b.trim_end())
        {
            return Some(i);
        }
    }
    // Tier 3: ignore leading and trailing whitespace
    for i in search_start..=last_possible {
        if lines[i..i + pattern.len()]
            .iter()
            .zip(pattern)
            .all(|(a, b)| a.trim() == b.trim())
        {
            return Some(i);
        }
    }
    // Tier 4: unicode punctuation / smart quote normalization
    for i in search_start..=last_possible {
        if lines[i..i + pattern.len()]
            .iter()
            .zip(pattern)
            .all(|(a, b)| normalize_unicode_line(a.trim()) == normalize_unicode_line(b.trim()))
        {
            return Some(i);
        }
    }
    None
}

fn normalize_unicode_line(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Preview diff for the approval dialog without mutating disk.
pub fn preview_patch(cwd: &Path, patch: &str) -> String {
    let hunks = match parse_patch(patch) {
        Ok(hunks) => hunks,
        Err(error) => return format!("(patch parse error: {error})\n\n{patch}"),
    };
    let mut parts = Vec::new();
    for hunk in hunks {
        match hunk {
            PatchHunk::AddFile { path, content } => {
                let change = per_edit_diff(cwd, &cwd.join(&path), "", &content);
                parts.push(change.unified_diff);
            }
            PatchHunk::DeleteFile { path } => {
                let full = resolve_checked(cwd, &path, false).unwrap_or_else(|_| cwd.join(&path));
                let before = std::fs::read(&full)
                    .ok()
                    .and_then(|bytes| pig_utils::text::decode(&bytes).ok())
                    .map(|doc| doc.text)
                    .unwrap_or_default();
                let change = per_edit_diff(cwd, &full, &before, "");
                parts.push(change.unified_diff);
            }
            PatchHunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let full = resolve_checked(cwd, &path, false).unwrap_or_else(|_| cwd.join(&path));
                let before = std::fs::read(&full)
                    .ok()
                    .and_then(|bytes| pig_utils::text::decode(&bytes).ok())
                    .map(|doc| doc.text)
                    .unwrap_or_default();
                let after = apply_update_chunks(&path, &before, &chunks).unwrap_or_default();
                let target = move_path
                    .as_deref()
                    .map(|p| cwd.join(p))
                    .unwrap_or(full);
                let change = per_edit_diff(cwd, &target, &before, &after);
                parts.push(change.unified_diff);
            }
        }
    }
    parts.join("\n")
}

/// Prepared, validated filesystem operation ready to commit atomically.
enum PlannedOp {
    Write {
        display_path: String,
        full: PathBuf,
        create_parents: bool,
        before_lf: String,
        after_lf: String,
        encoded: Vec<u8>,
        added: bool,
    },
    Delete {
        display_path: String,
        full: PathBuf,
        before_lf: String,
    },
    Move {
        src_display: String,
        src_full: PathBuf,
        dest_display: String,
        dest_full: PathBuf,
        before_lf: String,
        after_lf: String,
        encoded: Vec<u8>,
    },
}

fn validate_write_target(ctx: &ToolContext<'_>, path: &str, full: &Path) -> Result<(), String> {
    if let Some(message) = metadata_write_rejection(ctx.cwd, ctx.state, path, full) {
        return Err(message);
    }
    if crate::task::credential_grant(ctx.state, full) != crate::task::CredentialGrant::Outside {
        return Err(format!(
            "{} is inside a protected credential directory; ApplyPatch cannot touch credential files. Use Write for an explicit single-file replacement.",
            full.display()
        ));
    }
    if let Some(message) = sandbox_write_rejection(ctx, full) {
        return Err(message);
    }
    if is_sensitive_file(full) && !approved_this_call(ctx, full) {
        return Err(sensitive_file_error(full));
    }
    if let Some(message) = super::paths::read_through_rejection(ctx, full) {
        return Err(message);
    }
    Ok(())
}

/// Resolve a destination path that may not have its parent directory yet, while
/// enforcing workspace boundaries before creating any directories.
fn resolve_for_validation(
    ctx: &ToolContext<'_>,
    path: &str,
    may_create_parents: bool,
) -> Result<PathBuf, String> {
    match resolve_with_access(ctx, path, false, FsAccess::Write) {
        Ok(full) => Ok(full),
        Err(err) if may_create_parents && err.starts_with("Directory does not exist") => {
            let fs = ctx.state.env.fs.as_ref();
            let cwd_can = fs
                .canonicalize(ctx.cwd)
                .map_err(|e| format!("Invalid working directory {}: {e}", ctx.cwd.display()))?;
            let raw = Path::new(path);
            let joined = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                cwd_can.join(raw)
            };
            let normalized = super::paths::lexical_normalize_public(&joined);
            let outside = !normalized.starts_with(&cwd_can);
            if outside && ctx.fs_grant != Some(FsAccess::Write) {
                let exempt = (ctx.state.env.is_local() && super::paths::is_tmp_absolute_path(path))
                    || (raw.is_absolute()
                        && ctx
                            .state
                            .extra_rw_roots
                            .iter()
                            .any(|root| normalized.starts_with(root)));
                if !exempt {
                    return Err(format!(
                        "Path escapes the working directory: {path} (writing outside the workspace needs the user's approval; ask them to approve this call)"
                    ));
                }
            }
            Ok(normalized)
        }
        Err(err) => Err(err),
    }
}

pub(super) struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    fn name(&self) -> &'static str {
        APPLY_PATCH_TOOL
    }

    fn prefer_strict(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": APPLY_PATCH_TOOL,
                "description": "Apply a multi-file, multi-hunk patch atomically using the standard patch envelope (`*** Begin Patch` ... `*** End Patch`). Supports `*** Add File: <path>`, `*** Update File: <path>` (with optional `*** Move to: <new_path>` and one or more `@@` hunks), and `*** Delete File: <path>`. All paths, policy checks, and hunk matches are verified before writing any file; original encoding and line endings are preserved. Hunks must match each file as it is on disk now: read its current content first (with Read or a shell command) unless you have already seen it, and give enough context to place each hunk.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "patch": {
                            "type": "string",
                            "description": "Patch text starting with `*** Begin Patch` and ending with `*** End Patch`."
                        }
                    },
                    "required": ["patch"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let patch_text = args["patch"]
                .as_str()
                .ok_or("Missing required parameter: patch")?;
            let hunks = parse_patch(patch_text)?;
            let fs = ctx.state.env.fs.as_ref();

            // Phase 1: validate every target path and hunk match in
            // memory before mutating anything on disk.
            let mut planned: Vec<PlannedOp> = Vec::with_capacity(hunks.len());
            for hunk in hunks {
                match hunk {
                    PatchHunk::AddFile { path, content } => {
                        let full = resolve_for_validation(&ctx, &path, true)?;
                        validate_write_target(&ctx, &path, &full)?;
                        if fs.exists(&full) {
                            return Err(format!(
                                "Cannot Add File `{path}`: file already exists (use `*** Update File: {path}` instead)"
                            ));
                        }
                        let encoded =
                            pig_utils::text::encode(&content, FileEncoding::Utf8, false, LineEnding::Lf)?;
                        planned.push(PlannedOp::Write {
                            display_path: path,
                            full,
                            create_parents: true,
                            before_lf: String::new(),
                            after_lf: content,
                            encoded,
                            added: true,
                        });
                    }
                    PatchHunk::DeleteFile { path } => {
                        let full = resolve_with_access(&ctx, &path, false, FsAccess::Write)?;
                        validate_write_target(&ctx, &path, &full)?;
                        if !fs.exists(&full) {
                            return Err(format!("Cannot Delete File `{path}`: file does not exist"));
                        }
                        if let Some(err) = fs.metadata(&full).ok().and_then(|m| {
                            file_size_error(
                                m.len,
                                MAX_EDIT_FILE_BYTES,
                                "file too large for ApplyPatch",
                            )
                        }) {
                            return Err(err);
                        }
                        let bytes = fs
                            .read_all(&full)
                            .map_err(|e| read_io_error_with_fs(fs, &path, &full, e))?;
                        let before_lf = pig_utils::text::decode(&bytes)
                            .map(|doc| doc.text)
                            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
                        planned.push(PlannedOp::Delete {
                            display_path: path,
                            full,
                            before_lf,
                        });
                    }
                    PatchHunk::UpdateFile {
                        path,
                        move_path,
                        chunks,
                    } => {
                        let full = resolve_with_access(&ctx, &path, false, FsAccess::Write)?;
                        validate_write_target(&ctx, &path, &full)?;
                        if let Some(err) = fs.metadata(&full).ok().and_then(|m| {
                            file_size_error(
                                m.len,
                                MAX_EDIT_FILE_BYTES,
                                "not suitable for whole-file patching; use ExecCommand for large files",
                            )
                        }) {
                            return Err(err);
                        }
                        let bytes = fs
                            .read_all(&full)
                            .map_err(|e| read_io_error_with_fs(fs, &path, &full, e))?;
                        let doc = pig_utils::text::decode(&bytes)?;
                        let after_lf = apply_update_chunks(&path, &doc.text, &chunks)?;
                        let encoded = pig_utils::text::encode(
                            &after_lf,
                            doc.encoding,
                            doc.bom,
                            doc.line_ending,
                        )?;

                        if let Some(dest) = move_path {
                            let dest_full = resolve_for_validation(&ctx, &dest, true)?;
                            validate_write_target(&ctx, &dest, &dest_full)?;
                            if dest_full != full && fs.exists(&dest_full) {
                                return Err(format!(
                                    "Cannot Move `{path}` to `{dest}`: destination already exists"
                                ));
                            }
                            planned.push(PlannedOp::Move {
                                src_display: path,
                                src_full: full,
                                dest_display: dest,
                                dest_full,
                                before_lf: doc.text,
                                after_lf,
                                encoded,
                            });
                        } else {
                            planned.push(PlannedOp::Write {
                                display_path: path,
                                full,
                                create_parents: false,
                                before_lf: doc.text,
                                after_lf,
                                encoded,
                                added: false,
                            });
                        }
                    }
                }
            }

            // Phase 2: snapshot originals and commit all file operations.
            let mut file_changes = Vec::new();
            let mut summary_lines = Vec::new();
            let mut combined_diff = String::new();
            let mut total_additions = 0u32;
            let mut total_deletions = 0u32;
            let mut first_rel_path: Option<String> = None;

            for op in planned {
                match op {
                    PlannedOp::Write {
                        display_path,
                        full,
                        create_parents,
                        before_lf,
                        after_lf,
                        encoded,
                        added,
                    } => {
                        if create_parents
                            && let Some(parent) = full.parent()
                        {
                            fs.create_dir_all(parent).map_err(|e| {
                                format!("Failed to create directory {}: {e}", parent.display())
                            })?;
                        }
                        let full = if create_parents {
                            resolve_with_access(&ctx, &display_path, true, FsAccess::Write)?
                        } else {
                            full
                        };
                        ctx.tracker.snapshot_with_fs(fs, &full)?;
                        fs.write_atomic(&full, &encoded)
                            .map_err(|e| format!("Failed to write {}: {e}", full.display()))?;
                        if let Ok(change) = ctx.tracker.diff_with_fs(fs, ctx.cwd, &full) {
                            file_changes.push(change);
                        }
                        let edit = per_edit_diff(ctx.cwd, &full, &before_lf, &after_lf);
                        if first_rel_path.is_none() {
                            first_rel_path = Some(edit.path.clone());
                        }
                        total_additions += edit.additions;
                        total_deletions += edit.deletions;
                        if !combined_diff.is_empty() {
                            combined_diff.push('\n');
                        }
                        combined_diff.push_str(&edit.unified_diff);
                        summary_lines.push(format!(
                            "{} {}",
                            if added { "A" } else { "M" },
                            display_path
                        ));
                    }
                    PlannedOp::Delete {
                        display_path,
                        full,
                        before_lf,
                    } => {
                        ctx.tracker.snapshot_with_fs(fs, &full)?;
                        fs.remove_file(&full)
                            .map_err(|e| format!("Failed to delete {}: {e}", full.display()))?;
                        ctx.state
                            .read_states
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&full);
                        if let Ok(change) = ctx.tracker.diff_with_fs(fs, ctx.cwd, &full) {
                            file_changes.push(change);
                        }
                        let edit = per_edit_diff(ctx.cwd, &full, &before_lf, "");
                        if first_rel_path.is_none() {
                            first_rel_path = Some(edit.path.clone());
                        }
                        total_additions += edit.additions;
                        total_deletions += edit.deletions;
                        if !combined_diff.is_empty() {
                            combined_diff.push('\n');
                        }
                        combined_diff.push_str(&edit.unified_diff);
                        summary_lines.push(format!("D {display_path}"));
                    }
                    PlannedOp::Move {
                        src_display,
                        src_full,
                        dest_display,
                        dest_full,
                        before_lf,
                        after_lf,
                        encoded,
                    } => {
                        if let Some(parent) = dest_full.parent() {
                            fs.create_dir_all(parent).map_err(|e| {
                                format!("Failed to create directory {}: {e}", parent.display())
                            })?;
                        }
                        let dest_full =
                            resolve_with_access(&ctx, &dest_display, true, FsAccess::Write)?;
                        ctx.tracker.snapshot_with_fs(fs, &src_full)?;
                        ctx.tracker.snapshot_with_fs(fs, &dest_full)?;
                        fs.write_atomic(&dest_full, &encoded)
                            .map_err(|e| format!("Failed to write {}: {e}", dest_full.display()))?;
                        if dest_full != src_full {
                            fs.remove_file(&src_full).map_err(|e| {
                                format!("Failed to remove {}: {e}", src_full.display())
                            })?;
                            ctx.state
                                .read_states
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .remove(&src_full);
                            if let Ok(change) = ctx.tracker.diff_with_fs(fs, ctx.cwd, &src_full) {
                                file_changes.push(change);
                            }
                        }
                        if let Ok(change) = ctx.tracker.diff_with_fs(fs, ctx.cwd, &dest_full) {
                            file_changes.push(change);
                        }
                        let edit = per_edit_diff(ctx.cwd, &dest_full, &before_lf, &after_lf);
                        if first_rel_path.is_none() {
                            first_rel_path = Some(edit.path.clone());
                        }
                        total_additions += edit.additions;
                        total_deletions += edit.deletions;
                        if !combined_diff.is_empty() {
                            combined_diff.push('\n');
                        }
                        combined_diff.push_str(&edit.unified_diff);
                        summary_lines.push(format!("R {src_display} -> {dest_display}"));
                    }
                }
            }

            let edit_diff = first_rel_path.map(|path| FileChange {
                path: if summary_lines.len() > 1 {
                    format!("{} (+{} more)", path, summary_lines.len() - 1)
                } else {
                    path
                },
                unified_diff: combined_diff,
                additions: total_additions,
                deletions: total_deletions,
            });
            let output = format!(
                "Success. Updated the following files:\n{}",
                summary_lines.join("\n")
            );
            Ok(ToolEffect {
                output,
                file_changes,
                edit_diff,
                images: vec![],
                documents: vec![],
                delivered_notification_ids: vec![],
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::SessionToolState;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pig-core-patch-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn test_state() -> SessionToolState {
        let (tx, _rx) = async_channel::unbounded();
        SessionToolState::new("s-patch".into(), tx, vec![], vec![])
    }

    fn make_ctx<'a>(
        cwd: &'a Path,
        tracker: &'a mut ChangeTracker,
        state: &'a SessionToolState,
    ) -> ToolContext<'a> {
        ToolContext {
            cwd,
            tracker,
            state,
            fs_grant: None,
            sandbox_override: crate::task::SandboxOverride::default(),
            sandbox_profile: crate::task::SandboxProfile::WorkspaceWrite,
            additional_permissions: crate::task::AdditionalPermissions::default(),
        }
    }

    #[test]
    fn parse_patch_handles_add_update_move_and_delete() {
        let raw = "\
*** Begin Patch
*** Add File: new.txt
+hello
+world
*** Update File: a.rs
*** Move to: b.rs
@@ fn demo()
 fn demo() {
-    old();
+    new();
 }
*** Delete File: gone.txt
*** End Patch";
        let hunks = parse_patch(raw).expect("parse patch");
        assert_eq!(hunks.len(), 3);
        assert!(matches!(&hunks[0], PatchHunk::AddFile { path, content } if path == "new.txt" && content == "hello\nworld\n"));
        assert!(matches!(&hunks[1], PatchHunk::UpdateFile { path, move_path: Some(dest), chunks } if path == "a.rs" && dest == "b.rs" && chunks.len() == 1));
        assert!(matches!(&hunks[2], PatchHunk::DeleteFile { path } if path == "gone.txt"));
    }

    #[test]
    fn apply_update_chunks_supports_fuzzy_whitespace_and_unicode_matching() {
        let original = "fn greet() {\n    let s = \u{201C}hello\u{201D};   \n    println!(\"{s}\");\n}\n";
        let chunks = vec![UpdateChunk {
            change_context: Some("fn greet()".into()),
            old_lines: vec!["let s = \"hello\";".into()],
            new_lines: vec!["    let s = \"hi\";".into()],
            is_end_of_file: false,
        }];
        let updated = apply_update_chunks("demo.rs", original, &chunks).expect("apply fuzzy chunk");
        assert!(updated.contains("let s = \"hi\";"), "{updated}");
    }

    #[tokio::test]
    async fn apply_patch_commits_multi_file_atomically_and_rolls_back_on_error() {
        let dir = test_dir("atomic");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, "alpha\nbeta\n").unwrap();
        std::fs::write(&b, "one\ntwo\n").unwrap();

        let state = test_state();
        // First: patch where second hunk fails -> first file MUST stay untouched.
        let failing_patch = "\
*** Begin Patch
*** Update File: a.txt
@@
 alpha
-beta
+BETA
*** Update File: b.txt
@@
-nonexistent
+changed
*** End Patch";
        let mut tracker = ChangeTracker::default();
        let Err(err) = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": failing_patch }),
                make_ctx(&dir, &mut tracker, &state),
            )
            .await
        else {
            panic!("second hunk mismatch must fail whole patch");
        };
        assert!(err.contains("b.txt"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            "alpha\nbeta\n",
            "first file must remain unmodified when a later hunk fails"
        );

        // Second: valid multi-file add + update + delete.
        let valid_patch = "\
*** Begin Patch
*** Update File: a.txt
@@
 alpha
-beta
+BETA
*** Delete File: b.txt
*** Add File: sub/c.txt
+created
*** End Patch";
        let effect = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": valid_patch }),
                make_ctx(&dir, &mut tracker, &state),
            )
            .await
            .expect("valid patch succeeds");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "alpha\nBETA\n");
        assert!(!b.exists());
        assert_eq!(std::fs::read_to_string(dir.join("sub/c.txt")).unwrap(), "created\n");
        assert_eq!(effect.file_changes.len(), 3);
        assert!(effect.output.contains("M a.txt"));
        assert!(effect.output.contains("D b.txt"));
        assert!(effect.output.contains("A sub/c.txt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn apply_patch_preserves_crlf_line_endings() {
        let dir = test_dir("crlf");
        let f = dir.join("win.txt");
        std::fs::write(&f, "line1\r\nline2\r\n").unwrap();
        let state = test_state();
        let patch = "\
*** Begin Patch
*** Update File: win.txt
@@
 line1
-line2
+LINE2
*** End Patch";
        let mut tracker = ChangeTracker::default();
        ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                make_ctx(&dir, &mut tracker, &state),
            )
            .await
            .expect("patch crlf file");
        assert_eq!(std::fs::read(&f).unwrap(), b"line1\r\nLINE2\r\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
