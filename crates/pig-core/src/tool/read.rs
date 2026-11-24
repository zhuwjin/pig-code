use super::*;

/// Record what a Read showed under the opened canonical target **and** under
/// the spelling the model used. For a final symlink these differ: the read
/// followed the link, so a later Read of either spelling with the same
/// parameters finds the record and can answer "file unchanged".
fn record_read_view(
    ctx: &ToolContext<'_>,
    raw_path: &str,
    full: &Path,
    bytes: &[u8],
    view: (usize, usize, usize),
) {
    record_read_state(ctx.state, full, bytes, view);
    if let Some(spelling) = super::paths::write_spelling(ctx.state.env.fs.as_ref(), ctx.cwd, raw_path)
        && spelling != *full
    {
        record_read_state(ctx.state, &spelling, bytes, view);
    }
}

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn prefer_strict(&self) -> bool {
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
            if let Some(message) = sandbox_read_rejection(&ctx, path, &full) {
                return Err(message);
            }
            if is_sensitive_file(&full) && !approved_this_call(&ctx, &full) {
                return Err(sensitive_file_error(&full));
            }
            // Size guard: stat as a fast path, then enforce the same cap on the
            // bytes actually read (MAX + 1) so a file that grows between stat
            // and read cannot slip an unbounded read through.
            const SIZE_HINT: &str = "use Grep to locate the key content, or page through it with ExecCommand (head/tail/grep)";
            let fs = ctx.state.env.fs.as_ref();
            let meta = fs
                .metadata(&full)
                .map_err(|e| read_io_error_with_fs(fs, path, &full, e))?;
            if let Some(err) = file_size_error(meta.len, MAX_READ_FILE_BYTES, SIZE_HINT) {
                return Err(err);
            }
            let bytes = fs
                .read_bytes_bounded(&full, MAX_READ_FILE_BYTES)
                .map_err(|e| read_io_error_with_fs(fs, path, &full, e))?;
            if let Some(err) = file_size_error(bytes.len() as u64, MAX_READ_FILE_BYTES, SIZE_HINT) {
                return Err(err);
            }
            if pig_utils::pdf::has_pdf_header(&bytes) {
                return Err(
                    "This is a PDF document; read it with ReadMediaFile instead (the current model must support PDF input)"
                        .to_string(),
                );
            }
            let doc = match pig_utils::text::decode(&bytes) {
                Ok(doc) => doc,
                Err(error) => {
                    // Give explicit guidance for media (magic-byte sniffing, does not trust the extension)
                    if pig_utils::pdf::has_pdf_header(&bytes) {
                        return Err(
                            "This is a PDF document; read it with ReadMediaFile instead (the current model must support PDF input)"
                                .to_string(),
                        );
                    }
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
                return Ok(ToolEffect::plain("(empty file)".to_string()));
            }
            let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(MAX_READ_LINES as u64) as usize;
            let column_offset = args["column_offset"].as_u64().unwrap_or(0) as usize;
            // Repeat-read short circuit (same as ZCode file_unchanged): the same
            // view parameters and an unchanged content hash mean the model has
            // already seen exactly this output, so it is not repeated. A
            // different page, or bytes that changed since, is shown in full.
            let view = (offset, limit, column_offset);
            let unchanged = match ctx.state.read_states.lock() {
                Ok(states) => states.get(&full).is_some_and(|seen| {
                    seen.hash == hash_bytes(&bytes) && seen.view == view
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
            record_read_view(&ctx, path, &full, &bytes, view);
            Ok(ToolEffect::plain(out))
        })
    }
}

/// Disk-read error shared by Read/Edit: on file-not-found, append up to 20 file names from the parent directory to guide the model to correct the path
pub(crate) fn read_io_error(path: &str, full: &Path, error: std::io::Error) -> String {
    read_io_error_with_fs(&crate::task::LocalFileSystem, path, full, error)
}

pub(crate) fn read_io_error_with_fs(
    fs: &dyn crate::task::ExecutorFileSystem,
    path: &str,
    full: &Path,
    error: std::io::Error,
) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        let mut message = format!("File not found: {path}");
        if let Some(parent) = full.parent()
            && let Ok(entries) = fs.read_dir(parent)
        {
            let mut names: Vec<String> = entries.into_iter().map(|entry| entry.name).collect();
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

/// Register what a Read showed: the hash of the bytes it rendered and the
/// view parameters it used, for the repeat-read short circuit.
pub(crate) fn record_read_state(
    state: &SessionToolState,
    full: &Path,
    bytes: &[u8],
    view: (usize, usize, usize),
) {
    let entry = crate::task::ReadState {
        hash: hash_bytes(bytes),
        view,
    };
    if let Ok(mut states) = state.read_states.lock() {
        states.insert(full.to_path_buf(), entry);
    }
}
