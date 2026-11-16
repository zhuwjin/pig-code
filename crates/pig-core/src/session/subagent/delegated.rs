use super::*;

impl Session {
    /// Background subagent: its terminal state uses the same task outbox as a
    /// background shell, including cancellation and driver failure.
    /// Plugin delegation: start a background child from a profile the plugin
    /// named, using the same preparation/drive pipeline as the Agent tool. The
    /// parent session must be idle; the child inherits its workspace,
    /// permissions, trust and tool narrowing.
    pub(crate) fn start_delegated_agent(
        &mut self,
        profile_name: &str,
        prompt_text: &str,
        description: &str,
        parent_config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
    ) -> Result<crate::session::DelegatedAgentStarted, String> {
        if self.plan_enabled {
            return Err(
                "Subagents cannot be delegated in plan mode (a subagent may modify files).".into(),
            );
        }
        self.check_delegation_entry()?;
        let description = description.trim();
        let prompt_text = prompt_text.trim();
        if description.is_empty() || prompt_text.is_empty() {
            return Err("a delegated agent needs a description and a prompt".into());
        }
        let runtime = self.runtime_config_cloned();
        let session_id = self.event_session_id().to_string();
        let trust = runtime
            .as_ref()
            .map(|runtime| runtime.trust().clone())
            .unwrap_or_else(|| crate::config::WorkspaceTrust::untrusted(&self.agent.cwd));
        let profiles = crate::agent::load_profiles_with_plugins(
            &self.agent.cwd,
            &self.data_dir,
            &trust,
            &self.plugin_tools.registry(),
        );
        let profile = crate::agent::find_profile(&profiles, profile_name)?.clone();
        let ctx = crate::agent::ChildPrepCtx {
            cwd: &self.agent.cwd,
            data_dir: &self.data_dir,
            git_snapshot: self.git_snapshot.as_deref(),
            skills_prompt: &self.skills_prompt,
            agents_prompt: &self.agents_prompt,
            runtime: runtime.as_deref(),
            parent_config,
            session_id: &session_id,
            parent_agent_id: &self.agent.id,
            parent_delegation_depth: self.agent.delegation_depth,
            max_delegation_depth: self.max_delegation_depth(),
            parent_permissions: self.permissions,
            parent_history: &self.agent.history,
            profiles_summary: &self.profiles_summary,
            state: &self.state,
            mcp: self.mcp.as_ref(),
            plugin_tools: self.plugin_tools.clone(),
        };
        let child_config = crate::agent::resolve_child_config(&ctx, &profile)?;
        let prep = crate::agent::prepare_fresh_child(
            &ctx,
            profile,
            child_config,
            prompt_text,
            description,
            &mut self.agent_seq,
        )
        .map_err(|error| error.message())?;
        let agent_id = prep.agent_id.clone();
        let profile_name = prep.policy.profile.name.clone();
        let bg_cancel = CancellationToken::new();
        let command = format!("Subagent {profile_name}: {description}");
        let task_id = crate::task::register_agent_run_queued(
            &self.state,
            command,
            bg_cancel.clone(),
            prep.run_metadata(),
            true,
        );
        let progress = ProgressSink::Background {
            task_id: task_id.clone(),
        };
        let child = Session::new_subagent(prep, self, progress, SubagentMode::Background);
        let model = agent_card_model(&child);
        let item_id = format!("plugin-agent-{agent_id}");
        // The synthetic Agent row and its card use the existing tool-card UI;
        // the task notification path covers completion.
        self.emit(
            |session_id| Event::ToolCallBegin {
                session_id,
                item_id: item_id.clone(),
                tool: "Agent".into(),
                input_summary: description.to_owned(),
                detail: format!("profile: {profile_name}"),
            },
            tx,
        );
        self.emit(
            |session_id| Event::SubagentCard {
                session_id,
                item_id: item_id.clone(),
                agent_id: agent_id.clone(),
                run_id: child.agent.run_id(),
                profile: profile_name.clone(),
                description: description.to_owned(),
                model: model.clone(),
                background: true,
            },
            tx,
        );
        spawn_detached_child(
            child,
            task_id.clone(),
            bg_cancel,
            tx.clone(),
            Some(DelegatedUi { item_id }),
        );
        Ok(crate::session::DelegatedAgentStarted {
            agent_id,
            task_id,
            profile: profile_name,
            model,
            background: true,
        })
    }
}
