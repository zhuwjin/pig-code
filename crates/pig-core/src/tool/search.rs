use super::*;

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
                "description": "按文件名模式匹配工作区文件（如 **/*.rs）。尊重 .gitignore/.ignore，包含隐藏文件，按最近修改排序；敏感文件（.env/私钥等）自动过滤。head_limit/offset 分页（默认每页 200，0 = 不限）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "glob 模式；含 / 时按相对路径匹配，不含 / 时只匹配文件名" },
                        "path": { "type": "string", "description": "搜索根目录（相对工作目录），默认为工作目录" },
                        "head_limit": { "type": "integer", "description": "最多返回多少个文件，默认 200；0 = 不限条数（仍有字符预算兜底）" },
                        "offset": { "type": "integer", "description": "跳过前 N 个结果（分页续读），默认 0" }
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
            let pattern = args["pattern"].as_str().ok_or("缺少参数 pattern")?;
            let glob_pattern = glob::Pattern::new(pattern)
                .map_err(|e| format!("无效 glob 模式 {pattern}: {e}"))?;
            let head_limit =
                args["head_limit"].as_u64().unwrap_or(MAX_MATCH_RESULTS as u64) as usize;
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let root = match args["path"].as_str() {
                Some(path) => resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Read)?,
                None => ctx.cwd.to_path_buf(),
            };
            let mut files = walk_workspace(&root);
            sort_by_mtime_desc(&mut files);
            let match_by_path = pattern.contains('/');
            // 分页状态机（Grep 同款）：want 多探 1 个确认下一页
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
                // 含 / 的模式匹配相对 root 的完整路径；不含 / 只比文件名
                let relative = file
                    .strip_prefix(&root)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| file.clone());
                let matched = if match_by_path {
                    glob_pattern.matches_path(&relative)
                } else {
                    file.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| glob_pattern.matches(name))
                };
                if !matched {
                    continue;
                }
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
                // 输出相对 cwd（path 参数指向子目录时保留前缀）
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
                    "[输出已达 {} 字符上限；用更具体的 pattern/path，或 head_limit/offset 分页（下一页 offset={}）]",
                    MAX_GREP_OUTPUT_CHARS,
                    offset + results.len()
                ));
            } else if more_pages {
                parts.push(format!(
                    "[显示 {}-{} 个，用 offset={} 续读]",
                    offset + 1,
                    offset + results.len(),
                    offset + results.len()
                ));
            } else if offset > 0 {
                parts.push(format!(
                    "[显示 {}-{} 个，共 {seen} 个]",
                    offset + 1,
                    offset + results.len()
                ));
            }
            if filtered_sensitive > 0 {
                parts.push(format!("[已过滤 {filtered_sensitive} 个敏感文件]"));
            }
            let out = if parts.is_empty() {
                "（无匹配文件）".to_string()
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
                "description": "用正则搜索工作区文件内容，输出 文件:行号: 内容。尊重 .gitignore/.ignore，包含隐藏文件，跳过敏感文件（.env/私钥等），文件按最近修改优先；GBK/UTF-16 文件自动转码搜索（行号与 Read 一致）。head_limit/offset 按命中分页；before/after/context 附上下文行；output_mode 可选 content（默认）/files_with_matches（只列路径）/count（每文件命中行数）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "正则表达式" },
                        "path": { "type": "string", "description": "搜索目录或单文件（相对工作目录），默认工作目录" },
                        "include": { "type": "string", "description": "文件名过滤 glob（如 *.rs）" },
                        "ignore_case": { "type": "boolean", "description": "true 时忽略大小写（默认 false）" },
                        "head_limit": { "type": "integer", "description": "最多返回多少个命中（content 按命中行、其余按文件），默认 200；0 = 不限条数（仍有字符预算兜底）" },
                        "offset": { "type": "integer", "description": "跳过前 N 个命中（分页续读），默认 0" },
                        "before": { "type": "integer", "description": "每个命中前附带 N 行上下文（仅 content 模式）" },
                        "after": { "type": "integer", "description": "每个命中后附带 N 行上下文（仅 content 模式）" },
                        "context": { "type": "integer", "description": "前后各 N 行上下文；显式 before/after 优先" },
                        "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content 输出命中行（默认）；files_with_matches 只列命中文件路径；count 输出 每文件:命中行数（一行多次命中算 1）" }
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
            let pattern = args["pattern"].as_str().ok_or("缺少参数 pattern")?;
            let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
            let regex = regex::RegexBuilder::new(pattern)
                .case_insensitive(ignore_case)
                .build()
                .map_err(|e| format!("无效正则 {pattern}: {e}"))?;
            let include = args["include"].as_str().map(|s| s.to_string());
            let output_mode = args["output_mode"].as_str().unwrap_or("content");
            let head_limit =
                args["head_limit"].as_u64().unwrap_or(MAX_MATCH_RESULTS as u64) as usize;
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            // 上下文行仅 content 模式生效；显式 before/after 优先于 context
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
                Some(path) => resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Read)?,
                None => ctx.cwd.to_path_buf(),
            };

            // 单文件直接搜；目录走工作区遍历并按 mtime 降序（截断时保留最近改动的文件）
            let mut files = Vec::new();
            if root.is_file() {
                files.push(root);
            } else {
                files = walk_workspace(&root);
                sort_by_mtime_desc(&mut files);
            }

            // 分页状态机：命中单位 = 行（content）或文件（files/count）。
            // want 比「本页末尾」多探 1 个用于确认还有下一页；head_limit=0 不限条数。
            let want = if head_limit == 0 {
                usize::MAX
            } else {
                offset.saturating_add(head_limit).saturating_add(1)
            };
            let mut seen = 0usize; // 已扫过的命中总数
            let mut page_hits = 0usize; // 本页已收集的命中数
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
                if let Some(include) = &include {
                    let name = file.file_name().unwrap_or_default().to_string_lossy();
                    let glob_pattern = glob::Pattern::new(include)
                        .map_err(|e| format!("无效 include 模式 {include}: {e}"))?;
                    if !glob_pattern.matches(&name) {
                        continue;
                    }
                }
                if file.metadata().map(|m| m.len()).unwrap_or(0) > MAX_GREP_FILE_SIZE {
                    skipped_oversize += 1;
                    continue;
                }
                // 复用文本管线：GBK/UTF-16 解码后搜索（行号与 Read 视图一致），
                // 二进制/未知编码计数跳过（不再静默消失）
                let doc =
                    match std::fs::read(&file).ok().and_then(|b| crate::text::decode(&b).ok()) {
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
                        // rg -c 口径：计命中行数，一行多次命中算 1
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
                        // 收集本页命中的行号（升序）
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
                            // 无本页命中（全被 offset 跳过）；已触发下一页探测则就此打住
                            if more_pages {
                                break 'files;
                            }
                            continue;
                        }
                        if before == 0 && after == 0 {
                            for &ix in &page_hit_lines {
                                let row =
                                    format!("{relative}:{}: {}", ix + 1, truncate_grep_line(content[ix]));
                                used_chars += row.chars().count() + 1;
                                if used_chars > MAX_GREP_OUTPUT_CHARS {
                                    budget_hit = true;
                                    break 'files;
                                }
                                lines.push(row);
                            }
                        } else {
                            // 命中窗口合并渲染：相邻（含相接）窗口合并，
                            // 不相邻窗口之间以 rg 风格的裸 -- 分隔
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
                                for (window_ix, line) in
                                    content[start..=end].iter().enumerate()
                                {
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
                        // 渲染完本文件再退出（probe 命中与本页命中常在同一文件）
                        if more_pages {
                            break 'files;
                        }
                    }
                }
            }

            let unit = if output_mode == "content" {
                "行"
            } else {
                "个文件"
            };
            let mut footers: Vec<String> = Vec::new();
            if budget_hit {
                footers.push(format!(
                    "[输出已达 {} 字符上限，提前停止；缩小搜索范围，或用 head_limit/offset 分页（下一页 offset={}）]",
                    MAX_GREP_OUTPUT_CHARS,
                    offset + page_hits
                ));
            } else if more_pages {
                footers.push(format!(
                    "[显示 {}-{} {unit}，用 offset={} 续读]",
                    offset + 1,
                    offset + page_hits,
                    offset + page_hits
                ));
            } else if offset > 0 {
                footers.push(format!(
                    "[显示 {}-{} {unit}，共 {seen} {unit}]",
                    offset + 1,
                    offset + page_hits
                ));
            }
            let mut skipped_parts: Vec<String> = Vec::new();
            if skipped_sensitive > 0 {
                skipped_parts.push(format!("敏感 {skipped_sensitive}"));
            }
            if skipped_undecodable > 0 {
                skipped_parts.push(format!("二进制/未知编码 {skipped_undecodable}"));
            }
            if skipped_oversize > 0 {
                skipped_parts.push(format!("超过 2MB {skipped_oversize}"));
            }
            if !skipped_parts.is_empty() {
                footers.push(format!("[已跳过: {}]", skipped_parts.join("、")));
            }

            let mut out: Vec<String> = if lines.is_empty() {
                vec![if offset > 0 && seen > 0 {
                    format!("（offset={offset} 超出命中总数 {seen}）")
                } else {
                    "（无匹配内容）".to_string()
                }]
            } else {
                lines
            };
            out.extend(footers);
            Ok(ToolEffect::plain(out.join("\n")))
        })
    }
}

