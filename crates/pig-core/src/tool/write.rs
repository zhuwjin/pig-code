use super::*;

/// Atomic persistence (shared by Write/Edit): a temp file in the same directory + rename, so a crash/power loss midway never
/// leaves truncated content (same-volume rename is atomic; std rename on Windows also overwrites an existing target).
/// The temp name carries pid+nanoseconds to avoid concurrent collisions; `create_new` refuses a planted entry instead
/// of writing through it, and an existing target keeps its mode (a credential file the user approved replacing must
/// not become group/world-readable on the way). On failure the temp file is cleaned up.
pub(crate) fn atomic_write(full: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let dir = full.parent().unwrap_or_else(|| Path::new("."));
    let name = full
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), nanos));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        drop(file);
        #[cfg(unix)]
        if let Ok(metadata) = std::fs::metadata(full) {
            let permissions = metadata.permissions();
            // `set_permissions` accepts the mode bits it reads back; no mode
            // widening is performed for a new file (metadata fails first).
            if let Err(error) = std::fs::set_permissions(&tmp, permissions) {
                let _ = std::fs::remove_file(&tmp);
                return Err(error);
            }
        }
        std::fs::rename(&tmp, full)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("Failed to write {}: {e}", full.display()))
}

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn prefer_strict(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Write",
                "description": "Write an entire file (parent directories are created automatically). Prefer Edit for localized changes to large files. A file inside a profile credential directory (~/.ssh, ~/.config, ...) needs the user's approval: the dialog names that one file, the whole file is replaced, and the approval is never remembered for the session. Edit cannot be used there because the sandbox keeps the content unreadable.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to the working directory" },
                        "content": { "type": "string", "description": "The complete file content" }
                    },
                    "required": ["path", "content"]
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
            let path = args["path"]
                .as_str()
                .ok_or("Missing required parameter: path")?;
            let content = args["content"]
                .as_str()
                .ok_or("Missing required parameter: content")?;
            let full = resolve_with_access(&ctx, path, true, FsAccess::Write)?;
            if let Some(message) = metadata_write_rejection(ctx.cwd, ctx.state, path, &full) {
                return Err(message);
            }
            // Deny rules bind writes as much as reads — an approved
            // out-of-workspace write must not override them. The credential
            // single-file approval bypasses the veto for exactly that file.
            if let Some(message) = sandbox_write_rejection(&ctx, &full) {
                return Err(message);
            }
            if is_sensitive_file(&full) && !approved_this_call(&ctx, &full) {
                return Err(sensitive_file_error(&full));
            }
            // Existing files are written back with their encoding/BOM/line ending preserved; unrecognized ones (binary/unknown encoding) are overwritten as UTF-8/LF;
            // new files are always UTF-8/LF. Existing files over the Read cap may not be overwritten wholesale (same memory-guard policy)
            let fs = ctx.state.env.fs.as_ref();
            if let Some(err) = fs.metadata(&full).ok().and_then(|m| {
                file_size_error(
                    m.len,
                    MAX_READ_FILE_BYTES,
                    "target file too large; use Edit for localized changes instead",
                )
            }) {
                return Err(err);
            }
            // A credential file's approval is **write-only**: the dialog says
            // the whole file is replaced and the content stays unreadable. Read
            // it only for an ordinary target. Reading it here (encoding probe,
            // change diff, revert baseline) would put content the user never
            // shared into the diff, the UI and the persisted store.
            let credential_write = crate::task::credential_grant(ctx.state, &full)
                == crate::task::CredentialGrant::File;
            if !credential_write
                && let Some(message) = super::paths::read_through_rejection(&ctx, &full)
            {
                return Err(message);
            }
            let existing = if credential_write {
                None
            } else {
                fs.read_all(&full).ok()
            };
            if !credential_write {
                // A credential file has no readable state, so the freshness
                // rule cannot be satisfied; every other existing file must have
                // been read before it is overwritten.
                check_fresh(ctx.state, &full, existing.is_some(), "writing")?;
            }
            let (encoding, bom, line_ending, note) = match &existing {
                Some(bytes) => match pig_utils::text::decode(bytes) {
                    Ok(doc) => {
                        let note = match (
                            doc.encoding != FileEncoding::Utf8,
                            doc.line_ending == LineEnding::Crlf,
                        ) {
                            (true, true) => {
                                format!(
                                    " (preserved original encoding {} / CRLF)",
                                    doc.encoding.label()
                                )
                            }
                            (true, false) => {
                                format!(" (preserved original encoding {})", doc.encoding.label())
                            }
                            (false, true) => " (preserved original CRLF line endings)".to_string(),
                            (false, false) => String::new(),
                        };
                        (doc.encoding, doc.bom, doc.line_ending, note)
                    }
                    Err(_) => (
                        FileEncoding::Utf8,
                        false,
                        LineEnding::Lf,
                        " (original encoding unrecognized; overwritten as UTF-8)".to_string(),
                    ),
                },
                None => (FileEncoding::Utf8, false, LineEnding::Lf, String::new()),
            };
            // Both diff sides use the LF view: before = the original file's decoded text (or ""), after = the normalized LF text
            let before = existing.as_deref().map(decoded_view).unwrap_or_default();
            let after = content.replace("\r\n", "\n");
            if !credential_write {
                ctx.tracker.snapshot_with_fs(fs, &full)?;
            }
            let bytes = pig_utils::text::encode(content, encoding, bom, line_ending)?;
            fs.write_atomic(&full, &bytes)
                .map_err(|e| format!("Failed to write {}: {e}", full.display()))?;
            // Refresh freshness after the write: immediately editing the file just written must be legal
            record_read_state(ctx.state, &full, &bytes, false, None);
            let file_change = if credential_write {
                None
            } else {
                ctx.tracker.diff_with_fs(fs, ctx.cwd, &full).ok()
            };
            let edit_diff = Some(per_edit_diff(ctx.cwd, &full, &before, &after));
            Ok(ToolEffect {
                output: format!("Wrote {} ({} bytes){note}", path, bytes.len()),
                file_change,
                edit_diff,
                images: vec![],
                documents: vec![],
                delivered_notification_ids: vec![],
            })
        })
    }
}
