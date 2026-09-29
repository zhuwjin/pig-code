use super::*;

impl Session {
    /// 共享的门控执行（父会话通用路径与子代理循环复用）：薄封装——
    /// 按字段借用组装 GateCtx，转发自由函数 exec_tool_gated_ctx（前后台同一门控）。
    pub(crate) async fn exec_tool_gated(
        &mut self,
        call: &ToolCall,
        tool: Option<&dyn tool::Tool>,
        item_id: &str,
        turn_id: &str,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> GatedToolOutcome {
        let mut gate = GateCtx {
            cwd: &self.cwd,
            mode: self.mode,
            tracker: &mut self.tracker,
            state: &self.state,
            pending: &self.pending,
            permissions: &self.permissions,
            always_allowed: &mut self.always_allowed,
            session_id: &self.id,
            seq: &self.seq,
            store: &self.store,
        };
        exec_tool_gated_ctx(&mut gate, call, tool, item_id, turn_id, tx, cancel).await
    }


}

/// 模式中文名（工具结果文案用；与 app 侧 EXEC_MODES 的标签一致）
pub(crate) fn exec_mode_label(mode: ExecMode) -> &'static str {
    match mode {
        ExecMode::ConfirmBeforeEdit => "变更前确认",
        ExecMode::AutoEdit => "自动编辑",
        ExecMode::Plan => "计划",
        ExecMode::FullAccess => "完全访问",
        ExecMode::Yolo => "无管制",
    }
}

/// 审批弹窗详情（pub 供集成测试直接断言）。
/// Write/Edit 走文本管线：resolve_checked 解析路径（失败回退 join）、字节 →
/// text::decode → LF 视图算 diff（预览与真实写回一致）；Edit 走 compute_edit
///（replace_all 感知、容错梯队命中会注明）。解码失败回退直读 + replacen 的旧逻辑。
pub fn approval_detail(
    call: &ToolCall,
    cwd: &std::path::Path,
    danger_reason: Option<&str>,
) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    match call.name.as_str() {
        "Bash" => {
            let command = args["command"].as_str().unwrap_or("?");
            match danger_reason {
                Some(reason) => format!("⚠️ 高风险命令：{reason}\n\n{command}"),
                None => command.to_string(),
            }
        }
        "Write" => {
            let path = args["path"].as_str().unwrap_or("?");
            let content = args["content"].as_str().unwrap_or("");
            let full =
                crate::tool::resolve_checked(cwd, path, false).unwrap_or_else(|_| cwd.join(path));
            // before 用 LF 视图（GBK/UTF-16/CRLF 与真实写回同口径）；读不出按空（新建）
            let old = std::fs::read(&full)
                .ok()
                .and_then(|bytes| crate::text::decode(&bytes).ok())
                .map(|doc| doc.text)
                .unwrap_or_else(|| std::fs::read_to_string(&full).unwrap_or_default());
            // after 防御性归一为 LF（与 Write 执行的 diff 口径一致）
            let new = content.replace("\r\n", "\n");
            diff_preview(path, &old, &new)
        }
        "Edit" => {
            let path = args["path"].as_str().unwrap_or("?");
            let old_string = args["old_string"].as_str().unwrap_or("");
            let new_string = args["new_string"].as_str().unwrap_or("");
            let replace_all = args["replace_all"].as_bool().unwrap_or(false);
            let full =
                crate::tool::resolve_checked(cwd, path, false).unwrap_or_else(|_| cwd.join(path));
            let decoded = std::fs::read(&full)
                .ok()
                .and_then(|bytes| crate::text::decode(&bytes).ok());
            match decoded {
                Some(doc) => {
                    match tool::compute_edit(&doc.text, old_string, new_string, replace_all) {
                        Ok(outcome) => {
                            let mut detail = diff_preview(path, &doc.text, &outcome.after);
                            if let Some(note) = outcome.tier_note {
                                detail.push_str(&format!("\n\n（{note}）"));
                            }
                            if replace_all {
                                detail.push_str(&format!(
                                    "\n\n（replace_all：替换 {} 处）",
                                    outcome.replaced
                                ));
                            }
                            detail
                        }
                        // 匹配不上：退化为 naive 预览（旧口径）
                        Err(_) => diff_preview(
                            path,
                            &doc.text,
                            &doc.text.replacen(old_string, new_string, 1),
                        ),
                    }
                }
                // 解码失败（二进制/未知编码）：旧逻辑兜底
                None => {
                    let current = std::fs::read_to_string(&full).unwrap_or_default();
                    let new = current.replacen(old_string, new_string, 1);
                    diff_preview(path, &current, &new)
                }
            }
        }
        _ => serde_json::to_string_pretty(&args).unwrap_or_default(),
    }
}
fn diff_preview(path: &str, old: &str, new: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    format!("{path}\n\n{unified}")
}
