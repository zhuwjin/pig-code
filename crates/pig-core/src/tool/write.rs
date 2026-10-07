use super::*;

/// Atomic persistence (shared by Write/Edit): a temp file in the same directory + rename, so a crash/power loss midway never
/// leaves truncated content (same-volume rename is atomic; std rename on Windows also overwrites an existing target).
/// The temp name carries pid+nanoseconds to avoid concurrent collisions; on failure the temp file is cleaned up.
pub(crate) fn atomic_write(full: &Path, bytes: &[u8]) -> Result<(), String> {
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
    if let Err(e) = std::fs::write(&tmp, bytes) {
        return Err(format!("Failed to write {}: {e}", full.display()));
    }
    match std::fs::rename(&tmp, full) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(format!("Failed to write {}: {e}", full.display()))
        }
    }
}

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Write",
                "description": "Write an entire file (parent directories are created automatically). Prefer Edit for localized changes to large files.",
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
            let full = resolve_with_access(ctx.state, ctx.cwd, path, true, FsAccess::Write)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            // Existing files are written back with their encoding/BOM/line ending preserved; unrecognized ones (binary/unknown encoding) are overwritten as UTF-8/LF;
            // new files are always UTF-8/LF. Existing files over the Read cap may not be overwritten wholesale (same memory-guard policy)
            if let Some(err) = std::fs::metadata(&full).ok().and_then(|m| {
                file_size_error(
                    m.len(),
                    MAX_READ_FILE_BYTES,
                    "target file too large; use Edit for localized changes instead",
                )
            }) {
                return Err(err);
            }
            let existing = std::fs::read(&full).ok();
            check_fresh(ctx.state, &full, existing.is_some(), "writing")?;
            let (encoding, bom, line_ending, note) = match &existing {
                Some(bytes) => match crate::text::decode(bytes) {
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
            ctx.tracker.snapshot(&full)?;
            let bytes = crate::text::encode(content, encoding, bom, line_ending)?;
            atomic_write(&full, &bytes)?;
            // Refresh freshness after the write: immediately editing the file just written must be legal
            record_read_state(ctx.state, &full, &bytes, false, None);
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            let edit_diff = Some(per_edit_diff(ctx.cwd, &full, &before, &after));
            Ok(ToolEffect {
                output: format!("Wrote {} ({} bytes){note}", path, bytes.len()),
                file_change,
                edit_diff,
                images: vec![],
            })
        })
    }
}
