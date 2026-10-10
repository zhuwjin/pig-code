use super::*;

impl Session {
    /// Shared gated execution (reused by the parent session's general path and the subagent loop):
    /// a thin wrapper — assembles GateCtx from field borrows and forwards to the free function
    /// exec_tool_gated_ctx (foreground and background go through the same gate).
    pub(crate) async fn exec_tool_gated(
        &mut self,
        call: &ToolCall,
        tool: Option<&dyn tool::Tool>,
        item_id: &str,
        turn_id: &str,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> GatedToolOutcome {
        // The MCP tool list is held by the session (lazily connected on run_step's first step); subagent gating does not go through this wrapper.
        // Skills go through the extra channel (same as MCP: not in the all() static table, looked up by name as a fallback)
        let mut extra = self.mcp.as_ref().map(|mcp| mcp.tools()).unwrap_or_default();
        extra.push(Box::new(tool::SkillTool::new(&self.cwd, &self.data_dir)));
        let mut gate = GateCtx {
            cwd: &self.cwd,
            mode: self.mode,
            tracker: &mut self.tracker,
            state: &self.state,
            pending: &self.pending,
            permissions: &self.permissions,
            always_allowed: &mut self.always_allowed,
            session_id: &self.id,
            plan_enabled: self.plan_enabled,
            seq: &self.seq,
            store: &self.store,
            extra_tools: &extra,
        };
        exec_tool_gated_ctx(&mut gate, call, tool, item_id, turn_id, tx, cancel).await
    }
}

/// Approval dialog detail (pub so integration tests can assert it directly).
/// Bash detail = the bare command; the dangerous-command warning is carried by
/// ApprovalRequested.danger_key (the GUI localizes the title line by key), no longer embedded in detail.
/// Write/Edit go through the text pipeline: resolve_checked resolves the path (falls back to join on
/// failure), bytes -> pig_utils text::decode -> LF view for the diff (preview matches the real write-back); Edit
/// goes through compute_edit (replace_all-aware; a tolerant-tier hit is noted). Decoding failure falls
/// back to the old direct-read + replacen logic.
pub fn approval_detail(call: &ToolCall, cwd: &std::path::Path) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    match call.name.as_str() {
        "Bash" => args["command"].as_str().unwrap_or("?").to_string(),
        "Write" => {
            let path = args["path"].as_str().unwrap_or("?");
            let content = args["content"].as_str().unwrap_or("");
            let full =
                crate::tool::resolve_checked(cwd, path, false).unwrap_or_else(|_| cwd.join(path));
            // "before" uses the LF view (GBK/UTF-16/CRLF same policy as the real write-back); unreadable counts as empty (new file)
            let old = std::fs::read(&full)
                .ok()
                .and_then(|bytes| pig_utils::text::decode(&bytes).ok())
                .map(|doc| doc.text)
                .unwrap_or_else(|| std::fs::read_to_string(&full).unwrap_or_default());
            // "after" is defensively normalized to LF (same diff policy as Write's execution)
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
                .and_then(|bytes| pig_utils::text::decode(&bytes).ok());
            match decoded {
                Some(doc) => {
                    match tool::compute_edit(&doc.text, old_string, new_string, replace_all) {
                        Ok(outcome) => {
                            let mut detail = diff_preview(path, &doc.text, &outcome.after);
                            // Tolerant-tier hit annotation (the note itself is shared on the model side, fixed English)
                            if let Some(note) = outcome.tier_note {
                                detail.push_str(&format!("\n\n({note})"));
                            }
                            if replace_all {
                                detail.push_str(&format!(
                                    "\n\n(replace_all: replaced {} occurrences)",
                                    outcome.replaced
                                ));
                            }
                            detail
                        }
                        // No match: degrade to the naive preview (old policy)
                        Err(_) => diff_preview(
                            path,
                            &doc.text,
                            &doc.text.replacen(old_string, new_string, 1),
                        ),
                    }
                }
                // Decoding failure (binary/unknown encoding): old logic as fallback
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
