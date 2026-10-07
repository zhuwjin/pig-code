use super::*;

impl Session {
    /// Agent tool entry: argument gating → profile/model/tool resolution →
    /// drive the subagent in the foreground (borrowing Session fields to build
    /// GateCtx) or the background (fully owned + tokio::spawn; completion wakes
    /// the parent via the wake channel). resume reuses the original agent_id:
    /// load the context + append the new prompt and continue (profile/model
    /// re-resolved from current state).
    pub(crate) async fn run_subagent(
        &mut self,
        call: &ToolCall,
        // Child-tool approval request_ids are prefixed with the foreground
        // card's item_id / the background task_id (which embed turn info);
        // unused on its own since this sank into GateCtx
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
        let resume = args["resume"].as_str().unwrap_or("").trim();
        let subagent_type = args["subagent_type"].as_str().unwrap_or("").trim();
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

        // ---- context preparation: fresh start builds history; resume loads
        // history + appends the new prompt ----
        let profiles = crate::agent::load_profiles(&self.cwd, &self.data_dir);
        let agents_dir = crate::agent::agents_dir(&self.data_dir.join("sessions"), &self.id);
        let (profile, agent_id, history) = if resume.is_empty() {
            let query = if subagent_type.is_empty() {
                "general-purpose"
            } else {
                subagent_type
            };
            let profile = match crate::agent::find_profile(&profiles, query) {
                Ok(profile) => profile.clone(),
                Err(error) => return fail(error),
            };
            let agent_id = format!("a{}-{}", crate::rollout::now_secs(), self.agent_seq + 1);
            self.agent_seq += 1;
            let history = vec![
                ChatMsg::system(crate::prompt::subagent_system_prompt(
                    &profile,
                    &self.cwd,
                    self.git_snapshot.as_deref(),
                    &self.agents_prompt,
                    &self.skills_prompt,
                )),
                ChatMsg::user(prompt_text.to_string()),
            ];
            (profile, agent_id, history)
        } else {
            let jsonl = agents_dir.join(format!("{resume}.jsonl"));
            if !jsonl.exists() {
                // List available agent_ids (*.jsonl without the extension) to
                // help the model correct the spelling
                let mut ids: Vec<String> = std::fs::read_dir(&agents_dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry.ok())
                    .filter_map(|entry| {
                        let path = entry.path();
                        if path.extension().is_some_and(|ext| ext == "jsonl") {
                            path.file_stem().map(|s| s.to_string_lossy().to_string())
                        } else {
                            None
                        }
                    })
                    .collect();
                ids.sort();
                let available = if ids.is_empty() {
                    "(none)".to_string()
                } else {
                    ids.join(", ")
                };
                return fail(format!(
                    "Subagent \"{resume}\" does not exist. Available: {available}"
                ));
            }
            // Running conflict: same agent_id in the registry with status
            // Running → parallel continuation is not allowed
            let running_task = self
                .state
                .tasks
                .lock()
                .expect("task registry lock")
                .iter()
                .find(|t| {
                    t.agent_id.as_deref() == Some(resume)
                        && matches!(t.status, pig_protocol::TaskStatus::Running)
                })
                .map(|t| t.id.clone());
            if let Some(task_id) = running_task {
                return fail(format!(
                    "This subagent is still running (task_id {task_id}); stop it with TaskStop before resuming"
                ));
            }
            let (meta, mut history) = match crate::agent::read_agent(&jsonl) {
                Ok(loaded) => loaded,
                Err(error) => return fail(error),
            };
            // Re-find the profile from current state (deleted → error); the
            // model is likewise re-resolved from the profile's current state
            // (meta.model ignored)
            let profile = match crate::agent::find_profile(&profiles, &meta.profile) {
                Ok(profile) => profile.clone(),
                Err(error) => return fail(error),
            };
            history.push(ChatMsg::user(prompt_text.to_string()));
            (profile, resume.to_string(), history)
        };

        // ---- model resolution (strict: failure is reported to the model) ----
        let child_config = match self.app_config.as_ref() {
            Some(app_config) => {
                match crate::agent::resolve_subagent_model(app_config, parent_config, &profile) {
                    Ok(config) => config,
                    Err(error) => return fail(error),
                }
            }
            // App config not loaded: inheriting the parent model is
            // unaffected; explicit models cannot be resolved
            None if profile.model.is_some() => {
                return fail(
                    "App config is not loaded; cannot resolve the subagent's configured model"
                        .to_string(),
                );
            }
            None => parent_config.clone(),
        };

        // ---- tool narrowing: the subagent loop uses all() (naturally without
        // Agent, preventing nesting) + MCP inheritance ----
        let all_tools = tool::all();
        let all_names: Vec<String> = all_tools.iter().map(|t| t.name().to_string()).collect();
        let keep = crate::agent::child_tool_set(&profile, &all_names, child_config.input_image);
        let mut child_tools: Vec<Box<dyn tool::Tool>> = all_tools
            .into_iter()
            .filter(|t| keep.iter().any(|name| name == t.name()))
            .collect();
        // MCP inheritance: full-tool profiles (including Write/Edit after
        // narrowing) inherit all connected MCP tools; read-only profiles (e.g.
        // explore) inherit only those with readOnlyHint. Schemas go into the
        // subagent's sampling as well
        let mcp_inherits_all = crate::agent::child_inherits_all_mcp(&keep);
        if let Some(mcp) = &self.mcp {
            child_tools.extend(mcp.child_tools(mcp_inherits_all));
        }
        // Skill is supplied to all subagents (read-only, same cross-profile
        // default as MCP inheritance): skill bodies load on demand, and the
        // subagent system prompt injects the skill listing too
        child_tools.push(Box::new(tool::SkillTool::new(&self.cwd, &self.data_dir)));
        let child_schemas: Vec<serde_json::Value> =
            child_tools.iter().map(|t| t.schema()).collect();

        // ---- context persistence: fresh start writes meta + initial
        // messages; resume appends only the new user line ----
        let jsonl = agents_dir.join(format!("{agent_id}.jsonl"));
        if resume.is_empty() {
            persist_agent_line(
                &jsonl,
                &serde_json::json!({
                    "type": "meta",
                    "agent_id": agent_id,
                    "profile": profile.name,
                    "description": description,
                    "model": child_config.model,
                    "provider": child_config.provider_name,
                    "created_at": crate::rollout::now_secs(),
                }),
            );
            for msg in &history {
                persist_agent_msg(&jsonl, msg);
            }
        } else {
            persist_agent_msg(&jsonl, history.last().expect("resume user pushed"));
        }

        let max_turns = profile.max_turns.unwrap_or(crate::agent::DEFAULT_MAX_TURNS);
        let mut drive = SubagentDrive {
            agent_id,
            cwd: self.cwd.clone(),
            data_dir: self.data_dir.clone(),
            profile,
            child_config,
            tools: child_tools,
            schemas: child_schemas,
            history,
            jsonl,
            max_turns,
            description: description.to_string(),
            mcp: self.mcp.clone(),
            mcp_inherits_all,
        };

        // Agent card metadata: assembled once agent_id is allocated and
        // child_config resolved — sent live via the SubagentCard event and
        // persisted with RolloutRecord::ToolCall (replay rebuilds from the
        // record); foreground/background/resume share one path (profile/model
        // per this run's re-resolution)
        let card_record = crate::rollout::AgentCardRecord {
            agent_id: drive.agent_id.clone(),
            profile: drive.profile.name.clone(),
            description: drive.description.clone(),
            model: agent_card_model(&drive),
            background,
        };
        self.emit(
            |session_id, seq| Event::SubagentCard {
                session_id,
                seq,
                item_id: parent_item_id.to_string(),
                agent_id: card_record.agent_id.clone(),
                profile: card_record.profile.clone(),
                description: card_record.description.clone(),
                model: card_record.model.clone(),
                background: card_record.background,
            },
            tx,
        );

        // ---- background: register a task + spawn the drive, return running
        // immediately ----
        if background {
            return self.spawn_subagent_background(drive, tx, card_record);
        }

        // ---- foreground: borrow Session fields to build GateCtx and drive
        // synchronously ----
        let result = {
            // MCP tools inherited by the subagent (lookup fallback by name in
            // the gated execution segment; rules narrowed by profile)
            let mcp_extra = drive.extra_tools();
            let mut gate = GateCtx {
                cwd: &self.cwd,
                mode: self.mode,
                tracker: &mut self.tracker,
                state: &self.state,
                pending: &self.pending,
                permissions: &self.permissions,
                always_allowed: &mut self.always_allowed,
                session_id: &self.id,
                plan_enabled: false,
                seq: &self.seq,
                store: &self.store,
                extra_tools: &mcp_extra,
            };
            drive_subagent(
                &mut gate,
                &mut drive,
                &ProgressSink::Foreground {
                    parent_item_id: parent_item_id.to_string(),
                },
                tx,
                cancel,
            )
            .await
        };
        // Live panel wrap-up (including parent cancellation): clears the
        // "running" indicator in the "subagents" tab on the right
        self.emit(
            |session_id, seq| Event::SubagentActivity {
                session_id,
                seq,
                agent_id: drive.agent_id.clone(),
                item: None,
                finished: true,
            },
            tx,
        );
        if result.cancelled {
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
        // Wrap-up template: normal completion uses the template; turn
        // exhaustion / request failure uses the text brought out by the drive
        // as-is
        let note = if result.completed {
            format!(
                "agent_id: {}\nsubagent_type: {}\nstatus: completed\nturns: {}\n[summary]\n{}\nresume_hint: continue this subagent with Agent(resume=\"{}\", prompt=\"...\")",
                drive.agent_id,
                drive.profile.name,
                result.turns,
                result.result_text,
                drive.agent_id
            )
        } else {
            result.result_text
        };
        SubagentOutcome::Finished {
            note,
            is_error: result.is_error,
            card: Some(card_record),
            cards: vec![],
        }
    }
    /// AgentSwarm tool entry: parse_swarm_args validation →
    /// prepare_swarm_children batch preparation (same pipeline as
    /// run_subagent) → drive all concurrently (global concurrency slot cap;
    /// queue when exceeded). Default foreground: blocks until all finish, with
    /// the aggregated result as a single tool result; turn cancel → cancel all
    /// subagents. run_in_background=true: register background tasks one by one
    /// and spawn independent drives (same lifecycle as background Agent:
    /// stoppable via TaskStop, completions wake the parent one by one via the
    /// wake channel), returning per-item receipts immediately.
    pub(crate) async fn run_swarm(
        &mut self,
        call: &ToolCall,
        parent_item_id: &str,
        parent_config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> SubagentOutcome {
        let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
        let fail = |note: String| SubagentOutcome::Finished {
            note,
            is_error: true,
            // Early exits from argument/profile/model resolution failure have
            // no agent_id; no agent card is created
            card: None,
            cards: vec![],
        };
        let plan = match tool::parse_swarm_args(&args) {
            Ok(plan) => plan,
            Err(error) => return fail(error),
        };
        // Plan-mode hard rejection (same policy as run_subagent): subagents may
        // modify files — foreground/background rejected alike
        if self.plan_enabled {
            return fail(
                "Subagents cannot be delegated in plan mode (a subagent may modify files). Research read-only and write the plan yourself first, or exit plan mode before delegating."
                    .to_string(),
            );
        }
        let background = args["run_in_background"].as_bool().unwrap_or(false);
        let preps = match crate::agent::prepare_swarm_children(
            &crate::agent::SwarmPrepCtx {
                cwd: &self.cwd,
                data_dir: &self.data_dir,
                git_snapshot: self.git_snapshot.as_deref(),
                skills_prompt: &self.skills_prompt,
                agents_prompt: &self.agents_prompt,
                app_config: self.app_config.as_ref(),
                parent_config,
                session_id: &self.id,
                tasks: &self.state.tasks,
                mcp: self.mcp.as_ref(),
            },
            &plan,
            &mut self.agent_seq,
        ) {
            Ok(preps) => preps,
            Err(error) => return fail(error),
        };

        // ---- background: dispatch independent background drives one by one
        // (no join), return per-item receipts immediately ----
        if background {
            return self.dispatch_swarm_background(preps, parent_item_id, tx);
        }

        // ---- foreground: drive all concurrently, block until all finish ----
        // Each subagent's card metadata is collected one by one (sent live via
        // the SubagentCard event; persisted as agent_cards with the rollout
        // ToolCall record, replay rebuilds all subagent cards)
        let mut cards: Vec<crate::rollout::AgentCardRecord> = Vec::new();
        // slots preserve plan order (prep-failed entries land Failed in place;
        // spawn results are backfilled by ix)
        let mut slots: Vec<Option<crate::agent::SwarmChildResult>> =
            (0..preps.len()).map(|_| None).collect();
        let mut handles = Vec::new();
        let mut tokens = Vec::new();
        for (ix, prep) in preps.into_iter().enumerate() {
            let prep = match prep {
                crate::agent::SwarmPrep::Ready(prep) => prep,
                // Preparation-phase failure: not started, recorded as a failed
                // item in the aggregation (partial failure does not affect
                // others)
                crate::agent::SwarmPrep::Failed { description, error } => {
                    slots[ix] = Some(crate::agent::SwarmChildResult {
                        description,
                        agent_id: None,
                        status: crate::agent::SwarmChildStatus::Failed,
                        turns: 0,
                        result_path: None,
                        result_text: error,
                        queued: false,
                        usage: (0, 0, 0),
                    });
                    continue;
                }
            };
            let drive = SubagentDrive::from(*prep);
            // Agent card (same policy as run_subagent; batch foreground has
            // background=false)
            let card = crate::rollout::AgentCardRecord {
                agent_id: drive.agent_id.clone(),
                profile: drive.profile.name.clone(),
                description: drive.description.clone(),
                model: agent_card_model(&drive),
                background: false,
            };
            self.emit(
                |session_id, seq| Event::SubagentCard {
                    session_id,
                    seq,
                    item_id: parent_item_id.to_string(),
                    agent_id: card.agent_id.clone(),
                    profile: card.profile.clone(),
                    description: card.description.clone(),
                    model: card.model.clone(),
                    background: card.background,
                },
                tx,
            );
            cards.push(card);
            // Registered even while queued (command carries the "Queued · "
            // prefix); visible and stoppable in TaskList/panel
            let child_cancel = CancellationToken::new();
            let command = format!("Subagent {}: {}", drive.profile.name, drive.description);
            let task_id = crate::task::register_agent_task_queued(
                &self.state,
                command,
                child_cancel.clone(),
                drive.agent_id.clone(),
            );
            // Fully owned context (same list as spawn_subagent_background)
            let cwd = self.cwd.clone();
            let mode = self.mode;
            let permissions = self.permissions.clone();
            let mut always_allowed = self.always_allowed.clone();
            let state = self.state.clone();
            let pending = self.pending.clone();
            let store = self.store.clone();
            let seq = self.seq.clone();
            let session_id = self.id.clone();
            let tx_bg = tx.clone();
            let result_path = agent_result_path(&drive.jsonl, &drive.agent_id);
            let description = drive.description.clone();
            let agent_id = drive.agent_id.clone();
            let task_id_bg = task_id.clone();
            tokens.push(child_cancel.clone());
            handles.push(tokio::spawn(async move {
                // Global concurrency slot: queue when exceeded (interruptible
                // by TaskStop/parent cancel while queued)
                let queued = crate::task::subagent_slots_available() == 0;
                if queued {
                    crate::task::note_output(
                        &state.tasks,
                        &task_id_bg,
                        "Queued: the global subagent concurrency limit is reached; waiting for a free slot…\n",
                    );
                }
                let Some(_permit) = crate::task::acquire_subagent_slot(&child_cancel).await else {
                    // Cancelled while queued: stop_task already set Killed; no
                    // registry wrap-up needed
                    return (
                        ix,
                        crate::agent::SwarmChildResult {
                            description,
                            agent_id: Some(agent_id),
                            status: crate::agent::SwarmChildStatus::Cancelled,
                            turns: 0,
                            result_path: None,
                            result_text: String::new(),
                            queued,
                            usage: (0, 0, 0),
                        },
                    );
                };
                crate::task::mark_agent_task_started(&state, &task_id_bg);
                let mut tracker = ChangeTracker::default();
                let result = {
                    // MCP tools inherited by the subagent (owned: fetched
                    // inside the closure; the rule snapshot lives on drive)
                    let mcp_extra = drive.extra_tools();
                    let mut gate = GateCtx {
                        cwd: &cwd,
                        mode,
                        tracker: &mut tracker,
                        state: &state,
                        pending: &pending,
                        permissions: &permissions,
                        always_allowed: &mut always_allowed,
                        session_id: &session_id,
                        plan_enabled: false,
                        seq: &seq,
                        store: &store,
                        extra_tools: &mcp_extra,
                    };
                    let mut drive = drive;
                    drive_subagent(
                        &mut gate,
                        &mut drive,
                        &ProgressSink::Background {
                            task_id: task_id_bg.clone(),
                        },
                        &tx_bg,
                        &child_cancel,
                    )
                    .await
                };
                // Live panel wrap-up (including parent cancel/TaskStop)
                emit_bg(&session_id, &seq, &tx_bg, |sid, seq| {
                    Event::SubagentActivity {
                        session_id: sid,
                        seq,
                        agent_id: agent_id.clone(),
                        item: None,
                        finished: true,
                    }
                });
                // Registry wrap-up: entries already set Killed by TaskStop are
                // not overwritten; parent cancel records Killed
                {
                    let mut tasks = state.tasks.lock().expect("task registry lock");
                    if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id_bg)
                        && matches!(entry.status, pig_protocol::TaskStatus::Running)
                    {
                        entry.status = if result.cancelled {
                            pig_protocol::TaskStatus::Killed
                        } else if result.is_error {
                            pig_protocol::TaskStatus::Exited(-1)
                        } else {
                            pig_protocol::TaskStatus::Exited(0)
                        };
                        entry.ended_at = Some(crate::rollout::now_secs());
                    }
                }
                crate::task::note_output(
                    &state.tasks,
                    &task_id_bg,
                    &format!("[{}]\n{}\n", result.status_line, result.result_text),
                );
                let _ = state.task_notify.send(session_id.clone());
                (
                    ix,
                    crate::agent::SwarmChildResult {
                        description,
                        agent_id: Some(agent_id),
                        status: if result.cancelled {
                            crate::agent::SwarmChildStatus::Cancelled
                        } else if result.is_error {
                            crate::agent::SwarmChildStatus::Failed
                        } else {
                            crate::agent::SwarmChildStatus::Completed
                        },
                        turns: result.turns,
                        result_path: if result.cancelled {
                            None
                        } else {
                            std::fs::metadata(&result_path).ok().map(|_| result_path)
                        },
                        result_text: result.result_text,
                        queued,
                        usage: result.usage,
                    },
                )
            }));
        }

        // All concurrent; parent cancel → cancel all subagents and wait for
        // the wrap-up (Cancelled semantics match the foreground Agent: partial
        // results are excluded from the aggregation, the tool card settles as
        // "Stopped")
        let joined = tokio::select! {
            joined = futures_util::future::join_all(&mut handles) => Some(joined),
            _ = cancel.cancelled() => {
                for token in &tokens {
                    token.cancel();
                }
                let _ = futures_util::future::join_all(&mut handles).await;
                None
            }
        };
        let Some(joined) = joined else {
            return SubagentOutcome::Cancelled { card: None, cards };
        };
        for item in joined {
            match item {
                Ok((ix, child)) => slots[ix] = Some(child),
                Err(error) => eprintln!("[agent] swarm subagent task aborted: {error}"),
            }
        }
        let children: Vec<crate::agent::SwarmChildResult> = slots.into_iter().flatten().collect();
        // Subagent cost goes into the parent turn stats (foreground policy; no
        // StepUsage recorded / watermark untouched)
        for child in &children {
            self.turn_input += child.usage.0;
            self.turn_cache_read += child.usage.1;
            self.turn_output += child.usage.2;
        }
        let is_error = children
            .iter()
            .any(|c| c.status != crate::agent::SwarmChildStatus::Completed);
        SubagentOutcome::Finished {
            note: crate::agent::format_swarm_result(&children),
            is_error,
            // Batch agent cards: one per subagent (already sent live one by
            // one via the SubagentCard event), persisted as agent_cards with
            // the rollout ToolCall record; replay rebuilds all subagent cards
            card: None,
            cards,
        }
    }

    /// Background swarm branch: every Ready subagent gets a SubagentCard
    /// (background=true) + a registered "queued" task + a spawned independent
    /// background drive (drive_subagent_detached, shared with the background
    /// Agent: concurrency-slot queueing/registry wrap-up/result persistence/
    /// completions wake the parent one by one via the wake channel); no join,
    /// per-item receipts returned immediately (prep-phase failures listed
    /// too). usage does not go into the parent turn stats (same policy as the
    /// background Agent: discarded inside the drive); the parent turn
    /// continuing or ending does not affect the subagents, and cancelling the
    /// parent turn does not cancel them (they only honor TaskStop in their own
    /// registry entries).
    fn dispatch_swarm_background(
        &mut self,
        preps: Vec<crate::agent::SwarmPrep>,
        parent_item_id: &str,
        tx: &async_channel::Sender<Event>,
    ) -> SubagentOutcome {
        let mut cards: Vec<crate::rollout::AgentCardRecord> = Vec::new();
        let mut receipt: Vec<crate::agent::SwarmReceiptChild> = Vec::new();
        // The receipt's running/queued is estimated from free slots at
        // assembly time (a transient value; the real queue state is the
        // "Queued · " prefix in TaskList/panel, removed by
        // mark_agent_task_started once a slot lands)
        let slots_available = crate::task::subagent_slots_available();
        for prep in preps {
            let prep = match prep {
                crate::agent::SwarmPrep::Ready(prep) => prep,
                // Preparation-phase failure: not started, recorded as a failed
                // item in the receipt (partial failure does not affect others)
                crate::agent::SwarmPrep::Failed { description, error } => {
                    receipt.push(crate::agent::SwarmReceiptChild {
                        description,
                        agent_id: None,
                        task_id: None,
                        queued: false,
                        error: Some(error),
                    });
                    continue;
                }
            };
            let drive = SubagentDrive::from(*prep);
            // Agent card (same policy as run_subagent; background batch has
            // background=true)
            let card = crate::rollout::AgentCardRecord {
                agent_id: drive.agent_id.clone(),
                profile: drive.profile.name.clone(),
                description: drive.description.clone(),
                model: agent_card_model(&drive),
                background: true,
            };
            self.emit(
                |session_id, seq| Event::SubagentCard {
                    session_id,
                    seq,
                    item_id: parent_item_id.to_string(),
                    agent_id: card.agent_id.clone(),
                    profile: card.profile.clone(),
                    description: card.description.clone(),
                    model: card.model.clone(),
                    background: card.background,
                },
                tx,
            );
            let description = drive.description.clone();
            let agent_id = drive.agent_id.clone();
            // Registered even while queued (command carries the "Queued · "
            // prefix); visible and stoppable in TaskList/panel
            let child_cancel = CancellationToken::new();
            let command = format!("Subagent {}: {}", drive.profile.name, drive.description);
            let task_id = crate::task::register_agent_task_queued(
                &self.state,
                command,
                child_cancel.clone(),
                agent_id.clone(),
            );
            let queued = cards.len() >= slots_available;
            tokio::spawn(drive_subagent_detached(
                self.detached_gate_ctx(),
                drive,
                task_id.clone(),
                child_cancel,
                tx.clone(),
                std::time::Instant::now(),
            ));
            cards.push(card);
            receipt.push(crate::agent::SwarmReceiptChild {
                description,
                agent_id: Some(agent_id),
                task_id: Some(task_id),
                queued,
                error: None,
            });
        }
        SubagentOutcome::Finished {
            note: crate::agent::format_swarm_receipt(&receipt),
            // Call-level failure only when nothing was dispatched (all
            // preparation-phase failures); partial failures are listed in the
            // receipt
            is_error: cards.is_empty(),
            card: None,
            cards,
        }
    }

    /// Background subagent: register a task entry then tokio::spawn the drive
    /// (fully owned context); on completion update the registry + notify +
    /// wake the parent via the wake channel (killed by TaskStop: not woken).
    /// Returns running immediately.
    fn spawn_subagent_background(
        &self,
        drive: SubagentDrive,
        tx: &async_channel::Sender<Event>,
        card: crate::rollout::AgentCardRecord,
    ) -> SubagentOutcome {
        let bg_cancel = CancellationToken::new();
        let command = format!("Subagent {}: {}", drive.profile.name, drive.description);
        // Registered even while queued (command carries the "Queued · "
        // prefix), visible and stoppable in TaskList/panel;
        // mark_agent_task_started removes the prefix once a concurrency slot
        // lands
        let task_id = crate::task::register_agent_task_queued(
            &self.state,
            command,
            bg_cancel.clone(),
            drive.agent_id.clone(),
        );
        let agent_id = drive.agent_id.clone();
        let started_at = std::time::Instant::now();
        // Immediate receipt for the parent model (assembled up front,
        // independent of the task result); a full concurrency slot means
        // queued
        let queue_hint = if crate::task::subagent_slots_available() == 0 {
            "\nThe global subagent concurrency limit (8) is reached; this task is queued for a free slot and will notify as usual when done."
        } else {
            ""
        };
        let running_note = format!(
            "agent_id: {agent_id}\ntask_id: {task_id}\nstatus: running\nThe subagent is running in the background; the result will arrive as a <task-notification> — do not poll.\nUse TaskOutput for progress, TaskStop to stop it, and Agent(resume=\"{agent_id}\", prompt=\"...\") to resume it.{queue_hint}"
        );
        tokio::spawn(drive_subagent_detached(
            self.detached_gate_ctx(),
            drive,
            task_id,
            bg_cancel,
            tx.clone(),
            started_at,
        ));
        SubagentOutcome::Finished {
            note: running_note,
            is_error: false,
            card: Some(card),
            cards: vec![],
        }
    }

    /// Owned gate context for the background subagent drive (fully owned:
    /// shared Arc clones + value snapshots); spawn_subagent_background and the
    /// background swarm dispatch share the same list
    fn detached_gate_ctx(&self) -> DetachedGateCtx {
        DetachedGateCtx {
            cwd: self.cwd.clone(),
            mode: self.mode,
            permissions: self.permissions.clone(),
            always_allowed: self.always_allowed.clone(),
            state: self.state.clone(),
            pending: self.pending.clone(),
            store: self.store.clone(),
            seq: self.seq.clone(),
            session_id: self.id.clone(),
        }
    }
}

/// Owned gate context for the background subagent drive (assembled by
/// Session::detached_gate_ctx): shared Arc clones + value snapshots; the
/// independent ChangeTracker is created inside the drive — a background
/// subagent's changes do not enter the parent's "changes this turn" panel
/// (still visible in the git-based review panel)
struct DetachedGateCtx {
    cwd: PathBuf,
    mode: ExecMode,
    permissions: crate::permissions::PermissionRules,
    always_allowed: HashSet<(String, String)>,
    state: crate::task::SessionToolState,
    pending: PendingApprovals,
    store: Arc<Mutex<Store>>,
    seq: Arc<std::sync::atomic::AtomicU64>,
    session_id: String,
}

/// Background subagent drive body (shared by spawn_subagent_background and the
/// background swarm): queue on the global concurrency slot (cancellable by
/// TaskStop while queued; the registry is already Killed so exit directly) →
/// drive → live panel/registry wrap-up → note_output → task_notify; when not
/// cancelled, a <task-notification> wakes the parent via the wake channel (the
/// body gives a result.md pointer + Read guidance, no full-text inline — the
/// sessions/ subtree is in the extra_read_roots allowlist, the model Reads it
/// when needed). usage is discarded here: background subagent cost does not go
/// into the parent turn stats.
async fn drive_subagent_detached(
    mut ctx: DetachedGateCtx,
    drive: SubagentDrive,
    task_id: String,
    cancel: CancellationToken,
    tx: async_channel::Sender<Event>,
    started_at: std::time::Instant,
) {
    let agent_id = drive.agent_id.clone();
    let profile_name = drive.profile.name.clone();
    // Structured attributes of the notification's opening tag (for the UI's
    // compact card; the body stays verbatim)
    let description_attr = sanitize_notification_attr(&drive.description, 60);
    let model_attr = format!(
        "{} · {}",
        drive.child_config.provider_name, drive.child_config.model
    );
    // Record file path (absolute path of the subagent context JSONL, through
    // the same attribute sanitization)
    let record_attr = sanitize_notification_attr(&drive.jsonl.display().to_string(), 512);
    // Result full-text path (persisted at drive_subagent wrap-up; the
    // notification body gives the pointer + Read guidance)
    let result_path = agent_result_path(&drive.jsonl, &drive.agent_id);
    let result_attr = sanitize_notification_attr(&result_path.display().to_string(), 512);
    // Global concurrency slot: queue when exceeded
    if crate::task::subagent_slots_available() == 0 {
        crate::task::note_output(
            &ctx.state.tasks,
            &task_id,
            "Queued: the global subagent concurrency limit is reached; waiting for a free slot…\n",
        );
    }
    let Some(_permit) = crate::task::acquire_subagent_slot(&cancel).await else {
        return;
    };
    crate::task::mark_agent_task_started(&ctx.state, &task_id);
    let mut tracker = ChangeTracker::default();
    let result = {
        // MCP tools inherited by the subagent (owned: fetched inside the
        // closure; the rule snapshot lives on drive)
        let mcp_extra = drive.extra_tools();
        let mut gate = GateCtx {
            cwd: &ctx.cwd,
            mode: ctx.mode,
            tracker: &mut tracker,
            state: &ctx.state,
            pending: &ctx.pending,
            permissions: &ctx.permissions,
            always_allowed: &mut ctx.always_allowed,
            session_id: &ctx.session_id,
            plan_enabled: false,
            seq: &ctx.seq,
            store: &ctx.store,
            extra_tools: &mcp_extra,
        };
        let mut drive = drive;
        drive_subagent(
            &mut gate,
            &mut drive,
            &ProgressSink::Background {
                task_id: task_id.clone(),
            },
            &tx,
            &cancel,
        )
        .await
    };
    // Live panel wrap-up (including TaskStop kills): clears the "running"
    // indicator in the "subagents" tab on the right
    emit_bg(&ctx.session_id, &ctx.seq, &tx, |sid, seq| {
        Event::SubagentActivity {
            session_id: sid,
            seq,
            agent_id: agent_id.clone(),
            item: None,
            finished: true,
        }
    });
    // Registry wrap-up: entries already set Killed by TaskStop are not
    // overwritten (the cancelled case)
    {
        let mut tasks = ctx.state.tasks.lock().expect("task registry lock");
        if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id)
            && matches!(entry.status, pig_protocol::TaskStatus::Running)
        {
            entry.status = if result.is_error {
                pig_protocol::TaskStatus::Exited(-1)
            } else {
                pig_protocol::TaskStatus::Exited(0)
            };
            entry.ended_at = Some(crate::rollout::now_secs());
        }
    }
    crate::task::note_output(
        &ctx.state.tasks,
        &task_id,
        &format!("[{}]\n{}\n", result.status_line, result.result_text),
    );
    let _ = ctx.state.task_notify.send(ctx.session_id.clone());
    // Killed by TaskStop: do not wake the parent
    if result.cancelled {
        return;
    }
    // The opening tag's structured attributes stay untouched (the UI compact
    // card's data source); the body aligns with kimi-code: status line +
    // result file path + Read guidance
    let duration_ms = started_at.elapsed().as_millis() as u64;
    let duration = human_duration(duration_ms);
    let written_size = std::fs::metadata(&result_path).ok().map(|m| m.len());
    let body = if result.is_error {
        match written_size {
            Some(bytes) => format!(
                "Background subagent {agent_id} ({profile_name}) failed: {} (took {duration}).\nFull output was written to {} ({}) — view it with Read.\nContinue it with Agent(resume=\"{agent_id}\", prompt=\"...\") (the subagent keeps its full context; have it redo the step that produced no result).",
                result.result_text,
                result_path.display(),
                human_size(bytes)
            ),
            // Persist-failure fallback: no file to point at, reason inlined
            None => format!(
                "Background subagent {agent_id} ({profile_name}) failed: {} (took {duration}).\nContinue it with Agent(resume=\"{agent_id}\", prompt=\"...\") (the subagent keeps its full context; have it redo the step that produced no result).",
                result.result_text
            ),
        }
    } else {
        match written_size {
            Some(bytes) => format!(
                "Background subagent {agent_id} ({profile_name}) completed ({} steps, took {duration}).\nThe result was written to {} ({}) — read it with Read if you need the content.\nContinue this subagent with Agent(resume=\"{agent_id}\", prompt=\"...\").",
                result.turns,
                result_path.display(),
                human_size(bytes)
            ),
            // Persist-failure fallback: no file to point at, inline a
            // ≤3000-char preview (same fallback as kimi)
            None => format!(
                "Background subagent {agent_id} ({profile_name}) completed ({} steps, took {duration}).\n\n{}\n\nContinue this subagent with Agent(resume=\"{agent_id}\", prompt=\"...\").",
                result.turns,
                result.result_text.chars().take(3000).collect::<String>()
            ),
        }
    };
    let notification = if result.is_error {
        format!(
            "<task-notification agent_id=\"{agent_id}\" profile=\"{profile_name}\" status=\"failed\" turns=\"{}\" model=\"{model_attr}\" description=\"{description_attr}\" duration_ms=\"{duration_ms}\" record=\"{record_attr}\" result=\"{result_attr}\">\n{body}\n</task-notification>",
            result.turns
        )
    } else {
        format!(
            "<task-notification agent_id=\"{agent_id}\" profile=\"{profile_name}\" status=\"completed\" turns=\"{}\" model=\"{model_attr}\" description=\"{description_attr}\" duration_ms=\"{duration_ms}\" record=\"{record_attr}\" result=\"{result_attr}\">\n{body}\n</task-notification>",
            result.turns
        )
    };
    let _ = ctx.state.wake_notify.send((ctx.session_id, notification));
}

