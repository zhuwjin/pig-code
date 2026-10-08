use super::*;

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Read",
                "description": "Read a workspace file's contents, prefixed with \"line-number\\t\" on each line. path is relative to the working directory; page long files with offset/limit (a ~100k character budget per call); continue lines longer than 2000 characters with column_offset. UTF-16/GBK files are transcoded automatically; binary files are refused. Files over 100MB are refused (locate content with Grep or page through with Bash). Re-reading an unchanged file with the same parameters returns a \"file unchanged\" notice instead of the full text.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to the working directory" },
                        "offset": { "type": "integer", "description": "1-based starting line number (default 1)" },
                        "limit": { "type": "integer", "description": "Maximum number of lines to read (default 2000)" },
                        "column_offset": { "type": "integer", "description": "Zero-based starting character column applied to every line. Use it to continue reading lines longer than 2000 characters — the truncation note reports the next page's column_offset value" }
                    },
                    "required": ["path"]
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
            let full = resolve_with_access(&ctx, path, false, FsAccess::Read)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            // Size guard: stat before reading from disk to keep whole files out of memory (NotFound still goes through the guidance message)
            let meta = std::fs::metadata(&full).map_err(|e| read_io_error(path, &full, e))?;
            if let Some(err) = file_size_error(
                meta.len(),
                MAX_READ_FILE_BYTES,
                "use Grep to locate the key content, or page through it with Bash (head/tail/grep)",
            ) {
                return Err(err);
            }
            let bytes = std::fs::read(&full).map_err(|e| read_io_error(path, &full, e))?;
            let doc = match crate::text::decode(&bytes) {
                Ok(doc) => doc,
                Err(error) => {
                    // Give explicit guidance for images (magic-byte sniffing, does not trust the extension)
                    if let Some(mime) = sniff_image(&bytes) {
                        let label = match mime {
                            "image/png" => "PNG",
                            "image/jpeg" => "JPEG",
                            "image/gif" => "GIF",
                            "image/webp" => "WebP",
                            _ => mime,
                        };
                        return Err(format!(
                            "This is a {label} image; read it with ReadMediaFile instead (the current model must support image input)"
                        ));
                    }
                    return Err(error);
                }
            };
            if doc.text.is_empty() {
                record_read_state(ctx.state, &full, &bytes, false, None);
                return Ok(ToolEffect::plain("(empty file)".to_string()));
            }
            let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(MAX_READ_LINES as u64) as usize;
            let column_offset = args["column_offset"].as_u64().unwrap_or(0) as usize;
            // Repeat-read short circuit (same as ZCode file_unchanged): same view parameters + unchanged content hash
            // → skip re-outputting the full text, saving tokens. The refresh flow uses the same read_states record;
            // an identical hash means the freshness information needs no update.
            let view = (offset, limit, column_offset);
            let unchanged = match ctx.state.read_states.lock() {
                Ok(states) => states.get(&full).is_some_and(|r| {
                    // Truncated output is deterministic for the same parameters, so budget-truncated reads can short-circuit too
                    r.hash == hash_bytes(&bytes) && r.view == Some(view)
                }),
                Err(_) => false,
            };
            if unchanged {
                return Ok(ToolEffect::plain(
                    "(File unchanged: same parameters as the last Read and identical content — no need to re-read)".to_string(),
                ));
            }
            let lines: Vec<&str> = doc.text.lines().collect();
            let total = lines.len();
            let start = (offset - 1).min(total);
            // Render line by line (with line numbers), stopping at whichever comes first: the line cap or the character budget (line-number prefixes included);
            // overlong lines start reading at column_offset, with the truncation note carrying the continuation parameters
            let mut rendered: Vec<String> = Vec::new();
            let mut used_chars = 0usize;
            let mut end = start;
            for (index, line) in lines.iter().enumerate().skip(start) {
                if index - start >= limit {
                    break;
                }
                let line_no = index + 1;
                let line_chars = line.chars().count();
                let visible = line_chars.saturating_sub(column_offset);
                let body = if visible > MAX_LINE_CHARS {
                    let taken: String = line
                        .chars()
                        .skip(column_offset)
                        .take(MAX_LINE_CHARS)
                        .collect();
                    let next = column_offset + MAX_LINE_CHARS;
                    format!(
                        "{taken} [...line continues; read chars {}-{next} of {line_chars}; continue with column_offset={next}]",
                        column_offset + 1
                    )
                } else if column_offset > 0 {
                    let taken: String = line.chars().skip(column_offset).collect();
                    format!(
                        "{taken} [chars {}-{line_chars} of {line_chars} in this line]",
                        column_offset + 1
                    )
                } else {
                    (*line).to_string()
                };
                let row = format!("{line_no}\t{body}");
                let row_chars = row.chars().count();
                if !rendered.is_empty() && used_chars + row_chars > MAX_READ_CHARS {
                    break;
                }
                used_chars += row_chars;
                rendered.push(row);
                end = index + 1;
            }
            let mut out = rendered.join("\n");
            if end < total {
                out.push_str(&format!(
                    "\n\n[Truncated: showing lines {}-{end} of {total}; continue reading with the offset parameter]",
                    start + 1
                ));
            }
            // Meta info: note only for non-default encoding/line ending or lossy decoding
            if doc.encoding != FileEncoding::Utf8 || doc.line_ending == LineEnding::Crlf {
                let mut parts = vec![format!("encoding={}", doc.encoding.label())];
                if doc.line_ending == LineEnding::Crlf {
                    parts.push(
                        "line endings=CRLF (shown as LF; restored on write-back)".to_string(),
                    );
                }
                out.push_str(&format!("\n\n[File info: {}]", parts.join(", ")));
            }
            if doc.lossy {
                out.push_str("\n[Warning: decoding produced replacement characters; the detected encoding may be wrong]");
            }
            // ZCode semantics: only a "full read" truncated by the budget counts as partial; explicitly paged reads do not
            let paged = args.get("offset").is_some() || args.get("limit").is_some();
            record_read_state(ctx.state, &full, &bytes, !paged && end < total, Some(view));
            Ok(ToolEffect::plain(out))
        })
    }
}

