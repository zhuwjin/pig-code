use super::*;

/// Allowlist glob (ripgrep `-g` / gitignore semantics, matching kimi-code's rg --glob):
/// supports `{a,b}` braces (nestable), `**` across levels, `*` not crossing `/`, `!` prefix blocklist;
/// patterns without `/` match file names at any depth. Unclosed `{`/`[` is a parse error.
/// err_prefix distinguishes the caller's error message.
fn build_overrides(
    root: &Path,
    pattern: &str,
    err_prefix: &str,
) -> Result<ignore::overrides::Override, String> {
    let mut builder = ignore::overrides::OverrideBuilder::new(root);
    builder
        .add(pattern)
        .map_err(|e| format!("{err_prefix} {pattern}: {e}"))?;
    builder
        .build()
        .map_err(|e| format!("{err_prefix} {pattern}: {e}"))
}

impl Tool for Glob {
    fn name(&self) -> &'static str {
        "Glob"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Glob",
                "description": "Find workspace files by name pattern (e.g. **/*.rs), sorted by modification time (newest first). Respects .gitignore/.ignore, includes hidden files; sensitive files (.env, private keys, etc.) are always filtered out. head_limit/offset paginate (default 200 per page; 0 = unlimited).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern (gitignore syntax, same as ripgrep --glob): {a,b} brace expansion (nestable), ** crosses directory levels, ! prefix excludes; * does not cross /; with / it matches paths relative to the search root, without / it matches file names at any depth" },
                        "path": { "type": "string", "description": "Search root directory (relative to the working directory); defaults to the working directory" },
                        "head_limit": { "type": "integer", "description": "Maximum number of files to return (default 200); 0 = unlimited (a character budget still applies)" },
                        "offset": { "type": "integer", "description": "Skip the first N results (for pagination); default 0" }
                    },
                    "required": ["pattern"]
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
            let pattern = args["pattern"]
                .as_str()
                .ok_or("Missing required parameter: pattern")?;
            let head_limit = args["head_limit"]
                .as_u64()
                .unwrap_or(MAX_MATCH_RESULTS as u64) as usize;
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let root = match args["path"].as_str() {
                Some(path) => resolve_with_access(&ctx, path, false, FsAccess::Read)?,
                None => ctx.cwd.to_path_buf(),
            };
            // Filtering during traversal (Override allowlist): non-matching files never enter the result set, and the mtime stat is skipped too
            let overrides = build_overrides(&root, pattern, "Invalid glob pattern")?;
            let mut files = walk_workspace(&root, Some(overrides));
            sort_by_mtime_desc(&mut files);
            // Pagination state machine (same as Grep): want probes 1 extra entry to confirm a next page
            let want = if head_limit == 0 {
                usize::MAX
            } else {
                offset.saturating_add(head_limit).saturating_add(1)
            };
            let mut seen = 0usize;
            let mut more_pages = false;
            let mut budget_hit = false;
            let mut used_chars = 0usize;
            let mut results: Vec<String> = Vec::new();
            let mut filtered_sensitive = 0usize;
            for file in files {
                if is_sensitive_file(&file) {
                    filtered_sensitive += 1;
                    continue;
                }
                seen += 1;
                if seen <= offset {
                    continue;
                }
                if want != usize::MAX && seen >= want {
                    more_pages = true;
                    break;
                }
                // Output relative to cwd (the prefix is kept when the path parameter points to a subdirectory)
                let display = file
                    .strip_prefix(ctx.cwd)
                    .map(|p| p.to_path_buf())
                    .unwrap_or(file);
                let display = display.to_string_lossy().replace('\\', "/");
                used_chars += display.chars().count() + 1;
                if used_chars > MAX_GREP_OUTPUT_CHARS {
                    budget_hit = true;
                    break;
                }
                results.push(display);
            }
            let mut parts: Vec<String> = Vec::new();
            if !results.is_empty() {
                parts.push(results.join("\n"));
            }
            if budget_hit {
                parts.push(format!(
                    "[Output hit the {}-character budget; use a more specific pattern/path, or page with head_limit/offset (next page offset={})]",
                    MAX_GREP_OUTPUT_CHARS,
                    offset + results.len()
                ));
            } else if more_pages {
                parts.push(format!(
                    "[Showing {}-{}; continue with offset={}]",
                    offset + 1,
                    offset + results.len(),
                    offset + results.len()
                ));
            } else if offset > 0 {
                parts.push(format!(
                    "[Showing {}-{} of {seen}]",
                    offset + 1,
                    offset + results.len()
                ));
            }
            if filtered_sensitive > 0 {
                parts.push(format!(
                    "[Filtered out {filtered_sensitive} sensitive file(s)]"
                ));
            }
            let out = if parts.is_empty() {
                "(no matching files)".to_string()
            } else {
                parts.join("\n\n")
            };
            Ok(ToolEffect::plain(out))
        })
    }
}

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "Grep"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Grep",
                "description": "Search workspace file contents with a regular expression; output is file:line: content. Respects .gitignore/.ignore, includes hidden files, skips sensitive files (.env, private keys, etc.); files are searched most-recently-modified first. GBK/UTF-16 files are transcoded automatically before searching (line numbers match Read). head_limit/offset paginate by match; before/after/context attach context lines; output_mode is content (default) / files_with_matches (paths only) / count (matching-line count per file).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regular expression" },
                        "path": { "type": "string", "description": "Directory or single file to search (relative to the working directory); defaults to the working directory" },
                        "include": { "type": "string", "description": "File-name filter glob (e.g. *.rs or *.{rs,toml})" },
                        "ignore_case": { "type": "boolean", "description": "true to ignore case (default false)" },
                        "head_limit": { "type": "integer", "description": "Maximum number of matches to return (matching lines in content mode, files otherwise); default 200; 0 = unlimited (a character budget still applies)" },
                        "offset": { "type": "integer", "description": "Skip the first N matches (for pagination); default 0" },
                        "before": { "type": "integer", "description": "Include N lines of context before each match (content mode only)" },
                        "after": { "type": "integer", "description": "Include N lines of context after each match (content mode only)" },
                        "context": { "type": "integer", "description": "N lines of context on both sides; explicit before/after take precedence" },
                        "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content outputs matching lines (default); files_with_matches lists only matching file paths; count outputs file:matching-line-count (multiple hits on one line count once)" }
                    },
                    "required": ["pattern"]
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
            let pattern = args["pattern"]
                .as_str()
                .ok_or("Missing required parameter: pattern")?;
            let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
            let regex = regex::RegexBuilder::new(pattern)
                .case_insensitive(ignore_case)
                .build()
                .map_err(|e| format!("Invalid regex {pattern}: {e}"))?;
            let include = args["include"].as_str();
            let include_ov = match include {
                Some(inc) => Some(build_overrides(ctx.cwd, inc, "Invalid include pattern")?),
                None => None,
            };
            let output_mode = args["output_mode"].as_str().unwrap_or("content");
            let head_limit = args["head_limit"]
                .as_u64()
                .unwrap_or(MAX_MATCH_RESULTS as u64) as usize;
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            // Context lines apply only in content mode; explicit before/after take precedence over context
            let mut before = args["before"].as_u64().unwrap_or(0) as usize;
            let mut after = args["after"].as_u64().unwrap_or(0) as usize;
            if let Some(both) = args["context"].as_u64().map(|n| n as usize) {
                if args.get("before").is_none() {
                    before = both;
                }
                if args.get("after").is_none() {
                    after = both;
                }
            }
            if output_mode != "content" {
                before = 0;
                after = 0;
            }
            let root = match args["path"].as_str() {
                Some(path) => resolve_with_access(&ctx, path, false, FsAccess::Read)?,
                None => ctx.cwd.to_path_buf(),
            };

            // Search a single file directly; directories go through workspace traversal sorted by mtime descending (truncation keeps the most recently modified files)
            let mut files = Vec::new();
            if root.is_file() {
                files.push(root);
            } else {
                files = walk_workspace(&root, None);
                sort_by_mtime_desc(&mut files);
            }

            // Pagination state machine: the hit unit = line (content) or file (files/count).
            // want probes 1 entry past "the end of this page" to confirm a next page exists; head_limit=0 means no count limit.
            let want = if head_limit == 0 {
                usize::MAX
            } else {
                offset.saturating_add(head_limit).saturating_add(1)
            };
            let mut seen = 0usize; // total hits scanned so far
            let mut page_hits = 0usize; // hits collected for this page so far
            let mut more_pages = false;
            let mut budget_hit = false;
            let mut lines: Vec<String> = Vec::new();
            let mut used_chars = 0usize;
            let (mut skipped_sensitive, mut skipped_undecodable, mut skipped_oversize) =
                (0usize, 0usize, 0usize);

            'files: for file in files {
                if is_sensitive_file(&file) {
                    skipped_sensitive += 1;
                    continue;
                }
                // include compares file names only (gitignore's basename semantics for /-less patterns is exactly equivalent)
                if let Some(ov) = &include_ov {
                    let name = file.file_name().unwrap_or_default();
                    if !ov.matched(Path::new(name), false).is_whitelist() {
                        continue;
                    }
                }
                if file.metadata().map(|m| m.len()).unwrap_or(0) > MAX_GREP_FILE_SIZE {
                    skipped_oversize += 1;
                    continue;
                }
                // Reuse the text pipeline: search after GBK/UTF-16 decoding (line numbers match the Read view);
                // binary/unknown-encoding files are counted as skipped (no longer silently vanishing)
                let doc = match std::fs::read(&file)
                    .ok()
                    .and_then(|b| crate::text::decode(&b).ok())
                {
                    Some(doc) => doc,
                    None => {
                        skipped_undecodable += 1;
                        continue;
                    }
                };
                let content: Vec<&str> = doc.text.lines().collect();
                let relative = file
                    .strip_prefix(ctx.cwd)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| file.clone());
                let relative = relative.to_string_lossy().replace('\\', "/");

                match output_mode {
                    "files_with_matches" => {
                        if !content.iter().any(|l| regex.is_match(l)) {
                            continue;
                        }
                        seen += 1;
                        if seen <= offset {
                            continue;
                        }
                        if want != usize::MAX && seen >= want {
                            more_pages = true;
                            break 'files;
                        }
                        used_chars += relative.chars().count() + 1;
                        if used_chars > MAX_GREP_OUTPUT_CHARS {
                            budget_hit = true;
                            break 'files;
                        }
                        lines.push(relative.clone());
                        page_hits += 1;
                    }
                    "count" => {
                        // rg -c semantics: count matching lines; multiple hits on one line count once
                        let count = content.iter().filter(|l| regex.is_match(l)).count();
                        if count == 0 {
                            continue;
                        }
                        seen += 1;
                        if seen <= offset {
                            continue;
                        }
                        if want != usize::MAX && seen >= want {
                            more_pages = true;
                            break 'files;
                        }
                        let row = format!("{relative}:{count}");
                        used_chars += row.chars().count() + 1;
                        if used_chars > MAX_GREP_OUTPUT_CHARS {
                            budget_hit = true;
                            break 'files;
                        }
                        lines.push(row);
                        page_hits += 1;
                    }
                    _ => {
                        // Collect this page's hit line numbers (ascending)
                        let mut page_hit_lines: Vec<usize> = Vec::new();
                        for (ix, line) in content.iter().enumerate() {
                            if !regex.is_match(line) {
                                continue;
                            }
                            seen += 1;
                            if seen <= offset {
                                continue;
                            }
                            if want != usize::MAX && seen >= want {
                                more_pages = true;
                                break;
                            }
                            page_hit_lines.push(ix);
                        }
                        if page_hit_lines.is_empty() {
                            // No hits for this page (all skipped by offset); if the next-page probe already triggered, stop right here
                            if more_pages {
                                break 'files;
                            }
                            continue;
                        }
                        if before == 0 && after == 0 {
                            for &ix in &page_hit_lines {
                                let row = format!(
                                    "{relative}:{}: {}",
                                    ix + 1,
                                    truncate_grep_line(content[ix])
                                );
                                used_chars += row.chars().count() + 1;
                                if used_chars > MAX_GREP_OUTPUT_CHARS {
                                    budget_hit = true;
                                    break 'files;
                                }
                                lines.push(row);
                            }
                        } else {
                            // Hit windows rendered merged: adjacent (including touching) windows are merged,
                            // with a bare rg-style -- separator between non-adjacent windows
                            let mut windows: Vec<(usize, usize)> = Vec::new();
                            for &ix in &page_hit_lines {
                                let start = ix.saturating_sub(before);
                                let end = (ix + after).min(content.len() - 1);
                                match windows.last_mut() {
                                    Some(last) if start <= last.1.saturating_add(1) => {
                                        last.1 = last.1.max(end);
                                    }
                                    _ => windows.push((start, end)),
                                }
                            }
                            let mut first_window = true;
                            for (start, end) in windows {
                                if !first_window {
                                    used_chars += 3;
                                    if used_chars > MAX_GREP_OUTPUT_CHARS {
                                        budget_hit = true;
                                        break 'files;
                                    }
                                    lines.push("--".to_string());
                                }
                                first_window = false;
                                for (window_ix, line) in content[start..=end].iter().enumerate() {
                                    let row = format!(
                                        "{relative}:{}: {}",
                                        start + window_ix + 1,
                                        truncate_grep_line(line)
                                    );
                                    used_chars += row.chars().count() + 1;
                                    if used_chars > MAX_GREP_OUTPUT_CHARS {
                                        budget_hit = true;
                                        break 'files;
                                    }
                                    lines.push(row);
                                }
                            }
                        }
                        page_hits += page_hit_lines.len();
                        // Finish rendering this file before exiting (the probe hit and this page's hits are often in the same file)
                        if more_pages {
                            break 'files;
                        }
                    }
                }
            }

            let unit = if output_mode == "content" {
                "lines"
            } else {
                "files"
            };
            let mut footers: Vec<String> = Vec::new();
            if budget_hit {
                footers.push(format!(
                    "[Output hit the {}-character budget and stopped early; narrow the search, or page with head_limit/offset (next page offset={})]",
                    MAX_GREP_OUTPUT_CHARS,
                    offset + page_hits
                ));
            } else if more_pages {
                footers.push(format!(
                    "[Showing {}-{} {unit}; continue with offset={}]",
                    offset + 1,
                    offset + page_hits,
                    offset + page_hits
                ));
            } else if offset > 0 {
                footers.push(format!(
                    "[Showing {}-{} {unit} of {seen}]",
                    offset + 1,
                    offset + page_hits
                ));
            }
            let mut skipped_parts: Vec<String> = Vec::new();
            if skipped_sensitive > 0 {
                skipped_parts.push(format!("sensitive {skipped_sensitive}"));
            }
            if skipped_undecodable > 0 {
                skipped_parts.push(format!("binary/undecodable {skipped_undecodable}"));
            }
            if skipped_oversize > 0 {
                skipped_parts.push(format!("over 2MB {skipped_oversize}"));
            }
            if !skipped_parts.is_empty() {
                footers.push(format!("[Skipped: {}]", skipped_parts.join(", ")));
            }

            let mut out: Vec<String> = if lines.is_empty() {
                vec![if offset > 0 && seen > 0 {
                    format!("(offset={offset} is beyond the total of {seen} matches)")
                } else {
                    "(no matches)".to_string()
                }]
            } else {
                lines
            };
            out.extend(footers);
            Ok(ToolEffect::plain(out.join("\n")))
        })
    }
}