/// Subagent drive (shared by foreground/background): step loop + narrowed
/// tool gating + context persistence. Fully owned: the foreground borrows
/// Session fields to build GateCtx; in the background even GateCtx is fully
/// owned.
struct SubagentDrive {
    agent_id: String,
    cwd: PathBuf,
    data_dir: PathBuf,
    profile: crate::agent::AgentProfile,
    child_config: ResolvedModel,
    tools: Vec<Box<dyn tool::Tool>>,
    schemas: Vec<serde_json::Value>,
    history: Vec<ChatMsg>,
    jsonl: PathBuf,
    max_turns: usize,
    description: String,
    /// Session MCP handle (None = not connected): backgrounds/closures fetch
    /// inherited tools at GateCtx assembly time
    mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    /// Inheritance rule snapshot (decided from the profile-narrowed result at
    /// preparation): true = inherit all connected MCP tools; false = read-only
    /// profile, inherit only those with readOnlyHint
    mcp_inherits_all: bool,
}

impl SubagentDrive {
    /// extra_tools for the gated execution segment: fetched from the MCP
    /// handle per the inheritance rules (an McpTool clone is cheap) + Skill
    /// (not in the all() static table, served via extra by name)
    fn extra_tools(&self) -> Vec<Box<dyn tool::Tool>> {
        let mut extra = self
            .mcp
            .as_ref()
            .map(|mcp| mcp.child_tools(self.mcp_inherits_all))
            .unwrap_or_default();
        extra.push(Box::new(tool::SkillTool::new(&self.cwd, &self.data_dir)));
        extra
    }
}