/// Disk-read error shared by Read/Edit: on file-not-found, append up to 20 file names from the parent directory to guide the model to correct the path
pub(crate) fn read_io_error(path: &str, full: &Path, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        let mut message = format!("File not found: {path}");
        if let Some(parent) = full.parent()
            && let Ok(entries) = std::fs::read_dir(parent)
        {
            let mut names: Vec<String> = entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            names.truncate(20);
            if !names.is_empty() {
                message.push_str(&format!(
                    ". Directory {} contains: {}",
                    parent.display(),
                    names.join(", ")
                ));
            }
        }
        message
    } else {
        format!("Failed to read {}: {error}", full.display())
    }
}

/// File content fingerprint (DefaultHasher over the raw bytes)
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash_slice(bytes, &mut hasher);
    hasher.finish()
}

/// Register on Read success / refresh the freshness state after Write/Edit hits the disk (mtime takes the new post-write value).
/// view: Read carries this call's view parameters (for the repeat-read short circuit); internal refreshes pass None.
pub(crate) fn record_read_state(
    state: &SessionToolState,
    full: &Path,
    bytes: &[u8],
    partial: bool,
    view: Option<(usize, usize, usize)>,
) {
    let mtime = std::fs::metadata(full).ok().and_then(|m| m.modified().ok());
    let entry = crate::task::ReadState {
        mtime,
        size: bytes.len() as u64,
        hash: hash_bytes(bytes),
        partial,
        view,
    };
    if let Ok(mut states) = state.read_states.lock() {
        states.insert(full.to_path_buf(), entry);
    }
}

/// Pre-write freshness check (same as ZCode read-file-state): a nonexistent file (new creation) is allowed;
/// never read / last view was incomplete / disk modified externally after the read — all rejected.
/// The hash is compared only when mtime or size changed; an identical hash (content verbatim unchanged) is allowed and the state is updated along the way.
pub(crate) fn check_fresh(
    state: &SessionToolState,
    full: &Path,
    file_exists: bool,
    verb: &str,
) -> Result<(), String> {
    if !file_exists {
        return Ok(());
    }
    let (read_mtime, read_size, read_hash, partial) = {
        let states = state.read_states.lock().map_err(|e| e.to_string())?;
        let Some(read) = states.get(full) else {
            return Err(format!(
                "File exists but has not been read in this session; Read it before {verb} to avoid overwriting others' changes"
            ));
        };
        (read.mtime, read.size, read.hash, read.partial)
    };
    if partial {
        return Err(
            "The last Read was an incomplete view (output was truncated); finish it with offset/limit pages or a full Read before editing"
                .to_string(),
        );
    }
    let meta =
        std::fs::metadata(full).map_err(|e| format!("Failed to stat {}: {e}", full.display()))?;
    if meta.modified().ok() == read_mtime && meta.len() == read_size {
        return Ok(());
    }
    let bytes =
        std::fs::read(full).map_err(|e| format!("Failed to read {}: {e}", full.display()))?;
    if hash_bytes(&bytes) == read_hash {
        record_read_state(state, full, &bytes, false, None);
        return Ok(());
    }
    Err("The file was modified externally since the last Read; Read it again before editing (to avoid overwriting others' changes)".to_string())
}
