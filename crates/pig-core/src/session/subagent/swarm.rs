use super::*;

impl Session {
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
    pub(crate) fn run_swarm<'a>(
        &'a mut self,
        call: &'a ToolCall,
        parent_item_id: &'a str,
        parent_config: &'a ResolvedModel,
        tx: &'a async_channel::Sender<Event>,
        cancel: &'a CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SubagentOutcome> + Send + 'a>> {
        Box::pin(self.run_swarm_inner(call, parent_item_id, parent_config, tx, cancel))
    }

    async fn run_swarm_inner(
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
        if let Err(note) = self.check_delegation_entry() {
            return fail(note);
        }
        let background = args["run_in_background"].as_bool().unwrap_or(false);
        let runtime = self.runtime_config_cloned();
        let session_id = self.event_session_id().to_string();
        let preps = match crate::agent::prepare_swarm_children(
            &crate::agent::ChildPrepCtx {
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
                max_agents_per_session: runtime
                    .as_ref()
                    .map_or(64, |runtime| runtime.config().agents.max_agents_per_session),
                allowed_subagents: self
                    .subagent
                    .as_ref()
                    .and_then(|subagent| subagent.profile.subagents.as_deref()),
                child_cwd: plan.cwd.clone(),
                worktree: plan.worktree,
                parent_permissions: self.permissions,
                parent_history: &self.agent.history,
                profiles_summary: &self.profiles_summary,
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
            let description = prep.description.clone();
            let profile = prep.policy.profile.name.clone();
            let agent_id = prep.agent_id.clone();
            let jsonl = prep.jsonl.clone();
            // Registered in the Queued state; visible and stoppable in TaskList/panel
            let child_cancel = CancellationToken::new();
            let command = format!("Subagent {profile}: {description}");
            let task_id = crate::task::register_agent_run_queued(
                &self.state,
                command,
                child_cancel.clone(),
                prep.run_metadata(),
                false,
            );
            let progress = ProgressSink::Foreground {
                parent_item_id: parent_item_id.to_string(),
            };
            let mut child = Session::new_subagent(*prep, self, progress, SubagentMode::Foreground);
            // Agent card (same policy as run_subagent; batch foreground has
            // background=false)
            let card = self.emit_child_card(&child, parent_item_id, tx, false);
            cards.push(card);
            let identity = SwarmChildIdentity {
                description,
                agent_id,
                result_path: agent_result_path(&jsonl, &child.agent.id),
            };
            identities[ix] = Some(identity.clone());
            let writer = child
                .subagent
                .as_mut()
                .expect("child session")
                .take_writer()
                .expect("swarm subagent writer was claimed");
            // Construct the lifecycle before spawning: if the runtime drops the
            // task before its first poll, the captured lifecycle's Drop still
            // releases the writer and settles the registry entry.
            let life = AgentRunSupervisor::new(
                &child.state,
                &child.pending,
                child.event_session_id(),
                tx.clone(),
                &child,
                writer,
                Some(task_id),
            );
            tokens.push(child_cancel.clone());
            handle_ixs.push(ix);
            let tx_bg = tx.clone();
            let task_cancel = child_cancel.clone();
            handles.push(tokio::spawn(async move {
                let child_task = crate::agent::execute_local(
                    child,
                    tx_bg,
                    task_cancel,
                    crate::agent::AgentExecutionActivation::Start,
                );
                let managed = life
                    .drive(
                        &child_cancel,
                        crate::agent::scheduler().acquire(&child_cancel),
                        child_task,
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
            self.run_state.input += child.usage.0;
            self.run_state.cache_read += child.usage.1;
            self.run_state.output += child.usage.2;
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
        let slots_available = crate::agent::scheduler().available();
        for prep in preps {
            let prep = match prep {
                crate::agent::SwarmPrep::Ready(prep) => prep,
                // Preparation-phase failure: not started, recorded as a failed
                // item in the receipt (partial failure does not affect
                // others)
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
            let description = prep.description.clone();
            let profile = prep.policy.profile.name.clone();
            let agent_id = prep.agent_id.clone();
            // Registered in the Queued state; visible and stoppable in TaskList/panel
            let child_cancel = CancellationToken::new();
            let command = format!("Subagent {profile}: {description}");
            let task_id = crate::task::register_agent_run_queued(
                &self.state,
                command,
                child_cancel.clone(),
                prep.run_metadata(),
                true,
            );
            let progress = ProgressSink::Background {
                task_id: task_id.clone(),
            };
            let child = Session::new_subagent(*prep, self, progress, SubagentMode::Background);
            // Agent card (same policy as run_subagent; background batch has
            // background=true)
            let card = self.emit_child_card(&child, parent_item_id, tx, true);
            let queued = cards.len() >= slots_available;
            spawn_detached_child(
                child,
                task_id.clone(),
                child_cancel,
                tx.clone(),
                None,
                crate::agent::AgentExecutionActivation::Start,
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
}
