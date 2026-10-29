use super::*;

mod compact;
mod drive;
mod lifecycle;
mod managed;
mod persist;
mod state;

#[cfg(test)]
mod tests;

use drive::{agent_result_path, drive_subagent};
use lifecycle::ChildLifecycle;
use managed::{
    DetachedGateCtx, SubagentSlot, SwarmChildIdentity, aborted_swarm_child, drive_managed,
    spawn_detached_child, subagent_receipt, swarm_child_result,
};
use state::{ProgressSink, SubagentDrive, agent_card_model};

/// Child-local tool state: tasks, steers, writers, read states and roots are
/// shared with the session, but the todo list is a fresh per-child handle.
/// Waiting in a child is a query, not delivery into the root model's history.
fn child_local_state(state: &crate::task::SessionToolState) -> crate::task::SessionToolState {
    let mut state = state.clone();
    state.todos = Default::default();
    state.task_store = None;
    state
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

impl Session {
    /// Agent tool entry: argument gating → the shared child preparation
    /// builder (profile/model/tool/MCP/media/JSONL, identical to every swarm
    /// item) → the one managed child driver (global slot, exclusive context
    /// writer, panic/cancel settlement); foreground borrows Session fields,
    /// background owns them and reports terminal state through the task outbox. resume reuses
    /// the original agent id and re-resolves profile/model from current state.
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

        // ---- one shared child preparation pipeline for single Agent and
        // swarm: trust, profile/model resolution, tool narrowing, MCP
        // inheritance, media projection, writer claim and JSONL persistence ----
        let runtime = self.runtime_config_cloned();
        // Every child kind derives its project surface from the same resolved
        // runtime trust decision (the session token is kept in sync with it).
        let trust = runtime
            .as_ref()
            .map(|runtime| runtime.trust().clone())
            .unwrap_or_else(|| crate::config::WorkspaceTrust::untrusted(&self.cwd));
        let profiles = crate::agent::load_profiles_with_trust(&self.cwd, &self.data_dir, &trust);
        let ctx = crate::agent::ChildPrepCtx {
            cwd: &self.cwd,
            data_dir: &self.data_dir,
            git_snapshot: self.git_snapshot.as_deref(),
            skills_prompt: &self.skills_prompt,
            agents_prompt: &self.agents_prompt,
            runtime: runtime.as_deref(),
            parent_config,
            session_id: &self.id,
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
            match crate::agent::prepare_fresh_child(
                &ctx,
                profile,
                child_config,
                prompt_text,
                description,
                &mut self.agent_seq,
            ) {
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
        let mut drive = SubagentDrive::from(prep);

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
            |session_id| Event::SubagentCard {
                session_id,
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

        // ---- foreground: the same managed driver as every other child
        // (global slot, panic/cancel settlement). The child tracker shares the
        // session originals and this turn's summary, so its writes contribute
        // to the parent's TurnChanges exactly once. ----
        let progress = ProgressSink::Foreground {
            parent_item_id: parent_item_id.to_string(),
        };
        let mut tracker = self.tracker.foreground_child();
        let child_tool_state = child_local_state(&self.state);
        let life_writer = drive
            .take_writer()
            .expect("foreground subagent writer was claimed");
        let life = ChildLifecycle::new(
            &self.state,
            &self.pending,
            &self.id,
            tx.clone(),
            &drive,
            life_writer,
            &progress,
            None,
        );
        let managed = drive_managed(
            life,
            cancel,
            async {
                crate::task::acquire_subagent_slot(cancel)
                    .await
                    .map(SubagentSlot::Global)
            },
            async {
                let mcp_extra = drive.extra_tools();
                let mut gate = GateCtx {
                    cwd: &self.cwd,
                    permissions: self.permissions,
                    media: tool::MediaCapabilities::new(
                        drive.child_config.input_image,
                        drive.child_config.input_pdf,
                    ),
                    tracker: &mut tracker,
                    state: &child_tool_state,
                    owns_todo_state: false,
                    pending: &self.pending,
                    permission_rules: &mut self.permission_rules,
                    always_allowed: &mut self.always_allowed,
                    session_id: &self.id,
                    plan_enabled: false,
                    store: &self.store,
                    extra_tools: &mcp_extra,
                    hooks: &self.hooks,
                    history: &self.history,
                };
                drive_subagent(&mut gate, &mut drive, &progress, tx, cancel).await
            },
        )
        .await;
        let result = managed.result;
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
        let note = subagent_receipt(&drive.agent_id, &drive.profile.name, &result);
        SubagentOutcome::Finished {
            note,
            is_error: result.is_error,
            card: Some(card_record),
            cards: vec![],
        }
    }
    /// AgentSwarm tool entry: parse_swarm_args validation →
    /// prepare_swarm_children batch preparation through the shared child
    /// builder (identical to a single Agent) → drive all concurrently
    /// (global concurrency slot cap;
    /// queue when exceeded). Default foreground: blocks until all finish, with
    /// the aggregated result as a single tool result; turn cancel → cancel all
    /// subagents. run_in_background=true: register background tasks one by one
    /// and spawn independent drives (same lifecycle as background Agent:
    /// stoppable via TaskStop, completions wake the parent one by one via the
    /// task outbox), returning per-item receipts immediately.
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
        let runtime = self.runtime_config_cloned();
        let preps = match crate::agent::prepare_swarm_children(
            &crate::agent::ChildPrepCtx {
                cwd: &self.cwd,
                data_dir: &self.data_dir,
                git_snapshot: self.git_snapshot.as_deref(),
                skills_prompt: &self.skills_prompt,
                agents_prompt: &self.agents_prompt,
                runtime: runtime.as_deref(),
                parent_config,
                session_id: &self.id,
                state: &self.state,
                mcp: self.mcp.as_ref(),
                plugin_tools: self.plugin_tools.clone(),
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

        // ---- foreground: drive all concurrently through the managed child
        // driver, block until all finish ----
        // Each subagent's card metadata is collected one by one (sent live via
        // the SubagentCard event; persisted as agent_cards with the rollout
        // ToolCall record, replay rebuilds all subagent cards)
        let mut cards: Vec<crate::rollout::AgentCardRecord> = Vec::new();
        // slots preserve plan order (prep-failed entries land Failed in place;
        // spawn results are backfilled by ix)
        let mut slots: Vec<Option<crate::agent::SwarmChildResult>> =
            (0..preps.len()).map(|_| None).collect();
        let mut identities: Vec<Option<SwarmChildIdentity>> =
            (0..preps.len()).map(|_| None).collect();
        let mut handles = Vec::new();
        let mut handle_ixs = Vec::new();
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
            let mut drive = SubagentDrive::from(*prep);
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
                |session_id| Event::SubagentCard {
                    session_id,
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
            let identity = SwarmChildIdentity {
                description: drive.description.clone(),
                agent_id: drive.agent_id.clone(),
                result_path: agent_result_path(&drive.jsonl, &drive.agent_id),
            };
            identities[ix] = Some(identity.clone());
            // Registered in the Queued state; visible and stoppable in TaskList/panel
            let child_cancel = CancellationToken::new();
            let command = format!("Subagent {}: {}", drive.profile.name, drive.description);
            let task_id = crate::task::register_agent_task_queued(
                &self.state,
                command,
                child_cancel.clone(),
                drive.agent_id.clone(),
            );
            let writer = drive
                .take_writer()
                .expect("swarm subagent writer was claimed");
            // Fully owned context (same list as spawn_subagent_background)
            let cwd = self.cwd.clone();
            let permissions = self.permissions;
            let hooks = self.hooks.clone();
            let mut permission_rules = self.permission_rules.clone();
            let mut always_allowed = self.always_allowed.clone();
            let state = self.state.clone();
            let child_tool_state = child_local_state(&state);
            let pending = self.pending.clone();
            let store = self.store.clone();
            let session_id = self.id.clone();
            // Transcript snapshot for the hook gates (the child drives its own
            // loop; the snapshot is the conversation at dispatch time)
            let history = self.history.clone();
            let tx_bg = tx.clone();
            let progress = ProgressSink::Foreground {
                parent_item_id: parent_item_id.to_string(),
            };
            let task_id_bg = task_id.clone();
            let mut tracker = self.tracker.foreground_child();
            // Construct the lifecycle before spawning: if the runtime drops the
            // task before its first poll, the captured lifecycle's Drop still
            // releases the writer and settles the registry entry.
            let life = ChildLifecycle::new(
                &state,
                &pending,
                &session_id,
                tx_bg.clone(),
                &drive,
                writer,
                &progress,
                Some(task_id_bg),
            );
            tokens.push(child_cancel.clone());
            handle_ixs.push(ix);
            handles.push(tokio::spawn(async move {
                let mut drive = drive;
                let managed = drive_managed(
                    life,
                    &child_cancel,
                    async {
                        crate::task::acquire_subagent_slot(&child_cancel)
                            .await
                            .map(SubagentSlot::Global)
                    },
                    async {
                        let mcp_extra = drive.extra_tools();
                        let mut gate = GateCtx {
                            cwd: &cwd,
                            permissions,
                            media: tool::MediaCapabilities::new(
                                drive.child_config.input_image,
                                drive.child_config.input_pdf,
                            ),
                            tracker: &mut tracker,
                            state: &child_tool_state,
                            owns_todo_state: false,
                            pending: &pending,
                            permission_rules: &mut permission_rules,
                            always_allowed: &mut always_allowed,
                            session_id: &session_id,
                            plan_enabled: false,
                            store: &store,
                            extra_tools: &mcp_extra,
                            hooks: &hooks,
                            history: &history,
                        };
                        drive_subagent(&mut gate, &mut drive, &progress, &tx_bg, &child_cancel)
                            .await
                    },
                )
                .await;
                (ix, managed)
            }));
        }

        // All concurrent; parent cancel → cancel all subagents and wait for
        // the wrap-up (Cancelled semantics match the foreground Agent: partial
        // results are excluded from the aggregation, the tool card settles as
        // "Stopped")
        let (joined, parent_cancelled) = tokio::select! {
            // An already-fired parent cancel must win even when the children
            // settle in the same poll: the tool card is "Stopped", not a
            // partial aggregate. Still join every child so its terminal
            // cleanup (registry/writer/approvals) has completed before return.
            biased;
            _ = cancel.cancelled() => {
                for token in &tokens {
                    token.cancel();
                }
                (futures_util::future::join_all(&mut handles).await, true)
            }
            joined = futures_util::future::join_all(&mut handles) => (joined, false),
        };
        for (item, ix) in joined.into_iter().zip(handle_ixs) {
            match item {
                Ok((reported_ix, managed)) => {
                    debug_assert_eq!(reported_ix, ix);
                    slots[reported_ix] = Some(swarm_child_result(
                        identities[reported_ix].as_ref(),
                        managed,
                    ));
                }
                Err(error) => {
                    tracing::error!("swarm subagent task aborted: {error}");
                    if let Some(identity) = identities[ix].as_ref() {
                        slots[ix] = Some(aborted_swarm_child(
                            identity,
                            format!("Subagent driver task failed: {error}"),
                        ));
                    }
                }
            }
        }
        if parent_cancelled {
            return SubagentOutcome::Cancelled { card: None, cards };
        }
        let children: Vec<crate::agent::SwarmChildResult> = slots
            .into_iter()
            .enumerate()
            .map(|(ix, slot)| {
                slot.unwrap_or_else(|| {
                    identities[ix].as_ref().map_or_else(
                        || crate::agent::SwarmChildResult {
                            description: "swarm subtask".to_string(),
                            agent_id: None,
                            status: crate::agent::SwarmChildStatus::Failed,
                            turns: 0,
                            result_path: None,
                            result_text: "Subagent result missing".to_string(),
                            queued: false,
                            usage: (0, 0, 0),
                        },
                        |identity| {
                            aborted_swarm_child(
                                identity,
                                "Subagent driver ended without a result".to_string(),
                            )
                        },
                    )
                })
            })
            .collect();
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
    /// background drive through the managed child driver (shared with the
    /// background Agent: concurrency-slot queueing/registry wrap-up/result
    /// persistence/completion notifications through the shared task outbox);
    /// no join,
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
        // TaskStatus::Queued in TaskList/panel, moved to Running by
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
                |session_id| Event::SubagentCard {
                    session_id,
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
            // Registered in the Queued state; visible and stoppable in TaskList/panel
            let child_cancel = CancellationToken::new();
            let command = format!("Subagent {}: {}", drive.profile.name, drive.description);
            let task_id = crate::task::register_agent_task_queued(
                &self.state,
                command,
                child_cancel.clone(),
                agent_id.clone(),
            );
            let queued = cards.len() >= slots_available;
            spawn_detached_child(
                self.detached_gate_ctx(),
                drive,
                task_id.clone(),
                child_cancel,
                tx.clone(),
            );
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

    /// Background subagent: its terminal state uses the same task outbox as a
    /// background shell, including cancellation and driver failure.
    fn spawn_subagent_background(
        &self,
        drive: SubagentDrive,
        tx: &async_channel::Sender<Event>,
        card: crate::rollout::AgentCardRecord,
    ) -> SubagentOutcome {
        let bg_cancel = CancellationToken::new();
        let command = format!("Subagent {}: {}", drive.profile.name, drive.description);
        let task_id = crate::task::register_agent_task_queued(
            &self.state,
            command,
            bg_cancel.clone(),
            drive.agent_id.clone(),
        );
        let agent_id = drive.agent_id.clone();
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
        spawn_detached_child(
            self.detached_gate_ctx(),
            drive,
            task_id,
            bg_cancel,
            tx.clone(),
        );
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
            permissions: self.permissions,
            hooks: self.hooks.clone(),
            permission_rules: self.permission_rules.clone(),
            always_allowed: self.always_allowed.clone(),
            state: self.state.clone(),
            tool_state: child_local_state(&self.state),
            pending: self.pending.clone(),
            store: self.store.clone(),
            session_id: self.id.clone(),
            // Transcript snapshot for the hook gates (dispatch-time view of
            // the conversation)
            history: self.history.clone(),
            // Shares the session's first-seen originals (so a background write
            // never replaces the persisted pre-change baseline) but keeps an
            // isolated turn scope.
            tracker: self.tracker.detached_child(),
        }
    }
}
