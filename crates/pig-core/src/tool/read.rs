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
                "description": "读取工作区内文件内容，输出带「行号\\t」前缀。path 相对工作目录；文件过长用 offset/limit 分页（单次约 10 万字符上限）；单行超 2000 字符用 column_offset 续读。UTF-16/GBK 文件自动转码显示，二进制文件会拒绝。超过 100MB 的文件会拒绝（用 Grep 定位或 Bash 分段查看）。相同参数重读未变化的文件会返回「文件未变化」而不重复输出全文。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作目录的文件路径" },
                        "offset": { "type": "integer", "description": "起始行号（从 1 开始），默认 1" },
                        "limit": { "type": "integer", "description": "最多读取行数，默认 2000" },
                        "column_offset": { "type": "integer", "description": "每行起始字符列（0 起）。用于续读超 2000 字符的长行——截断提示会给出下一页的 column_offset 值" }
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
            let path = args["path"].as_str().ok_or("缺少参数 path")?;
            let full = resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Read)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            // 体积护栏：先 stat 再读盘，防整文件进内存（NotFound 仍走引导文案）
            let meta = std::fs::metadata(&full).map_err(|e| read_io_error(path, &full, e))?;
            if let Some(err) = file_size_error(
                meta.len(),
                MAX_READ_FILE_BYTES,
                "请用 Grep 搜索关键内容，或用 Bash（head/tail/grep）分段查看",
            ) {
                return Err(err);
            }
            let bytes = std::fs::read(&full).map_err(|e| read_io_error(path, &full, e))?;
            let doc = match crate::text::decode(&bytes) {
                Ok(doc) => doc,
                Err(error) => {
                    // 图片给明确指引（魔数嗅探，不信任扩展名）
                    if let Some(mime) = sniff_image(&bytes) {
                        let label = match mime {
                            "image/png" => "PNG",
                            "image/jpeg" => "JPEG",
                            "image/gif" => "GIF",
                            "image/webp" => "WebP",
                            _ => mime,
                        };
                        return Err(format!(
                            "这是 {label} 图片，请改用 ReadMediaFile 读取（当前模型需支持图片输入）"
                        ));
                    }
                    return Err(error);
                }
            };
            if doc.text.is_empty() {
                record_read_state(ctx.state, &full, &bytes, false, None);
                return Ok(ToolEffect::plain("（空文件）".to_string()));
            }
            let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(MAX_READ_LINES as u64) as usize;
            let column_offset = args["column_offset"].as_u64().unwrap_or(0) as usize;
            // 重复读短路（ZCode file_unchanged 同款）：同视图参数 + 内容 hash 未变
            // → 不再输出全文，省 token。刷新流程用的是同一条 read_states 记录，
            // hash 一致意味着新鲜度信息无需更新。
            let view = (offset, limit, column_offset);
            let unchanged = match ctx.state.read_states.lock() {
                Ok(states) => states.get(&full).is_some_and(|r| {
                    // 截断输出对同参数是确定性的，预算截断的读同样可短路
                    r.hash == hash_bytes(&bytes) && r.view == Some(view)
                }),
                Err(_) => false,
            };
            if unchanged {
                return Ok(ToolEffect::plain(
                    "（文件未变化：与上次 Read 参数相同且内容一致，无需重复读取）".to_string(),
                ));
            }
            let lines: Vec<&str> = doc.text.lines().collect();
            let total = lines.len();
            let start = (offset - 1).min(total);
            // 逐行渲染（带行号），行数上限与字符预算（含行号前缀）先到先停；
            // 超长行按 column_offset 起读，截断提示带续读参数
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
                        "{taken} [...本行未完，已读第 {}-{next} 字符（共 {line_chars}），用 column_offset={next} 续读]",
                        column_offset + 1
                    )
                } else if column_offset > 0 {
                    let taken: String = line.chars().skip(column_offset).collect();
                    format!(
                        "{taken} [本行第 {}-{line_chars} 字符（共 {line_chars}）]",
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
                    "\n\n[已截断: 显示 {}-{end} 行，共 {total} 行；用 offset 参数继续读取]",
                    start + 1
                ));
            }
            // 元信息：仅非默认编码/行尾或 lossy 时提示
            if doc.encoding != FileEncoding::Utf8 || doc.line_ending == LineEnding::Crlf {
                let mut parts = vec![format!("编码={}", doc.encoding.label())];
                if doc.line_ending == LineEnding::Crlf {
                    parts.push("行尾=CRLF（已转为 LF 显示，写回时还原）".to_string());
                }
                out.push_str(&format!("\n\n[文件信息: {}]", parts.join(", ")));
            }
            if doc.lossy {
                out.push_str("\n[警告: 解码存在替换字符，编码识别可能有误]");
            }
            // ZCode 口径：只有被预算截断的「整读」才算 partial；显式分页读不算
            let paged = args.get("offset").is_some() || args.get("limit").is_some();
            record_read_state(ctx.state, &full, &bytes, !paged && end < total, Some(view));
            Ok(ToolEffect::plain(out))
        })
    }
}

/// Read/Edit 共用的读盘报错：文件不存在时附父目录下最多 20 个文件名，引导模型修正路径
pub(crate) fn read_io_error(path: &str, full: &Path, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        let mut message = format!("文件不存在: {path}");
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
                    "。目录 {} 下有: {}",
                    parent.display(),
                    names.join(", ")
                ));
            }
        }
        message
    } else {
        format!("读取失败 {}: {error}", full.display())
    }
}

/// 文件内容指纹（原始字节的 DefaultHasher）
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash_slice(bytes, &mut hasher);
    hasher.finish()
}

/// Read 成功登记 / Write/Edit 写盘后刷新 新鲜度状态（mtime 取写盘后的新值）。
/// view：Read 携带本次视图参数（供重复读短路）；内部刷新传 None。
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

/// 写前新鲜度检查（ZCode read-file-state 同款）：文件不存在（新建）放行；
/// 未读过 / 上次是不完整视图 / 读后磁盘被外部改动，一律拒绝。
/// mtime 或 size 有变化才比 hash；hash 相同（内容逐字未变）放行并顺手更新状态。
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
                "文件已存在且本会话尚未读过；为避免覆盖他人改动，请先 Read 再{verb}"
            ));
        };
        (read.mtime, read.size, read.hash, read.partial)
    };
    if partial {
        return Err(
            "上次 Read 是不完整视图（输出被截断）；请用 offset/limit 分页读完或完整 Read 后再改"
                .to_string(),
        );
    }
    let meta =
        std::fs::metadata(full).map_err(|e| format!("读取文件状态失败 {}: {e}", full.display()))?;
    if meta.modified().ok() == read_mtime && meta.len() == read_size {
        return Ok(());
    }
    let bytes = std::fs::read(full).map_err(|e| format!("读取失败 {}: {e}", full.display()))?;
    if hash_bytes(&bytes) == read_hash {
        record_read_state(state, full, &bytes, false, None);
        return Ok(());
    }
    Err("文件自上次 Read 后已被外部修改，请先重新 Read 再改（避免覆盖他人改动）".to_string())
}
