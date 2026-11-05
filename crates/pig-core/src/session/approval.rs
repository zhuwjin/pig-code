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
        // MCP and plugin tool lists are held by the session; subagent gating does not go through this wrapper.
        // Skills go through the extra channel (not in the all() static table, looked up by name as a fallback)
        let mut extra = self.mcp.as_ref().map(|mcp| mcp.tools()).unwrap_or_default();
        let trusted = self.trust.admits(&self.cwd);
        extra.push(Box::new(tool::SkillTool::new_trusted(
            &self.cwd,
            &self.plugin_tools.registry(),
            trusted,
        )));
        // Root-only stateless tools reach the generic executor through the
        // extra channel (all_root is schema-side only)
        extra.push(Box::new(tool::SendMessageTool));
        extra.extend(self.plugin_tools.tools_filtered(&self.plugin_tool_filter));
        self.plugin_tools.dispatcher().set_event_sender(tx.clone());
        let mut gate = GateCtx {
            cwd: &self.cwd,
            permissions: self.permissions,
            media: tool::MediaCapabilities::new(
                self.projected_input_image.unwrap_or(false),
                self.projected_input_pdf.unwrap_or(false),
            ),
            tracker: &mut self.tracker,
            state: &self.state,
            owns_todo_state: true,
            pending: &self.pending,
            permission_rules: &mut self.permission_rules,
            always_allowed: &mut self.always_allowed,
            session_id: &self.id,
            plan_enabled: self.plan_enabled,
            store: &self.store,
            extra_tools: &extra,
            hooks: &self.hooks,
            history: &self.history,
        };
        exec_tool_gated_ctx(&mut gate, call, tool, item_id, turn_id, tx, cancel).await
    }

    /// The one trust boundary for both a grant and a revoke: install the
    /// resolved runtime and atomically refresh every project-file surface it
    /// admits.
    ///
    /// - project hooks run (grant) or stay skipped with the banner (revoke);
    /// - project permission `allow` rules are loaded under the new decision
    ///   (denials were always loaded);
    /// - project skills and agent profiles enter or leave the prompt (the
    ///   frozen prompt is invalidated deliberately: a trust change is a
    ///   one-time prefix-cache break);
    /// - the project sandbox layer is installed from the same runtime;
    /// - lazy MCP reconnects on the next step with the new project servers.
    ///
    /// `None` means no configuration is loaded: the trust token and project
    /// overlay are cleared rather than left stale. Callers pass a runtime only
    /// after the trust decision has been saved and reloaded successfully, so a
    /// failed save/reload activates nothing.
    pub(crate) fn apply_trust(&mut self, runtime: Option<Arc<crate::config::RuntimeConfig>>) {
        let trust = runtime
            .as_ref()
            .map(|runtime| runtime.trust().clone())
            .unwrap_or_else(|| crate::config::WorkspaceTrust::untrusted(&self.cwd));
        let trusted = trust.admits(&self.cwd);
        let project_sandbox = runtime
            .as_ref()
            .map(|runtime| runtime.project_sandbox().clone())
            .unwrap_or_default();
        let config = runtime.as_deref().map(|runtime| runtime.config());
        self.permission_rules =
            super::initialization::load_permissions(&self.cwd, &self.data_dir, &trust);
        self.hooks = crate::hooks::HookSet::load(&self.cwd, config);
        self.plugin_tools.reload(&trust, config.cloned());
        self.trust = trust;
        self.skills_prompt = crate::plugins::skills_section_with_trust(
            &self.cwd,
            &self.plugin_tools.registry(),
            trusted,
        );
        self.profiles_summary = crate::agent::agent_description_list(
            &crate::agent::load_profiles_with_trust(&self.cwd, &self.data_dir, &self.trust),
        );
        // Force the next turn to rebuild the system prompt with the newly
        // admitted listing; the old prompt is no longer the truth.
        self.prompt_text = None;
        self.prompt_memory_enabled = None;
        self.prompt_skills_state = None;
        self.mcp = None;
        self.install_project_sandbox(project_sandbox);
        self.runtime = runtime;
    }

    /// Install a runtime without reloading project surfaces when the trust
    /// decision is unchanged (config saves, turn boundaries). A trust change
    /// — including `None`, which clears a stale token and project overlay —
    /// goes through the one [`Self::apply_trust`] boundary instead.
    pub(crate) fn set_runtime(&mut self, runtime: Option<Arc<crate::config::RuntimeConfig>>) {
        let trusted = runtime
            .as_ref()
            .is_some_and(|runtime| runtime.trust().admits(&self.cwd));
        if trusted != self.trust.admits(&self.cwd) {
            self.apply_trust(runtime);
            return;
        }
        let project_sandbox = runtime
            .as_ref()
            .map(|runtime| runtime.project_sandbox().clone())
            .unwrap_or_default();
        self.install_project_sandbox(project_sandbox);
        self.runtime = runtime;
    }

    /// Install (or clear) the workspace's own sandbox layer on the session
    /// state. The state is shared with every clone the tool path holds, so the
    /// next command picks the change up. A stored runtime already carries the
    /// parsed layer; only a session without one falls back to reading the
    /// workspace file under the live trust decision.
    pub(crate) fn refresh_project_sandbox(&self) {
        let layer = self
            .runtime
            .as_ref()
            .map(|runtime| runtime.project_sandbox().clone())
            .unwrap_or_else(|| {
                crate::config::project_sandbox_layer(&self.cwd, self.trust.admits(&self.cwd))
            });
        self.install_project_sandbox(layer);
    }

    fn install_project_sandbox(&self, layer: crate::config::ProjectSandboxLayer) {
        *self
            .state
            .sandbox_project
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = layer;
    }

    /// The single workspace-trust entry point for the session banner: every
    /// project-authored surface this decision would admit, not only hooks.
    /// Returns the canonical workspace and how many items await the decision.
    pub(crate) fn pending_project_trust(&self) -> Option<(String, usize)> {
        if self.trust.admits(&self.cwd) {
            return None;
        }
        let project_dir = self.cwd.join(".pigcode");
        let mut count = self.hooks.project_count;
        count += pig_utils::skills::discover_root(
            &pig_utils::skills::project_root(&self.cwd),
            pig_utils::skills::SkillSource::Project,
        )
        .len();
        count += crate::agent::load_profiles_with_trust(
            &self.cwd,
            &self.data_dir,
            &crate::config::WorkspaceTrust::new(&self.cwd, true),
        )
        .iter()
        .filter(|profile| profile.source == crate::agent::AgentSource::Project)
        .count();
        for file in ["mcp.json", "permissions.toml", "config.toml"] {
            if project_dir.join(file).is_file() {
                count += 1;
            }
        }
        (count > 0).then(|| (self.cwd.display().to_string(), count))
    }

    /// Hook run context bound to this session (turn-side hook chains)
    pub(crate) fn hook_run_ctx(
        &self,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> crate::hooks::HookRunCtx {
        crate::hooks::HookRunCtx {
            session_id: self.id.clone(),
            cwd: self.cwd.clone(),
            tx: tx.clone(),
            cancel: cancel.clone(),
        }
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
        name if crate::tool::is_exec_command(name) => {
            args["command"].as_str().unwrap_or("?").to_string()
        }
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
