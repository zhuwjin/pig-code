use super::*;

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "Edit"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Edit",
                "description": "Perform an exact text replacement in a file. old_string must appear exactly once in the file (replace_all=true replaces every occurrence); Read the file to confirm its content before editing. Tolerant matching covers common slips — line-number prefixes, curly vs straight quotes, and literal \\n escape sequences (the output notes when one kicks in). The file's original encoding and line endings are preserved. Files over 50MB are refused (use Bash sed/awk for large files).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to the working directory" },
                        "old_string": { "type": "string", "description": "The original text to replace (must appear exactly once; with replace_all=true every occurrence is replaced)" },
                        "new_string": { "type": "string", "description": "The replacement text; when empty and old_string spans whole lines, the trailing newline is deleted along with the match, leaving no blank line" },
                        "replace_all": { "type": "boolean", "description": "true replaces every match (default false, which requires a unique occurrence)" }
                    },
                    "required": ["path", "old_string", "new_string"]
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
            let old = args["old_string"]
                .as_str()
                .ok_or("Missing required parameter: old_string")?;
            let new = args["new_string"]
                .as_str()
                .ok_or("Missing required parameter: new_string")?;
            if old.is_empty() {
                return Err("old_string must not be empty; use Write to create a file".to_string());
            }
            if old == new {
                return Err(
                    "old_string and new_string are identical; nothing to change".to_string()
                );
            }
            let replace_all = args["replace_all"].as_bool().unwrap_or(false);
            let full = resolve_with_access(&ctx, path, false, FsAccess::Write)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            // The size guard runs before the freshness check: oversized files are rejected outright, never entering whole-read/whole-write
            if let Some(err) = std::fs::metadata(&full).ok().and_then(|m| {
                file_size_error(
                    m.len(),
                    MAX_EDIT_FILE_BYTES,
                    "not suitable for whole-file editing; use Bash (sed/awk) for localized changes, or split the file first",
                )
            }) {
                return Err(err);
            }
            check_fresh(ctx.state, &full, full.exists(), "editing")?;
            let bytes = std::fs::read(&full).map_err(|e| read_io_error(path, &full, e))?;
            let doc = crate::text::decode(&bytes)?;
            let content = doc.text;
            // The match + replacement computation lives in compute_edit (shared with the approval preview; what the preview shows is what gets applied)
            let outcome = match compute_edit(&content, old, new, replace_all) {
                Ok(outcome) => outcome,
                Err(EditMatchError::NotFound) => {
                    let mut message = format!(
                        "old_string not found in {path}. Read the file to confirm its current content first (indentation and line breaks must match exactly)."
                    );
                    if doc.line_ending == LineEnding::Crlf {
                        message.push_str(
                            " The file uses CRLF line endings; Read output is shown as LF, so use LF line breaks in old_string.",
                        );
                    }
                    return Err(message);
                }
                Err(EditMatchError::NotUnique { count, lines }) => {
                    let lines_note = if lines.is_empty() {
                        String::new()
                    } else {
                        let joined = lines
                            .iter()
                            .map(|l| l.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        if count > lines.len() {
                            format!(" (lines {joined}, among others)")
                        } else {
                            format!(" (lines {joined})")
                        }
                    };
                    return Err(format!(
                        "old_string appears {count} times in {path}{lines_note} and cannot be located uniquely. Expand old_string until it is unique; to replace every occurrence, set replace_all=true."
                    ));
                }
            };
            ctx.tracker.snapshot(&full)?;
            // Matching and replacement both happen on the LF view (already normalized by decoding); the original encoding/line ending is restored on write-back
            let after = outcome.after;
            let encoded = crate::text::encode(&after, doc.encoding, doc.bom, doc.line_ending)?;
            atomic_write(&full, &encoded)?;
            // Refresh freshness after the write: immediately editing the file just written must be legal
            record_read_state(ctx.state, &full, &encoded, false, None);
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            let edit_diff = Some(per_edit_diff(ctx.cwd, &full, &content, &after));
            let output = if replace_all {
                format!("Edited {path} (replaced {} occurrences)", outcome.replaced)
            } else if let Some(note) = outcome.tier_note {
                format!("Edited {path} ({note})")
            } else {
                format!("Edited {path}")
            };
            Ok(ToolEffect {
                output,
                file_change,
                edit_diff,
                images: vec![],
            })
        })
    }
}

/// The two match-failure shapes of Edit (error messages are assembled by the caller, which has the path/line-ending context).
/// NotUnique carries the line numbers of the matches (1-based, up to 5, helping the model locate them while broadening the scope).
#[derive(Debug)]
pub enum EditMatchError {
    NotFound,
    NotUnique { count: usize, lines: Vec<usize> },
}