/// Model segment of the agent card subtitle: "{provider_name} · {model}"
/// (optionally with a thinking-level suffix)
fn agent_card_model(drive: &SubagentDrive) -> String {
    let mut model = format!(
        "{} · {}",
        drive.child_config.provider_name, drive.child_config.model
    );
    if let Some(level) = &drive.profile.thought_level {
        model = format!("{model} · {level}");
    }
    model
}

/// SwarmChildPrep → SubagentDrive: fields map one to one; the structure
/// mapping between the preparation side and the drive side is collected here
/// (shared by run_swarm's foreground/background branches)
impl From<crate::agent::SwarmChildPrep> for SubagentDrive {
    fn from(prep: crate::agent::SwarmChildPrep) -> Self {
        Self {
            agent_id: prep.agent_id,
            cwd: prep.cwd,
            data_dir: prep.data_dir,
            profile: prep.profile,
            child_config: prep.child_config,
            tools: prep.tools,
            schemas: prep.schemas,
            history: prep.history,
            jsonl: prep.jsonl,
            max_turns: prep.max_turns,
            description: prep.description,
            mcp: prep.mcp,
            mcp_inherits_all: prep.mcp_inherits_all,
        }
    }
}
/// Sink for subagent progress reporting
enum ProgressSink {
    /// Foreground: SubagentProgress streamed live to the parent session's
    /// Agent tool card
    Foreground { parent_item_id: String },
    /// Background: writes the task registry's output (the parent card already
    /// ended, no SubagentProgress)
    Background { task_id: String },
}
impl ProgressSink {
    /// Ownership prefix for child-tool item_ids / approval request_ids
    fn item_prefix(&self) -> &str {
        match self {
            ProgressSink::Foreground { parent_item_id } => parent_item_id,
            ProgressSink::Background { task_id } => task_id,
        }
    }
}
/// Subagent progress reporting (routed per sink, see ProgressSink)
fn report_progress(
    ctx: &GateCtx<'_>,
    sink: &ProgressSink,
    tx: &async_channel::Sender<Event>,
    note: String,
) {
    match sink {
        ProgressSink::Foreground { parent_item_id } => {
            let item_id = parent_item_id.clone();
            emit_bg(ctx.session_id, ctx.seq, tx, |session_id, seq| {
                Event::SubagentProgress {
                    session_id,
                    seq,
                    item_id,
                    note,
                }
            });
        }
        ProgressSink::Background { task_id } => {
            crate::task::note_output(&ctx.state.tasks, task_id, &format!("{note}\n"));
        }
    }
}
/// Append one line to the subagent context JSONL (failure is non-fatal: log
/// and continue, same policy as rollout.append)
fn persist_agent_line(jsonl: &Path, line: &serde_json::Value) {
    if let Err(error) = crate::agent::append_agent_record(jsonl, line) {
        eprintln!("[agent] failed to persist subagent context: {error}");
    }
}
/// Persist a subagent message: base64 is not persisted (same policy as the
/// main rollout); after resume the model cannot see images — acceptable
fn persist_agent_msg(jsonl: &Path, msg: &ChatMsg) {
    let mut msg = msg.clone();
    msg.images.clear();
    persist_agent_line(jsonl, &serde_json::json!({ "type": "msg", "msg": msg }));
}
/// Sanitize a notification opening-tag attribute value: strip `"` and
/// newlines (prevents tag truncation/injection), truncate to max_chars (60
/// for description; 512 for the record path — absolute paths far exceed 60)
fn sanitize_notification_attr(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| !matches!(c, '"' | '\n' | '\r'))
        .take(max_chars)
        .collect()
}
/// Human-readable duration: ≥60s → "Xm Ys", otherwise "X.Xs"
fn human_duration(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}
/// Human-readable file size: KB/MB with one decimal
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    }
}
/// Result of drive_subagent
struct SubagentDriveResult {
    /// One-line status (for the registry output wrap-up)
    status_line: String,
    /// Result text (completed has been truncated to the 32K budget +
    /// persisted; failure carries the reason; cancelled is empty)
    result_text: String,
    /// Steps actually run
    turns: usize,
    /// The subagent finished normally (false on turn exhaustion/request
    /// failure) — used by the foreground template assembly
    completed: bool,
    is_error: bool,
    /// Cumulative sampled tokens (input, cache_read, output): the foreground
    /// adds them into the parent turn stats, the background discards them
    usage: (u64, u64, u64),
    /// Aborted via the driven cancel token (background TaskStop / foreground
    /// parent cancel)
    cancelled: bool,
}
/// Subagent result file path: {agents_dir}/{agent_id}.result.md (same
/// directory as the context jsonl)
fn agent_result_path(jsonl: &Path, agent_id: &str) -> PathBuf {
    jsonl.with_file_name(format!("{agent_id}.result.md"))
}
/// Subagent drive entry: run the drive loop; the full result text is first
/// persisted to {agents_dir}/{agent_id}.result.md (written on success and
/// failure alike — failure writes the error explanation; cancel/kill has no
/// artifact and is skipped; a write failure only degrades — the truncation
/// hint has no file to point at — and is not fatal), then result_text is
/// truncated to the 32K budget.
async fn drive_subagent(
    ctx: &mut GateCtx<'_>,
    run: &mut SubagentDrive,
    progress: &ProgressSink,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> SubagentDriveResult {
    let result_path = agent_result_path(&run.jsonl, &run.agent_id);
    let mut result = drive_subagent_loop(ctx, run, progress, tx, cancel).await;
    if !result.cancelled {
        let written = std::fs::write(&result_path, &result.result_text).is_ok();
        result.result_text = truncate_agent_result(
            written.then_some(result_path.as_path()),
            std::mem::take(&mut result.result_text),
        );
    }
    result
}
/// Subagent live display-item reporting (live-only, sent in both foreground
/// and background): project the new messages in history[emitted..] into
/// display items and emit them one by one (tool results within a batch get
/// their output backfilled by tool_call_id), advancing the watermark. The
/// panel claims them by agent_id.
fn emit_activity_since(
    ctx: &GateCtx<'_>,
    tx: &async_channel::Sender<Event>,
    agent_id: &str,
    history: &[ChatMsg],
    emitted: &mut usize,
) {
    for item in crate::agent::project_display_items(&history[*emitted..]) {
        let agent_id = agent_id.to_string();
        emit_bg(ctx.session_id, ctx.seq, tx, move |session_id, seq| {
            Event::SubagentActivity {
                session_id,
                seq,
                agent_id,
                item: Some(item),
                finished: false,
            }
        });
    }
    *emitted = history.len();
}
/// Subagent drive loop (shared by foreground/background): sampling with an
/// independent context + gated execution with the narrowed tool set; the
/// parent timeline has only one Agent tool card (foreground progress goes
/// through SubagentProgress, child tools emit no top-level events); messages
/// appended at each step are reported live via SubagentActivity (incremental
/// display in the "subagents" tab on the right).
async fn drive_subagent_loop(
    ctx: &mut GateCtx<'_>,
    run: &mut SubagentDrive,
    progress: &ProgressSink,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> SubagentDriveResult {
    let mut usage = (0u64, 0u64, 0u64);
    let mut last_text = String::new();
    let mut steps_run = 0usize;
    let mut completed = false;
    // Live-reporting watermark: length of the history prefix already projected
    // as SubagentActivity (the initial system/user/resume history does not
    // count — LoadSubagent covers it with a full load)
    let mut emitted_up_to = run.history.len();
    for step in 1..=run.max_turns {
        steps_run = step;
        report_progress(ctx, progress, tx, format!("Step {step} · thinking…"));
        let (child_tx, mut child_rx) = tokio::sync::mpsc::unbounded_channel();
        let provider_task = tokio::spawn(provider::stream_chat(
            run.child_config.clone(),
            run.history.clone(),
            run.schemas.clone(),
            child_tx,
            cancel.clone(),
        ));
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut failed: Option<String> = None;
        loop {
            let event = tokio::select! {
                event = child_rx.recv() => event,
                _ = cancel.cancelled() => None,
            };
            match event {
                // Subagent reasoning/text deltas are not forwarded as
                // top-level events: the parent timeline only gets progress
                // status lines
                Some(ProviderEvent::Reasoning(delta)) => reasoning.push_str(&delta),
                Some(ProviderEvent::Text(delta)) => text.push_str(&delta),
                Some(ProviderEvent::ToolCalls(calls)) => tool_calls = calls,
                Some(ProviderEvent::Usage {
                    input,
                    cache_read,
                    output,
                    ..
                }) => {
                    // Token usage is carried out via the return value: the
                    // foreground adds it into the parent turn stats, the
                    // background discards it; no StepUsage recorded / watermark
                    // untouched (the watermark tracks the parent session's own
                    // context)
                    usage.0 += input;
                    usage.1 += cache_read;
                    usage.2 += output;
                }
                Some(ProviderEvent::Finished) | None => break,
                Some(ProviderEvent::Failed(error)) => {
                    failed = Some(crate::provider::core_error_en(&error));
                    break;
                }
            }
        }
        if let Some(error) = failed {
            provider_task.abort();
            // Before wrapping up, report this step's already-recorded messages
            // to the live panel (same below)
            emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
            let note = format!("Subagent model request failed: {error}");
            return SubagentDriveResult {
                status_line: format!("failed: {note}"),
                result_text: note,
                turns: steps_run,
                completed: false,
                is_error: true,
                usage,
                cancelled: false,
            };
        }
        if cancel.is_cancelled() {
            provider_task.abort();
            emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
            return SubagentDriveResult {
                status_line: "cancelled".to_string(),
                result_text: String::new(),
                turns: steps_run,
                completed: false,
                is_error: false,
                usage,
                cancelled: true,
            };
        }
        let _ = provider_task.await;
        let assistant = ChatMsg::assistant(
            text,
            tool_calls.clone(),
            Some(reasoning).filter(|r| !r.is_empty()),
        );
        last_text = assistant.content.clone().unwrap_or_default();
        run.history.push(assistant);
        persist_agent_msg(&run.jsonl, run.history.last().expect("assistant pushed"));
        if tool_calls.is_empty() {
            // This step has only the assistant message: report before wrap-up
            emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
            completed = true;
            break;
        }
        for child_call in &tool_calls {
            report_progress(
                ctx,
                progress,
                tx,
                format!("Step {step} · {}", tool::summarize(child_call)),
            );
            let child_item_id = format!("{}-c{step}-{}", progress.item_prefix(), child_call.id);
            let Some(child_tool) = run
                .tools
                .iter()
                .find(|t| t.name() == child_call.name)
                .map(|t| t.as_ref())
            else {
                // The narrowed tool set has no such name: record an error
                // result and continue (the subagent is not interrupted)
                let note = format!(
                    "Unknown tool {} (the subagent's tool set is narrowed)",
                    child_call.name
                );
                run.history.push(ChatMsg::tool_result(&child_call.id, note));
                persist_agent_msg(&run.jsonl, run.history.last().expect("tool result pushed"));
                continue;
            };
            match exec_tool_gated_ctx(
                ctx,
                child_call,
                Some(child_tool),
                &child_item_id,
                progress.item_prefix(),
                tx,
                cancel,
            )
            .await
            {
                GatedToolOutcome::Cancelled => {
                    emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
                    return SubagentDriveResult {
                        status_line: "cancelled".to_string(),
                        result_text: String::new(),
                        turns: steps_run,
                        completed: false,
                        is_error: false,
                        usage,
                        cancelled: true,
                    };
                }
                GatedToolOutcome::Rejected { note } => {
                    run.history.push(ChatMsg::tool_result(&child_call.id, note));
                    persist_agent_msg(&run.jsonl, run.history.last().expect("tool result pushed"));
                }
                GatedToolOutcome::Executed { output, images, .. } => {
                    run.history.push(ChatMsg::tool_result_with_images(
                        &child_call.id,
                        output,
                        images,
                    ));
                    persist_agent_msg(&run.jsonl, run.history.last().expect("tool result pushed"));
                }
            }
        }
        // End-of-step reporting: this step's assistant (with tool_calls) + all
        // tool results are projected in one batch, with output backfilled by
        // tool_call_id within the batch
        emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
    }
    // ---- wrap-up: return the untruncated full text (result.md persistence
    // and the 32K budget truncation are finalized in the outer
    // drive_subagent) ----
    if completed {
        if last_text.is_empty() {
            SubagentDriveResult {
                status_line: "failed: subagent produced no final text".to_string(),
                result_text: "The subagent produced no final text".to_string(),
                turns: steps_run,
                completed: true,
                is_error: true,
                usage,
                cancelled: false,
            }
        } else {
            SubagentDriveResult {
                status_line: format!("completed ({steps_run} steps)"),
                result_text: last_text,
                turns: steps_run,
                completed: true,
                is_error: false,
                usage,
                cancelled: false,
            }
        }
    } else if last_text.is_empty() {
        SubagentDriveResult {
            status_line: format!(
                "failed: reached the maximum turn count ({}), subagent produced no conclusion",
                run.max_turns
            ),
            result_text: format!(
                "Reached the maximum turn count ({}). The subagent produced no conclusion",
                run.max_turns
            ),
            turns: steps_run,
            completed: false,
            is_error: true,
            usage,
            cancelled: false,
        }
    } else {
        SubagentDriveResult {
            status_line: format!(
                "completed (reached the maximum turn count {})",
                run.max_turns
            ),
            result_text: format!(
                "Reached the maximum turn count ({}). {last_text}",
                run.max_turns
            ),
            turns: steps_run,
            completed: false,
            is_error: false,
            usage,
            cancelled: false,
        }
    }
}
/// Subagent result budget: returned verbatim within 32K chars; beyond that,
/// return the first 32K + truncation guidance. The full text is persisted
/// first by the caller to {agents_dir}/{agent_id}.result.md (same as kimi's
/// output.log), which result_path points to; None = persist failure.
fn truncate_agent_result(result_path: Option<&Path>, result: String) -> String {
    const MAX_RESULT_CHARS: usize = 32_000;
    if result.chars().count() <= MAX_RESULT_CHARS {
        return result;
    }
    let hint = match result_path {
        Some(path) => format!(
            "\n\n[Result too long; truncated. Full text: {}]",
            path.display()
        ),
        None => "\n\n[Result too long; truncated. Failed to save the full text]".to_string(),
    };
    let head: String = result.chars().take(MAX_RESULT_CHARS).collect();
    format!("{head}{hint}")
}
