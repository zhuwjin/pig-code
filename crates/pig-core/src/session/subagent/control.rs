use super::*;

/// The immediate answer of a plugin-delegated child.
pub(crate) struct DelegatedAgentStarted {
    pub agent_id: String,
    pub task_id: String,
    pub profile: String,
    pub model: String,
    pub background: bool,
}

impl Session {
    pub(super) fn max_delegation_depth(&self) -> u32 {
        self.runtime
            .as_ref()
            .map_or(1, |runtime| runtime.config().agents.max_delegation_depth)
    }

    pub(super) fn check_delegation_entry(&self) -> Result<(), String> {
        let depth = self.agent.delegation_depth;
        let max = self.max_delegation_depth();
        if depth < max {
            return Ok(());
        }
        if max == 0 {
            return Err("Subagents are disabled by [agents].max_delegation_depth=0.".to_string());
        }
        Err(format!(
            "Delegation depth limit reached: this agent is at depth {depth}, and [agents].max_delegation_depth={max}. No further Agent or AgentSwarm call is allowed."
        ))
    }

    /// Agent tool entry: argument gating → the shared child preparation
    /// builder (profile/model/tool/MCP/media/JSONL, identical to every swarm
    /// item) → a child `Session` driven by the shared engine under the managed
    /// lifecycle (global slot, exclusive context writer, panic/cancel
    /// settlement). Foreground, background, resume, swarm and plugin
    /// delegation all take this path.
    pub(crate) fn run_subagent<'a>(
        &'a mut self,
        call: &'a ToolCall,
        // Child-tool approval request_ids are prefixed with the foreground
        // card's item_id / the background task_id (which embed turn info);
        // unused on its own since this sank into GateCtx
        _parent_turn_id: &'a str,
        parent_item_id: &'a str,
        parent_config: &'a ResolvedModel,
        tx: &'a async_channel::Sender<Event>,
        cancel: &'a CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SubagentOutcome> + Send + 'a>> {
        // Boxed at the public boundary: a root tool batch can start a child,
        // whose own loop re-enters the same engine. Erasing this future keeps
        // that cycle out of the type checker.
        Box::pin(self.run_subagent_inner(
            call,
            _parent_turn_id,
            parent_item_id,
            parent_config,
            tx,
            cancel,
        ))
    }