/// Product of compute_edit: post-replacement text, replacement count, and the tolerant-tier hit note
pub struct EditOutcome {
    pub after: String,
    pub replaced: usize,
    /// Tolerant-tier hit notes such as "tolerant match: line-number prefixes stripped" / "tolerant match: quote style
    /// adjusted to match the file"; None on an exact match
    pub tier_note: Option<&'static str>,
}

/// Edit's match + replacement computation (LF view): a four-level ladder — exact → strip Read line-number
/// prefixes → quote normalization → unescape normalization — each level doing its own uniqueness check; replace_all goes exact-only
/// (tolerant matches replace once). Shared by the approval preview and Edit execution, guaranteeing "what the preview shows is what gets applied".
pub fn compute_edit(
    content_lf: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<EditOutcome, EditMatchError> {
    let mut effective_old = old.to_string();
    let mut effective_new = new.to_string();
    let mut quote_window: Option<(usize, usize)> = None;
    // Window start line numbers when quote normalization hits multiple times (for the NotUnique error)
    let mut quote_multi_lines: Vec<usize> = Vec::new();
    let mut tier_note: Option<&'static str> = None;
    let mut count = content_lf.matches(old).count();
    if count == 0 && !replace_all {
        if let Some(stripped) = strip_line_number_prefixes(old) {
            let stripped_count = content_lf.matches(&stripped).count();
            if stripped_count > 0 {
                count = stripped_count;
                effective_old = stripped;
                tier_note = Some("tolerant match: line-number prefixes stripped");
            }
        }
        if count == 0 {
            let windows = find_quote_normalized_windows(content_lf, old);
            if windows.len() == 1 {
                let (start, end) = windows[0];
                count = 1;
                quote_window = Some((start, end));
                effective_new = follow_quote_style(new, &content_lf[start..end]);
                tier_note = Some("tolerant match: quote style adjusted to match the file");
            } else if windows.len() > 1 {
                count = windows.len();
                quote_multi_lines = windows
                    .iter()
                    .take(5)
                    .map(|(start, _)| line_of(content_lf, *start))
                    .collect();
            }
        }
        // Level 4: unescape normalization — the model writes literal \n\t\r etc. into old_string
        // (common when copying from string literals/JSON); new_string is unescaped in step (same as ZCode).
        // Not applied on unrecognized escapes or a trailing lone backslash (better to miss the match than mangle the text).
        if count == 0
            && let Some(old_un) = unescape_literal(old)
        {
            let new_un = unescape_literal(new).unwrap_or_else(|| new.to_string());
            let un_count = content_lf.matches(&old_un).count();
            if un_count > 0 {
                count = un_count;
                effective_old = old_un;
                effective_new = new_un;
                tier_note = Some("tolerant match: literal escape sequences unescaped");
            }
        }
    }
    if count == 0 {
        return Err(EditMatchError::NotFound);
    }
    if count > 1 && !replace_all {
        let lines = if quote_multi_lines.is_empty() {
            match_lines(content_lf, &effective_old, 5)
        } else {
            quote_multi_lines
        };
        return Err(EditMatchError::NotUnique { count, lines });
    }
    let (after, replaced) = match quote_window {
        // Quote-normalization hit: replace those N original lines wholesale
        Some((start, end)) => {
            let mut out = String::with_capacity(content_lf.len());
            out.push_str(&content_lf[..start]);
            out.push_str(&effective_new);
            out.push_str(&content_lf[end..]);
            (out, 1)
        }
        None => apply_replacement(content_lf, &effective_old, &effective_new, replace_all),
    };
    Ok(EditOutcome {
        after,
        replaced,
        tier_note,
    })
}

/// Manual scan-and-replace (not String::replace, implementing ZCode's deletion optimization):
/// when new is empty, old does not end with \n, and the match position is immediately followed by \n, that \n is deleted too, leaving no blank line.
/// Returns (post-replacement text, replacement count).
fn apply_replacement(content: &str, old: &str, new: &str, replace_all: bool) -> (String, usize) {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    let mut replaced = 0usize;
    while let Some(pos) = rest.find(old) {
        out.push_str(&rest[..pos]);
        out.push_str(new);
        rest = &rest[pos + old.len()..];
        if new.is_empty() && !old.ends_with('\n') && rest.starts_with('\n') {
            rest = &rest[1..];
        }
        replaced += 1;
        if !replace_all {
            break;
        }
    }
    out.push_str(rest);
    (out, replaced)
}

/// Edit tolerant level 4: unescape literal escape sequences (\n \t \r \" \' \` \$ \\ → real characters).
/// Unrecognized escape combinations stay as-is; a trailing lone backslash returns None (cannot be interpreted safely);
/// input containing no escape sequences at all also returns None (this level need not participate).
fn unescape_literal(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut escaped_any = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => {
                out.push('\n');
                escaped_any = true;
            }
            Some('t') => {
                out.push('\t');
                escaped_any = true;
            }
            Some('r') => {
                out.push('\r');
                escaped_any = true;
            }
            Some(escaped @ ('"' | '\'' | '`' | '$' | '\\')) => {
                out.push(escaped);
                escaped_any = true;
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => return None,
        }
    }
    escaped_any.then_some(out)
}

