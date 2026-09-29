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
                "description": "精确替换文件中的文本。old_string 必须在文件中唯一出现（replace_all=true 时替换全部出现）；先 Read 确认内容再改。行号前缀、弯直引号、字面 \\n 等转义序列的常见笔误有容错匹配（命中时输出会注明）。保留原文件的编码与行尾。超过 50MB 的文件会拒绝（大文件请用 Bash sed/awk）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作目录的文件路径" },
                        "old_string": { "type": "string", "description": "要被替换的原文（须唯一出现；replace_all=true 时替换全部）" },
                        "new_string": { "type": "string", "description": "替换后的新文本；为空且 old_string 占整行时连行尾换行一起删除，不留空行" },
                        "replace_all": { "type": "boolean", "description": "true 时替换全部匹配（默认 false，要求唯一出现）" }
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
            let path = args["path"].as_str().ok_or("缺少参数 path")?;
            let old = args["old_string"].as_str().ok_or("缺少参数 old_string")?;
            let new = args["new_string"].as_str().ok_or("缺少参数 new_string")?;
            if old.is_empty() {
                return Err("old_string 不能为空；要创建文件请用 Write".to_string());
            }
            if old == new {
                return Err("old_string 与 new_string 相同，无需修改".to_string());
            }
            let replace_all = args["replace_all"].as_bool().unwrap_or(false);
            let full = resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Write)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            // 体积护栏放在新鲜度检查之前：大文件直接拒绝，不进整读整写
            if let Some(err) = std::fs::metadata(&full).ok().and_then(|m| {
                file_size_error(
                    m.len(),
                    MAX_EDIT_FILE_BYTES,
                    "不适合整文件编辑；请用 Bash（sed/awk）做局部修改，或先拆分文件",
                )
            }) {
                return Err(err);
            }
            check_fresh(ctx.state, &full, full.exists(), "修改")?;
            let bytes = std::fs::read(&full).map_err(|e| read_io_error(path, &full, e))?;
            let doc = crate::text::decode(&bytes)?;
            let content = doc.text;
            // 匹配+替换计算抽在 compute_edit（审批预览共用，预览即所得）
            let outcome = match compute_edit(&content, old, new, replace_all) {
                Ok(outcome) => outcome,
                Err(EditMatchError::NotFound) => {
                    let mut message = format!(
                        "old_string 在 {path} 中未找到。请先用 Read 确认文件当前内容（注意缩进与换行需完全一致）。"
                    );
                    if doc.line_ending == LineEnding::Crlf {
                        message.push_str(
                            "该文件为 CRLF 行尾，Read 输出已转为 LF，old_string 请用 LF 换行。",
                        );
                    }
                    return Err(message);
                }
                Err(EditMatchError::NotUnique { count }) => {
                    return Err(format!(
                        "old_string 在 {path} 中出现 {count} 次，无法唯一定位。请扩大 old_string 范围使其唯一；如需全部替换，设 replace_all=true。"
                    ));
                }
            };
            ctx.tracker.snapshot(&full)?;
            // 匹配与替换都在 LF 视图（解码已归一）上做；写回时还原原编码/行尾
            let after = outcome.after;
            let encoded = crate::text::encode(&after, doc.encoding, doc.bom, doc.line_ending)?;
            std::fs::write(&full, &encoded)
                .map_err(|e| format!("写入失败 {}: {e}", full.display()))?;
            // 写盘后刷新新鲜度：紧接着再改自己刚写的文件必须合法
            record_read_state(ctx.state, &full, &encoded, false, None);
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            let edit_diff = Some(per_edit_diff(ctx.cwd, &full, &content, &after));
            let output = if replace_all {
                format!("已修改 {path}（替换 {} 处）", outcome.replaced)
            } else if let Some(note) = outcome.tier_note {
                format!("已修改 {path}（{note}）")
            } else {
                format!("已修改 {path}")
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

/// Edit 匹配失败的两种形态（报错文案在调用方拼，那里才有 path/行尾上下文）
#[derive(Debug)]
pub enum EditMatchError {
    NotFound,
    NotUnique { count: usize },
}

/// compute_edit 的产物：替换后文本、替换处数、容错梯队命中说明
pub struct EditOutcome {
    pub after: String,
    pub replaced: usize,
    /// 「容错匹配：已剥离行号前缀」/「容错匹配：引号风格已跟随文件」；精确命中为 None
    pub tier_note: Option<&'static str>,
}

/// Edit 的匹配+替换计算（LF 视图）：精确 → 剥离 Read 行号前缀 → 引号归一 →
/// 反转义归一 四级梯队，每级各自做唯一性检查；replace_all 只走精确
/// （宽匹配仅做单次替换）。审批预览与 Edit 执行共用，保证「预览即所得」。
pub fn compute_edit(
    content_lf: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<EditOutcome, EditMatchError> {
    let mut effective_old = old.to_string();
    let mut effective_new = new.to_string();
    let mut quote_window: Option<(usize, usize)> = None;
    let mut tier_note: Option<&'static str> = None;
    let mut count = content_lf.matches(old).count();
    if count == 0 && !replace_all {
        if let Some(stripped) = strip_line_number_prefixes(old) {
            let stripped_count = content_lf.matches(&stripped).count();
            if stripped_count > 0 {
                count = stripped_count;
                effective_old = stripped;
                tier_note = Some("容错匹配：已剥离行号前缀");
            }
        }
        if count == 0 {
            let windows = find_quote_normalized_windows(content_lf, old);
            if windows.len() == 1 {
                let (start, end) = windows[0];
                count = 1;
                quote_window = Some((start, end));
                effective_new = follow_quote_style(new, &content_lf[start..end]);
                tier_note = Some("容错匹配：引号风格已跟随文件");
            } else if windows.len() > 1 {
                count = windows.len();
            }
        }
        // 第 4 级：反转义归一——模型把字面 \n\t\r 等写进 old_string
        //（从字符串字面量/JSON 复制时常见）；new_string 同步反转义（ZCode 同款）。
        // 出现未识别转义或尾部孤立反斜杠时不应用（宁可不匹配也不乱改）。
        if count == 0
            && let Some(old_un) = unescape_literal(old)
        {
            let new_un = unescape_literal(new).unwrap_or_else(|| new.to_string());
            let un_count = content_lf.matches(&old_un).count();
            if un_count > 0 {
                count = un_count;
                effective_old = old_un;
                effective_new = new_un;
                tier_note = Some("容错匹配：已反转义字面转义序列");
            }
        }
    }
    if count == 0 {
        return Err(EditMatchError::NotFound);
    }
    if count > 1 && !replace_all {
        return Err(EditMatchError::NotUnique { count });
    }
    let (after, replaced) = match quote_window {
        // 引号归一命中：整段替换原文那 N 行
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


/// 手动扫描替换（不用 String::replace，实现 ZCode 同款删除优化）：
/// new 为空、old 不以 \n 结尾、且匹配位置后紧跟 \n 时，连这个 \n 一起删，不留空行。
/// 返回 (替换后文本, 替换处数)。
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

/// Edit 容错第 4 级：反转义字面转义序列（\n \t \r \" \' \` \$ \\ → 真实字符）。
/// 未识别的转义组合保持原样；尾部孤立反斜杠返回 None（无法安全解释）；
/// 完全不含转义序列也返回 None（该级无需参与）。
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

/// Edit 容错第 2 级：剥离 Read 输出的行号前缀（每行 ^\d+\t 或 ^\d+:）。
/// 任一行不带合法前缀则整体不剥离（返回 None）；行尾单个 \n 视为终止符。
fn strip_line_number_prefixes(s: &str) -> Option<String> {
    let (body, trailing_newline) = match s.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (s, false),
    };
    let mut lines = Vec::new();
    for line in body.split('\n') {
        let digit_len = line.bytes().take_while(|b| b.is_ascii_digit()).count();
        let rest = &line[digit_len..];
        let stripped = if digit_len > 0 && rest.starts_with('\t') {
            &rest[1..]
        } else if digit_len > 0 && rest.starts_with(':') {
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

/// Edit 容错第 3 级的弯引号归一：‘’→'，“”→"。
fn normalize_quotes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            _ => c,
        })
        .collect()
}

/// 引号归一后按行窗口匹配：content 与 old 各按行归一，逐行相等即命中。
/// 返回各命中窗口在 content 里的字节区间（窗口覆盖整 N 行，不含尾随换行）。
/// old 行尾单个 \n 视为终止符。
fn find_quote_normalized_windows(content: &str, old: &str) -> Vec<(usize, usize)> {
    let old_body = old.strip_suffix('\n').unwrap_or(old);
    let old_lines: Vec<String> = old_body.split('\n').map(normalize_quotes).collect();
    let n = old_lines.len();
    let content_lines: Vec<&str> = content.split('\n').collect();
    if n == 0 || content_lines.len() < n {
        return Vec::new();
    }
    // 每行的字节起始偏移（split('\n') 的分隔符恰为 1 字节）
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

/// 跟随文件引号风格：原文匹配段含弯引号时，把 new_string 的直引号按出现顺序交替转弯。
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