/// Workspace traversal (the same ignore engine as ripgrep): respects .gitignore/.ignore/.git exclude,
/// includes hidden files, but always skips VCS directories (.git/.svn/.hg/.bzr/.jj/.sl) and .pigcode.
/// Collects file paths only. When `overrides` is an allowlist glob (the Glob tool), non-matching files
/// are excluded during traversal.
pub(crate) fn walk_workspace(
    root: &Path,
    overrides: Option<ignore::overrides::Override>,
) -> Vec<PathBuf> {
    const SKIP_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl", ".pigcode"];
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .filter_entry(|entry| {
            !(entry.file_type().is_some_and(|t| t.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| SKIP_DIRS.contains(&name)))
        });
    if let Some(overrides) = overrides {
        builder.overrides(overrides);
    }
    builder
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| entry.into_path())
        .collect()
}

/// mtime (nanoseconds since the UNIX epoch); 0 when unavailable
fn mtime_nanos(path: &Path) -> u128 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Same sort as ZCode/kimi: mtime descending, ties broken by path lexicographic ascending
fn sort_by_mtime_desc(files: &mut Vec<PathBuf>) {
    let mut stamped: Vec<(u128, PathBuf)> = std::mem::take(files)
        .into_iter()
        .map(|path| (mtime_nanos(&path), path))
        .collect();
    stamped.sort_by(|(ma, pa), (mb, pb)| mb.cmp(ma).then_with(|| pa.cmp(pb)));
    files.extend(stamped.into_iter().map(|(_, path)| path));
}

/// @ file search: traverses the workspace (respects .gitignore, includes hidden files, skips VCS directories), scored and sorted by substring match.
pub fn search_files(cwd: &Path, query: &str, limit: usize) -> Vec<String> {
    let files = walk_workspace(cwd, None);
    let query = query.to_lowercase();
    let mut scored: Vec<(u64, String)> = files
        .iter()
        .filter_map(|file| {
            let relative = file
                .strip_prefix(cwd)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            if query.is_empty() {
                return Some((relative.len() as u64 + 1000, relative));
            }
            let lower = relative.to_lowercase();
            lower
                .find(&query)
                .map(|pos| (pos as u64 * 1000 + relative.len() as u64, relative))
        })
        .collect();
    scored.sort();
    scored.truncate(limit);
    scored.into_iter().map(|(_, path)| path).collect()
}