    async fn run_subagent_inner(
        &mut self,
        call: &ToolCall,
        _parent_turn_id: &str,
        parent_item_id: &str,
        parent_config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> SubagentOutcome {
        // ---- argument parsing and mutual-exclusion gating ----
        let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
        let fail = |note: String| SubagentOutcome::Finished {
            note,
            is_error: true,
            // Early exits from argument/profile/model resolution failure have
            // no agent_id; no agent card is created
            card: None,
            cards: vec![],
        };
        let description = args["description"].as_str().unwrap_or("").trim();
        if description.is_empty() {
            return fail(
                "Agent is missing the `description` argument (a 3-5 word task summary)".to_string(),
            );
        }
        let prompt_text = args["prompt"].as_str().unwrap_or("").trim();
        if prompt_text.is_empty() {
            return fail(
                "Agent is missing the `prompt` argument (a complete, self-contained task brief)"
                    .to_string(),
            );
        }
        let background = args["run_in_background"].as_bool().unwrap_or(false);
        let fork = args["fork"].as_bool().unwrap_or(false);
        let resume = args["resume"].as_str().unwrap_or("").trim();
        let subagent_type = args["subagent_type"].as_str().unwrap_or("").trim();
        if fork && !resume.is_empty() {
            return fail(
                "`fork` and `resume` are mutually exclusive: fork creates a new child from the parent's snapshot; resume continues an existing child"
                    .to_string(),
            );
        }
        if !resume.is_empty() && !subagent_type.is_empty() {
            return fail(
                "`resume` and `subagent_type` are mutually exclusive: do not specify a type when resuming an existing subagent"
                    .to_string(),
            );
        }
        // Plan-mode hard rejection: subagents may modify files; read-only
        // semantics must not be bypassed
        if self.plan_enabled {
            return fail(
                "Subagents cannot be delegated in plan mode (a subagent may modify files). Research read-only and write the plan yourself first, or exit plan mode before delegating."
                    .to_string(),
            );
        }
        if let Err(note) = self.check_delegation_entry() {
            return fail(note);
        }

        // ---- one shared child preparation pipeline for single Agent and
        // swarm: trust, profile/model resolution, tool narrowing, MCP
        // inheritance, media projection, writer claim and JSONL persistence ----
        let runtime = self.runtime_config_cloned();
        let session_id = self.event_session_id().to_string();
        // Every child kind derives its project surface from the same resolved
        // runtime trust decision (the session token is kept in sync with it).
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
        let prep = if resume.is_empty() {
            let query = if subagent_type.is_empty() {
                "general-purpose"
            } else {
                subagent_type
            };
            let profile = match crate::agent::find_profile(&profiles, query) {
                Ok(profile) => profile.clone(),
                Err(error) => return fail(error),
            };
            let child_config = match crate::agent::resolve_child_config(&ctx, &profile) {
                Ok(config) => config,
                Err(error) => return fail(error),
            };
            let prepared = if fork {
                crate::agent::prepare_fork_child(
                    &ctx,
                    profile,
                    child_config,
                    prompt_text,
                    description,
                    &mut self.agent_seq,
                )
            } else {
                crate::agent::prepare_fresh_child(
                    &ctx,
                    profile,
                    child_config,
                    prompt_text,
                    description,
                    &mut self.agent_seq,
                )
            };
            match prepared {
                Ok(prep) => prep,
                Err(error) => return fail(error.message()),
            }
        } else {
            match crate::agent::prepare_resume_child(
                &ctx,
                &profiles,
                resume,
                prompt_text,
                description,
            ) {
                Ok(prep) => prep,
                Err(error) => return fail(error.message()),
            }
        };
        // ---- background: register a task first (its id is the child's
        // progress/ownership prefix), then spawn the detached child ----
        if background {
            let agent_id = prep.agent_id.clone();
            let profile = prep.policy.profile.name.clone();
            let description = prep.description.clone();
            let bg_cancel = CancellationToken::new();
            let command = format!("Subagent {profile}: {description}");
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
            let card_record = self.emit_child_card(&child, parent_item_id, tx, true);
            let queue_hint = if crate::task::subagent_slots_available() == 0 {
                format!(
                    "\nThe global subagent concurrency limit ({}) is reached; this task is queued \
                     for a free slot and will notify as usual when done.",
                    crate::task::MAX_CONCURRENT_SUBAGENTS
                )
            } else {
                String::new()
            };
            let running_note = format!(
                "agent_id: {agent_id}\ntask_id: {task_id}\nstatus: running\nThe subagent is running in the background; its terminal notification is delivered automatically.\nUse TaskWait to wait, TaskOutput for progress, TaskStop to stop it, and Agent(resume=\"{agent_id}\", prompt=\"...\") to resume it.{queue_hint}"
            );
            spawn_detached_child(child, task_id, bg_cancel, tx.clone(), None);
            return SubagentOutcome::Finished {
                note: running_note,
                is_error: false,
                card: Some(card_record),
                cards: vec![],
            };
        }

        // ---- foreground: the same managed driver as every other child
        // (global slot, panic/cancel settlement). The child tracker shares the
        // session originals and this turn's summary, so its writes contribute
        // to the parent's TurnChanges exactly once. ----
        let child_cancel = cancel.child_token();
        let task_id = crate::task::register_agent_run_queued(
            &self.state,
            format!(
                "Subagent {}: {}",
                prep.policy.profile.name, prep.description
            ),
            child_cancel.clone(),
            prep.run_metadata(),
            false,
        );
        let progress = ProgressSink::Foreground {
            parent_item_id: parent_item_id.to_string(),
        };
        let mut child = Session::new_subagent(prep, self, progress, SubagentMode::Foreground);
        let card_record = self.emit_child_card(&child, parent_item_id, tx, false);
        let agent_id = child.agent.id.clone();
        let profile = child
            .subagent
            .as_ref()
            .expect("child session")
            .profile
            .name
            .clone();
        let life_writer = child
            .subagent
            .as_mut()
            .expect("child session")
            .take_writer()
            .expect("foreground subagent writer was claimed");
        let life = ChildLifecycle::new(
            &child.state,
            &child.pending,
            child.event_session_id(),
            tx.clone(),
            &child,
            life_writer,
            Some(task_id),
        );
        let run_cancel = child_cancel.clone();
        let child_task: std::pin::Pin<
            Box<dyn std::future::Future<Output = SubagentResult> + Send + '_>,
        > = Box::pin(async move { child.run_subagent_task(tx, &run_cancel).await });
        let managed = drive_managed(
            life,
            &child_cancel,
            async {
                crate::task::acquire_subagent_slot(&child_cancel)
                    .await
                    .map(SubagentSlot::Global)
            },
            child_task,
        )
        .await;
        let result = managed.result;
        if result.is_cancelled() {
            return SubagentOutcome::Cancelled {
                card: Some(card_record),
                cards: vec![],
            };
        }
        // Subagent cost goes into the parent turn stats (no StepUsage
        // recorded / watermark untouched)
        self.turn_input += result.usage.0;
        self.turn_cache_read += result.usage.1;
        self.turn_output += result.usage.2;
        let note = subagent_receipt(&agent_id, &profile, &result);
        SubagentOutcome::Finished {
            note,
            is_error: result.is_error(),
            card: Some(card_record),
            cards: vec![],
        }
    }

    /// Emit one Agent card for a child and return its durable record.
    pub(super) fn emit_child_card(
        &mut self,
        child: &Session,
        parent_item_id: &str,
        tx: &async_channel::Sender<Event>,
        background: bool,
    ) -> crate::rollout::AgentCardRecord {
        let sub = child.subagent.as_ref().expect("child session");
        let card = crate::rollout::AgentCardRecord {
            agent_id: child.agent.id.clone(),
            run_id: child.agent.run_id().expect("child run identity"),
            profile: sub.profile.name.clone(),
            description: sub.description.clone(),
            model: agent_card_model(child),
            background,
        };
        self.emit(
            |session_id| Event::SubagentCard {
                session_id,
                item_id: parent_item_id.to_string(),
                agent_id: card.agent_id.clone(),
                run_id: Some(card.run_id.clone()),
                profile: card.profile.clone(),
                description: card.description.clone(),
                model: card.model.clone(),
                background: card.background,
            },
            tx,
        );
        card
    }
}
