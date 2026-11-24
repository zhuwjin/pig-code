//! Early execution of read-only tool calls: Codex overlaps tool futures with
//! the rest of the response stream. When a call's arguments are complete
//! mid-stream and it is provably approval-free and conflict-free with every
//! call before it, it starts at once in its own task while the model keeps
//! streaming. Nothing is visible until the assistant message is committed:
//! events, rollout records and the repeat breaker all stay at the batch,
//! where the call's wave joins the finished read instead of running it again.
//!
//! Root turns only, and only calls that leave nothing behind in the session:
//! a wave that never gets to the call (failed or cancelled attempt, different
//! final arguments) drops the task, which aborts it, with nothing to undo.

use super::*;

/// The early run of one call: its gated outcome and how long the read took.
pub(crate) type EarlyTask = tokio::task::JoinHandle<(GatedToolOutcome, u64)>;

/// Built-in reads with no session side effect: the slow ones, where overlap
/// with the rest of the stream is worth having.
///
/// `Read` is deliberately absent. It records what it showed, so that an
/// identical later Read can answer "file unchanged"; a read abandoned with
/// its stream would leave that record for content the model never saw, and
/// the next Read would then skip the very text the model is waiting for.
/// Reading one file is fast anyway. Runtime (MCP/plugin) tools stay out too:
/// they may be slow or remote, or have effects on their server that the
/// concurrency classification cannot see.
const EARLY_TOOLS: &[&str] = &["Grep", "Glob", "FetchURL"];

/// One call whose input streamed during the current attempt.
#[derive(Clone, Debug, Default)]
pub(crate) struct StreamedCall {
    id: String,
    name: String,
    /// Raw accumulated argument JSON.
    arguments: String,
    /// The arguments parsed as one complete JSON value.
    complete: bool,
}

/// A read-only call that started executing before the assistant message was
/// committed. Dropping it aborts a task nobody joined.
pub(crate) struct EarlyTool {
    /// Provider streaming index; a restarted index abandons the entry.
    pub index: usize,
    id: String,
    name: String,
    arguments: String,
    task: Option<EarlyTask>,
}

impl EarlyTool {
    /// The early run counts for `call` only when the final batch carries the
    /// same id, name and argument text the stream accumulated.
    fn matches(&self, call: &ToolCall) -> bool {
        self.id == call.id && self.name == call.name && self.arguments == call.arguments
    }
}

impl Drop for EarlyTool {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Session {
    /// Root stream: the tool call at `index` became identifiable.
    pub(crate) fn note_tool_started(&mut self, index: usize, id: &str, name: &str) {
        self.early_tools.retain(|early| early.index != index);
        self.streamed_calls.insert(
            index,
            StreamedCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: String::new(),
                complete: false,
            },
        );
    }

    /// Root stream: `arguments` is the whole argument JSON accumulated so far
    /// for the call at `index`. The first time it parses as one complete
    /// value, the call may start executing early.
    pub(crate) fn note_tool_arguments(
        &mut self,
        index: usize,
        arguments: &str,
        rt: &engine::StepRuntime,
        tx: &async_channel::Sender<Event>,
    ) {
        let Some(call) = self.streamed_calls.get_mut(&index) else {
            return;
        };
        call.arguments.clear();
        call.arguments.push_str(arguments);
        if call.complete || serde_json::from_str::<serde_json::Value>(arguments).is_err() {
            return;
        }
        call.complete = true;
        self.start_early_tool(index, rt, tx);
    }

