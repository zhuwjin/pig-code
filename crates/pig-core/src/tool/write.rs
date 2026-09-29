use super::*;

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Write",
                "description": "写入整个文件（自动创建父目录）。大文件优先用 Edit 做局部修改。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作目录的文件路径" },
                        "content": { "type": "string", "description": "完整文件内容" }
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
            let path = args["path"].as_str().ok_or("缺少参数 path")?;
            let content = args["content"].as_str().ok_or("缺少参数 content")?;
            let full = resolve_with_access(ctx.state, ctx.cwd, path, true, FsAccess::Write)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            // 已存在文件沿用其编码/BOM/行尾写回；无法识别（二进制/未知编码）按 UTF-8/LF 覆盖；
            // 新文件一律 UTF-8/LF。存量文件超 Read 上限不允许整文件覆盖（内存护栏同口径）
            if let Some(err) = std::fs::metadata(&full).ok().and_then(|m| {
                file_size_error(
                    m.len(),
                    MAX_READ_FILE_BYTES,
                    "目标文件过大，请改用 Edit 做局部修改",
                )
            }) {
                return Err(err);
            }
            let existing = std::fs::read(&full).ok();
            check_fresh(ctx.state, &full, existing.is_some(), "写入")?;
            let (encoding, bom, line_ending, note) = match &existing {
                Some(bytes) => match crate::text::decode(bytes) {
                    Ok(doc) => {
                        let note = match (
                            doc.encoding != FileEncoding::Utf8,
                            doc.line_ending == LineEnding::Crlf,
                        ) {
                            (true, true) => {
                                format!("（保留原编码 {} / CRLF）", doc.encoding.label())
                            }
                            (true, false) => format!("（保留原编码 {}）", doc.encoding.label()),
                            (false, true) => "（保留原行尾 CRLF）".to_string(),
                            (false, false) => String::new(),
                        };
                        (doc.encoding, doc.bom, doc.line_ending, note)
                    }
                    Err(_) => (
                        FileEncoding::Utf8,
                        false,
                        LineEnding::Lf,
                        "（原文件编码无法识别，已按 UTF-8 覆盖）".to_string(),
                    ),
                },
                None => (FileEncoding::Utf8, false, LineEnding::Lf, String::new()),
            };
            // diff 两侧都用 LF 视图：before = 原文件解码文本（或 ""），after = 归一后的 LF 文本
            let before = existing.as_deref().map(decoded_view).unwrap_or_default();
            let after = content.replace("\r\n", "\n");
            ctx.tracker.snapshot(&full)?;
            let bytes = crate::text::encode(content, encoding, bom, line_ending)?;
            std::fs::write(&full, &bytes)
                .map_err(|e| format!("写入失败 {}: {e}", full.display()))?;
            // 写盘后刷新新鲜度：紧接着再 Edit 自己刚写的文件必须合法
            record_read_state(ctx.state, &full, &bytes, false, None);
            let file_change = ctx.tracker.diff(ctx.cwd, &full).ok();
            let edit_diff = Some(per_edit_diff(ctx.cwd, &full, &before, &after));
            Ok(ToolEffect {
                output: format!("已写入 {}（{} 字节）{note}", path, bytes.len()),
                file_change,
                edit_diff,
                images: vec![],
            })
        })
    }
}