/// 工作区遍历（ripgrep 同款 ignore 引擎）：尊重 .gitignore/.ignore/.git exclude，
/// 包含隐藏文件，但始终跳过 VCS 目录（.git/.svn/.hg/.bzr/.jj/.sl）与 .pigcode。
/// 只收集文件路径。
pub(crate) fn walk_workspace(root: &Path) -> Vec<PathBuf> {
    const SKIP_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl", ".pigcode"];
    ignore::WalkBuilder::new(root)
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
        })
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| entry.into_path())
        .collect()
}

/// mtime（纳秒，UNIX 纪元起）；取不到当 0
fn mtime_nanos(path: &Path) -> u128 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// ZCode/kimi 同款排序：mtime 降序，同 mtime 按路径字典序升序
fn sort_by_mtime_desc(files: &mut Vec<PathBuf>) {
    let mut stamped: Vec<(u128, PathBuf)> = std::mem::take(files)
        .into_iter()
        .map(|path| (mtime_nanos(&path), path))
        .collect();
    stamped.sort_by(|(ma, pa), (mb, pb)| mb.cmp(ma).then_with(|| pa.cmp(pb)));
    files.extend(stamped.into_iter().map(|(_, path)| path));
}


/// @ 文件搜索：遍历工作区（尊重 .gitignore、含隐藏文件、跳过 VCS 目录），按子串匹配打分排序。
pub fn search_files(cwd: &Path, query: &str, limit: usize) -> Vec<String> {
    let files = walk_workspace(cwd);
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