/// Line numbers (1-based, first max taken) of each occurrence of needle in content.
fn match_lines(content: &str, needle: &str, max: usize) -> Vec<usize> {
    let mut lines = Vec::new();
    let mut offset = 0usize;
    while let Some(pos) = content[offset..].find(needle) {
        lines.push(line_of(content, offset + pos));
        if lines.len() >= max {
            break;
        }
        offset += pos + needle.len();
    }
    lines
}

/// Line number (1-based) containing byte offset pos.
fn line_of(text: &str, pos: usize) -> usize {
    1 + text[..pos].matches('\n').count()
}

/// Edit tolerant level 2: strip Read's line-number prefixes (^\d+\t or ^\d+: per line).
/// If any line lacks a valid prefix, nothing is stripped (return None); a single trailing \n is treated as a terminator.
fn strip_line_number_prefixes(s: &str) -> Option<String> {
    let (body, trailing_newline) = match s.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (s, false),
    };
    let mut lines = Vec::new();
    for line in body.split('\n') {
        let digit_len = line.bytes().take_while(|b| b.is_ascii_digit()).count();
        let rest = &line[digit_len..];
        let stripped = if digit_len > 0 && (rest.starts_with('\t') || rest.starts_with(':')) {
            &rest[1..]
        } else {
            return None;
        };
        lines.push(stripped);
    }
    let joined = lines.join("\n");
    if joined.is_empty() {
        return None;
    }
    Some(if trailing_newline {
        format!("{joined}\n")
    } else {
        joined
    })
}

/// Curly-quote normalization for Edit tolerant level 3: ‘’→', “”→".
fn normalize_quotes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            _ => c,
        })
        .collect()
}

/// Line-window matching after quote normalization: content and old are each normalized per line; a hit is line-by-line equality.
/// Returns the byte range in content of each hit window (the window covers whole N lines, excluding the trailing newline).
/// A single trailing \n in old is treated as a terminator.
fn find_quote_normalized_windows(content: &str, old: &str) -> Vec<(usize, usize)> {
    let old_body = old.strip_suffix('\n').unwrap_or(old);
    let old_lines: Vec<String> = old_body.split('\n').map(normalize_quotes).collect();
    let n = old_lines.len();
    let content_lines: Vec<&str> = content.split('\n').collect();
    if n == 0 || content_lines.len() < n {
        return Vec::new();
    }
    // Byte start offset of each line (the split('\n') separator is exactly 1 byte)
    let mut offsets = Vec::with_capacity(content_lines.len());
    let mut pos = 0usize;
    for line in &content_lines {
        offsets.push(pos);
        pos += line.len() + 1;
    }
    let normalized: Vec<String> = content_lines
        .iter()
        .map(|line| normalize_quotes(line))
        .collect();
    let mut hits = Vec::new();
    for i in 0..=(content_lines.len() - n) {
        if (0..n).all(|j| normalized[i + j] == old_lines[j]) {
            let start = offsets[i];
            let end = offsets[i + n - 1] + content_lines[i + n - 1].len();
            hits.push((start, end));
        }
    }
    hits
}

/// Follow the file's quote style: when the matched original segment contains curly quotes, new_string's straight quotes are alternately turned curly in order of appearance.
fn follow_quote_style(new_string: &str, original_segment: &str) -> String {
    let curly_double =
        original_segment.contains('\u{201C}') || original_segment.contains('\u{201D}');
    let curly_single =
        original_segment.contains('\u{2018}') || original_segment.contains('\u{2019}');
    if !curly_double && !curly_single {
        return new_string.to_string();
    }
    let mut out = String::with_capacity(new_string.len());
    let mut double_open = true;
    let mut single_open = true;
    for c in new_string.chars() {
        match c {
            '"' if curly_double => {
                out.push(if double_open { '\u{201C}' } else { '\u{201D}' });
                double_open = !double_open;
            }
            '\'' if curly_single => {
                out.push(if single_open { '\u{2018}' } else { '\u{2019}' });
                single_open = !single_open;
            }
            _ => out.push(c),
        }
    }
    out
}
