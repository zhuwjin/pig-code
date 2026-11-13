//! Subagent orchestration.
//!
//! A child is a full `Session`: the shared engine drives its steps exactly
//! like a root turn. This module owns the lifecycle around that Session —
//! preparation, concurrency slots, context persistence, tool narrowing,
//! delegation depth and settlement.

use super::*;

mod compact;
mod lifecycle;
mod managed;
mod persist;
mod state;
mod turn;

#[cfg(test)]
mod tests;

use lifecycle::ChildLifecycle;
use managed::{
    DelegatedUi, SubagentSlot, SwarmChildIdentity, aborted_swarm_child, drive_managed,
    spawn_detached_child, subagent_receipt, swarm_child_result,
};
pub(super) use state::SubagentState;
use state::{ProgressSink, SubagentMode, agent_card_model};
use turn::agent_result_path;

/// Child tool sets exclude Agent/AgentSwarm, so the current orchestration
/// admits exactly one delegation level. The depth lives on the agent context
/// and is checked at every entry point (a future nested mode lifts this).
const MAX_DELEGATION_DEPTH: u32 = 1;

/// The immediate answer of a plugin-delegated child.
pub(crate) struct DelegatedAgentStarted {
    pub agent_id: String,
    pub task_id: String,
    pub profile: String,
    pub model: String,
    pub background: bool,
}

/// Result of running one child agent
struct SubagentResult {
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
        if self.agent.delegation_depth >= MAX_DELEGATION_DEPTH {
            return fail(
                "Nested subagents are not supported: this agent is already a delegated child."
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
            session_id: &self.agent.id,
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
        // ---- background: register a task first (its id is the child's
        // progress/ownership prefix), then spawn the detached child ----
        if background {
            let agent_id = prep.agent_id.clone();
            let profile = prep.profile.name.clone();
            let description = prep.description.clone();
            let bg_cancel = CancellationToken::new();
            let command = format!("Subagent {profile}: {description}");
            let task_id = crate::task::register_agent_task_queued(
                &self.state,
                command,
                bg_cancel.clone(),
                agent_id.clone(),
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
            None,
        );
        let child_task: std::pin::Pin<
            Box<dyn std::future::Future<Output = SubagentResult> + Send + '_>,
        > = Box::pin(async move { child.run_subagent_task(tx, cancel).await });
        let managed = drive_managed(
            life,
            cancel,
            async {
                crate::task::acquire_subagent_slot(cancel)
                    .await
                    .map(SubagentSlot::Global)
            },
            child_task,
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
        let note = subagent_receipt(&agent_id, &profile, &result);
        SubagentOutcome::Finished {
            note,
            is_error: result.is_error,
            card: Some(card_record),
            cards: vec![],
        }
    }

    /// Emit one Agent card for a child and return its durable record.
    fn emit_child_card(
        &mut self,
        child: &Session,
        parent_item_id: &str,
        tx: &async_channel::Sender<Event>,
        background: bool,
    ) -> crate::rollout::AgentCardRecord {
        let sub = child.subagent.as_ref().expect("child session");
        let card = crate::rollout::AgentCardRecord {
            agent_id: child.agent.id.clone(),
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
                profile: card.profile.clone(),
                description: card.description.clone(),
                model: card.model.clone(),
                background: card.background,
            },
            tx,
        );
        card
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
        if self.agent.delegation_depth >= MAX_DELEGATION_DEPTH {
            return fail(
                "Nested subagents are not supported: this agent is already a delegated child."
                    .to_string(),
            );
        }
        let background = args["run_in_background"].as_bool().unwrap_or(false);
        let runtime = self.runtime_config_cloned();
        let preps = match crate::agent::prepare_swarm_children(
            &crate::agent::ChildPrepCtx {
                cwd: &self.agent.cwd,
                data_dir: &self.data_dir,
                git_snapshot: self.git_snapshot.as_deref(),
                skills_prompt: &self.skills_prompt,
                agents_prompt: &self.agents_prompt,
                runtime: runtime.as_deref(),
                parent_config,
                session_id: &self.agent.id,
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
            let profile = prep.profile.name.clone();
            let agent_id = prep.agent_id.clone();
            let jsonl = prep.jsonl.clone();
            // Registered in the Queued state; visible and stoppable in TaskList/panel
            let child_cancel = CancellationToken::new();
            let command = format!("Subagent {profile}: {description}");
            let task_id = crate::task::register_agent_task_queued(
                &self.state,
                command,
                child_cancel.clone(),
                agent_id.clone(),
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
            let life = ChildLifecycle::new(
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
                let child_task: std::pin::Pin<
                    Box<dyn std::future::Future<Output = SubagentResult> + Send + 'static>,
                > = Box::pin(async move { child.run_subagent_task(&tx_bg, &task_cancel).await });
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
            let description = prep.description.clone();
            let profile = prep.profile.name.clone();
            let agent_id = prep.agent_id.clone();
            // Registered in the Queued state; visible and stoppable in TaskList/panel
            let child_cancel = CancellationToken::new();
            let command = format!("Subagent {profile}: {description}");
            let task_id = crate::task::register_agent_task_queued(
                &self.state,
                command,
                child_cancel.clone(),
                agent_id.clone(),
            );
            let progress = ProgressSink::Background {
                task_id: task_id.clone(),
            };
            let child = Session::new_subagent(*prep, self, progress, SubagentMode::Background);
            // Agent card (same policy as run_subagent; background batch has
            // background=true)
            let card = self.emit_child_card(&child, parent_item_id, tx, true);
            let queued = cards.len() >= slots_available;
            spawn_detached_child(child, task_id.clone(), child_cancel, tx.clone(), None);
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
        if self.agent.delegation_depth >= MAX_DELEGATION_DEPTH {
            return Err(
                "Nested subagents are not supported: this agent is already a delegated child."
                    .into(),
            );
        }
        let description = description.trim();
        let prompt_text = prompt_text.trim();
        if description.is_empty() || prompt_text.is_empty() {
            return Err("a delegated agent needs a description and a prompt".into());
        }
        let runtime = self.runtime_config_cloned();
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
            session_id: &self.agent.id,
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
        let profile_name = prep.profile.name.clone();
        let bg_cancel = CancellationToken::new();
        let command = format!("Subagent {profile_name}: {description}");
        let task_id = crate::task::register_agent_task_queued(
            &self.state,
            command,
            bg_cancel.clone(),
            agent_id.clone(),
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