    fn start_early_tool(
        &mut self,
        index: usize,
        rt: &engine::StepRuntime,
        tx: &async_channel::Sender<Event>,
    ) {
        let is_early_read = self
            .streamed_calls
            .get(&index)
            .is_some_and(|call| EARLY_TOOLS.contains(&call.name.as_str()));
        if self.is_subagent() || !is_early_read {
            return;
        }
        let (turn_id, _, control, _, _) = rt.root_parts();

        // The call may run early only if it would share the batch's first wave
        // with everything before it: each earlier call must have finished
        // streaming and be neither exclusive nor conflicting with this one. A
        // later conflicting call is placed in a later wave, where it waits.
        let tools = self.root_tools();
        let rules = self
            .permission_rules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut plans: Vec<scheduler::ToolAccessPlan> = Vec::new();
        for (other_index, other) in &self.streamed_calls {
            if *other_index > index {
                break;
            }
            if !other.complete {
                return;
            }
            let plan = scheduler::classify_tool_access(
                &ToolCall {
                    id: other.id.clone(),
                    name: other.name.clone(),
                    arguments: other.arguments.clone(),
                },
                &tools,
                self.permissions,
                self.plan_enabled,
                rt.media,
                &rules,
                &self.state,
                &self.agent.cwd,
                &self.hooks,
            );
            let blocked = matches!(plan, scheduler::ToolAccessPlan::Exclusive)
                || plans
                    .iter()
                    .any(|previous| scheduler::access_plans_conflict(previous, &plan));
            if blocked {
                return;
            }
            plans.push(plan);
        }
        drop(rules);

        let call = self.streamed_calls.get(&index).expect("checked above");
        let (id, name, arguments) = (call.id.clone(), call.name.clone(), call.arguments.clone());
        let turn_id = turn_id.to_string();
        let cancel = control.cancel_token().clone();
        // Only a label for records the gate might write; nothing is surfaced
        // before the batch claims the call's real item id.
        let item_id = rt
            .live_tool_inputs
            .get(&index)
            .map(|(_, item_id)| item_id.clone())
            .unwrap_or_default();
        let gate = self.gate_snapshot();
        let mut tracker = self.tracker.foreground_child();
        let tx = tx.clone();
        let call = ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
        };
        let task = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let tool = tools
                .iter()
                .find(|candidate| candidate.name() == call.name)
                .map(|candidate| candidate.as_ref());
            let outcome = gate
                .exec_tool_gated(&call, tool, &item_id, &turn_id, &tx, &cancel, &mut tracker)
                .await;
            (outcome, started.elapsed().as_millis() as u64)
        });
        tracing::debug!(tool = %name, "early tool execution started");
        self.early_tools.push(EarlyTool {
            index,
            id,
            name,
            arguments,
            task: Some(task),
        });
    }

    /// The early run of exactly `call`, once. The caller joins it in place of
    /// executing the call; anything left over is dropped with the batch.
    pub(super) fn take_early_task(&mut self, call: &ToolCall) -> Option<EarlyTask> {
        let position = self
            .early_tools
            .iter()
            .position(|early| early.matches(call))?;
        self.early_tools.swap_remove(position).task.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn early(index: usize, call: &ToolCall) -> EarlyTool {
        EarlyTool {
            index,
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            task: None,
        }
    }

    #[test]
    fn an_early_run_belongs_to_one_exact_call() {
        let started = call("call_1", "Read", "{\"path\":\"a.rs\"}");
        let other = early(0, &started);
        assert!(other.matches(&started));
        // Same id, different tool or argument text: not the same call.
        assert!(!other.matches(&call("call_1", "Grep", "{\"path\":\"a.rs\"}")));
        assert!(!other.matches(&call("call_1", "Read", "{\"path\":\"b.rs\"}")));
        // Whitespace the provider normalized at commit: run it again, do not
        // reuse a result that may belong to different arguments.
        assert!(!other.matches(&call("call_1", "Read", "{ \"path\": \"a.rs\" }")));
    }

    #[test]
    fn only_side_effect_free_reads_start_early() {
        for tool in ["Grep", "Glob", "FetchURL"] {
            assert!(EARLY_TOOLS.contains(&tool), "{tool} should start early");
        }
        // `Read` records what it showed for the repeat-read short circuit: see
        // `EARLY_TOOLS`.
        for tool in [
            "Read",
            "Write",
            "Edit",
            tool::APPLY_PATCH_TOOL,
            "ExecCommand",
            "WriteStdin",
            "Agent",
            "TodoList",
            "AskUserQuestion",
            tool::CALL_TOOL,
            tool::TOOL_SEARCH_TOOL,
            "Skill",
        ] {
            assert!(!EARLY_TOOLS.contains(&tool), "{tool} must not start early");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_complete_json_object_is_what_makes_arguments_complete() {
        // Prefixes of an object never parse; the closed object does, whatever
        // surrounds it as whitespace.
        for prefix in [
            "",
            "{",
            "{\"path\"",
            "{\"path\":",
            "{\"path\": \"a",
            "{\"path\": \"a.rs\"",
            "{\"path\": \"a.rs\",",
            "{\"path\": \"a.rs\", \"limit\": 1",
        ] {
            assert!(
                serde_json::from_str::<serde_json::Value>(prefix).is_err(),
                "{prefix:?} is not complete"
            );
        }
        for complete in [
            "{}",
            "{\"path\": \"a.rs\"}",
            " {\"path\": \"a.rs\", \"limit\": 1} \n",
            "{\"q\": \"}\", \"n\": [1, {\"k\": null}]}",
        ] {
            assert!(
                serde_json::from_str::<serde_json::Value>(complete).is_ok(),
                "{complete:?} is complete"
            );
        }
    }
}
