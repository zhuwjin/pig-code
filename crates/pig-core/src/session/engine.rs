//! Shared agent engine.
//!
//! `run_step` owns the per-step sequence (preflight -> provider attempt(s) ->
//! assistant commit -> tool batch); `stream_attempt` owns one provider
//! request. This is the local analog of the reference agents' shared loop
//! layer (Codex's thread-owned turn core, Kimi Code's `IAgentLoopService`,
//! DeepSeek Harness's `dsh-agent` loop): one implementation owns sequencing,
//! streaming, accumulation and usage semantics, while the orchestration layer
//! supplies policy. The orchestration layers keep what differs: compaction
//! strategy, persistence (root rollout vs child JSONL), tool policy, step
//! limits and lifecycle.

use std::collections::HashMap;
use std::time::Instant;

use pig_protocol::{CoreError, Event, FinishReason};
use pig_provider::{AssistantReplay, ProviderEvent, RetryNotice, ToolCall};
use tokio_util::sync::CancellationToken;

use crate::api_usage::ApiCallTracker;

use super::Session;

/// What one step hands back to the outer turn loop.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StepOutcome {
    /// Tool calls ran; the caller may run another step.
    Continue,
    TextOnly {
        finish_reason: Option<FinishReason>,
    },
    Paused,
    Cancelled,
    Failed,
}

/// The policy decision after one streamed attempt.
pub(crate) enum AttemptAction {
    Retry,
    Continue,
    Terminal(StepOutcome),
}

/// The policy decision after committing a successful attempt.
pub(crate) enum CommitAction {
    TextOnly { finish_reason: Option<FinishReason> },
    Paused,
    Tools(Vec<ToolCall>),
    Terminal(StepOutcome),
}

/// Which agent kind this step runs for. Root turns carry their model and call
/// control per run (the model can switch between turns); a child agent resolves
/// both once at preparation and keeps them in `Session::agent`.
pub(crate) enum StepMode {
    Root {
        turn_id: String,
        config: pig_provider::ResolvedModel,
        control: pig_provider::CallControl,
        preserve_latest: bool,
        pause_pin: Option<usize>,
    },
    Child,
}

/// A child step failure that the child loop must report differently from a
/// generic model-request failure.
pub(crate) enum StepFailure {
    Overflow(CoreError),
    Message(String),
}

/// Per-step ephemeral state. The engine owns the sequence; Session methods own
/// the policy and read/write this shared runtime.
pub(crate) struct StepRuntime {
    pub(crate) mode: StepMode,
    pub(crate) tools_tokens: u64,
    pub(crate) media: crate::tool::MediaCapabilities,
    pub(crate) schemas: Vec<serde_json::Value>,
    pub(crate) text_item: String,
    pub(crate) reasoning_item: String,
    /// Provider streaming index -> (provider call id, internal item id).
    pub(crate) live_tool_inputs: HashMap<usize, (String, String)>,
    pub(crate) step_item_ids: Vec<String>,
    /// Step-scoped model clock; set once after the preflight compaction.
    pub(crate) step_started: Option<Instant>,
    pub(crate) step_first_token_at: Option<Instant>,
    /// One reactive overflow recovery per step.
    pub(crate) recovery_attempted: bool,
    pub(crate) paused: bool,
    /// Child only: history index of the assistant row this step streams into.
    pub(crate) assistant_row: usize,
    /// Child only: ownership prefix for child tool item/approval ids.
    pub(crate) ownership: String,
    /// Child only: live rows currently shown for this step.
    pub(crate) live_items: Vec<String>,
    pub(crate) failure: Option<StepFailure>,
    pub(crate) last_text: String,
    pub(crate) incomplete_reason: Option<FinishReason>,
    /// Child only: reasoning duration of the committed assistant row, applied
    /// to the final projection after a tool batch.
    pub(crate) reasoning_ms: Option<u64>,
}

impl StepRuntime {
    pub(crate) fn root(
        turn_id: String,
        config: pig_provider::ResolvedModel,
        control: pig_provider::CallControl,
        preserve_latest: bool,
        pause_pin: Option<usize>,
    ) -> Self {
        Self {
            mode: StepMode::Root {
                turn_id,
                config,
                control,
                preserve_latest,
                pause_pin,
            },
            ..Self::empty(StepMode::Child)
        }
    }

    pub(crate) fn child() -> Self {
        Self::empty(StepMode::Child)
    }

    fn empty(mode: StepMode) -> Self {
        Self {
            mode,
            tools_tokens: 0,
            media: crate::tool::MediaCapabilities::default(),
            schemas: Vec::new(),
            text_item: String::new(),
            reasoning_item: String::new(),
            live_tool_inputs: HashMap::new(),
            step_item_ids: Vec::new(),
            step_started: None,
            step_first_token_at: None,
            recovery_attempted: false,
            paused: false,
            assistant_row: 0,
            ownership: String::new(),
            live_items: Vec::new(),
            failure: None,
            last_text: String::new(),
            incomplete_reason: None,
            reasoning_ms: None,
        }
    }

    pub(crate) fn root_parts(
        &self,
    ) -> (
        &str,
        &pig_provider::ResolvedModel,
        &pig_provider::CallControl,
        bool,
        Option<usize>,
    ) {
        match &self.mode {
            StepMode::Root {
                turn_id,
                config,
                control,
                preserve_latest,
                pause_pin,
            } => (turn_id, config, control, *preserve_latest, *pause_pin),
            StepMode::Child => panic!("child runtime has no root turn parts"),
        }
    }
}

/// One shared agent step: preflight, provider attempt(s), assistant commit and
/// tool execution. Root turns and child agents are the same `Session` type; the
/// Session methods below branch on `Session::subagent` for the policy that
/// differs.
pub(crate) async fn run_step(
    session: &mut Session,
    rt: &mut StepRuntime,
    step: usize,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> StepOutcome {
    session.prepare_step(rt, step, tx, cancel).await;
    'attempt: loop {
        if session.preflight_step(rt, step, tx, cancel).await {
            return StepOutcome::Cancelled;
        }
        let api_started = Instant::now();
        let api_started_ms = crate::model_io::now_ms();
        let (mut rx, task) = session.begin_step_attempt(rt, step, tx, cancel).await;
        let mut api_tracker = ApiCallTracker::default();
        let mut attempt = {
            let mut sink = SessionAttemptSink {
                session,
                runtime: rt,
                tx,
                reasoning_started_ms: None,
                text_started_ms: None,
            };
            stream_attempt(&mut rx, cancel, &mut api_tracker, &mut sink).await
        };
        match session
            .record_step_attempt(
                rt,
                &mut attempt,
                api_tracker,
                api_started,
                api_started_ms,
                step,
                tx,
                cancel,
            )
            .await
        {
            AttemptAction::Retry => {
                task.abort();
                continue 'attempt;
            }
            AttemptAction::Terminal(outcome) => {
                task.abort();
                return outcome;
            }
            AttemptAction::Continue => {
                let _ = task.await;
            }
        }
        // Some Responses streams deliver the assistant message only as the
        // authoritative replay item; the durable replay carries the text.
        if attempt.text.is_empty()
            && let Some(replay) = attempt.replay.as_ref()
        {
            attempt.text = replay
                .items
                .iter()
                .filter_map(|item| match item {
                    pig_provider::AssistantReplayItem::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
        }
        match session.commit_step_assistant(rt, attempt, step, tx).await {
            CommitAction::TextOnly { finish_reason } => {
                return StepOutcome::TextOnly { finish_reason };
            }
            CommitAction::Paused => return StepOutcome::Paused,
            CommitAction::Tools(calls) => {
                // Boxed to break the type-level cycle: a root tool batch can
                // spawn a child, whose own loop re-enters this engine.
                return Box::pin(session.execute_step_tools(rt, calls, step, tx, cancel)).await;
            }
            CommitAction::Terminal(outcome) => return outcome,
        }
    }
}

/// Live forwarding for one attempt. Root and child branches keep their own
/// event vocabulary while the engine owns accumulation and timing.
struct SessionAttemptSink<'a> {
    session: &'a mut Session,
    runtime: &'a mut StepRuntime,
    tx: &'a async_channel::Sender<Event>,
    /// Wall-clock start of the current streamed row; the UI's live duration
    /// label is derived from it.
    reasoning_started_ms: Option<u64>,
    text_started_ms: Option<u64>,
}

impl AttemptSink for SessionAttemptSink<'_> {
    fn reasoning(&mut self, delta: &str, full: &str) {
        if self.session.is_subagent() {
            self.session.emit_child_live(
                self.runtime,
                self.tx,
                pig_protocol::ConversationItem::Reasoning {
                    id: format!("r{}", self.runtime.assistant_row),
                    text: full.to_string(),
                    streaming: true,
                    turn_id: None,
                    started_at_ms: Some(
                        *self
                            .reasoning_started_ms
                            .get_or_insert_with(crate::model_io::now_ms),
                    ),
                    duration_ms: None,
                    sources: Vec::new(),
                },
            );
            return;
        }
        self.session.record_output_delta(delta);
        let item_id = self.runtime.reasoning_item.clone();
        let delta = delta.to_string();
        self.session.emit(
            move |session_id| Event::ReasoningDelta {
                session_id,
                item_id,
                delta,
            },
            self.tx,
        );
    }

    fn text(&mut self, delta: &str, full: &str) {
        if self.session.is_subagent() {
            self.session.emit_child_live(
                self.runtime,
                self.tx,
                pig_protocol::ConversationItem::Markdown {
                    id: format!("a{}", self.runtime.assistant_row),
                    text: full.to_string(),
                    streaming: true,
                    turn_id: None,
                    started_at_ms: Some(
                        *self
                            .text_started_ms
                            .get_or_insert_with(crate::model_io::now_ms),
                    ),
                    duration_ms: None,
                    sources: Vec::new(),
                },
            );
            return;
        }
        self.session.record_output_delta(delta);
        let item_id = self.runtime.text_item.clone();
        let delta = delta.to_string();
        self.session.emit(
            move |session_id| Event::TextDelta {
                session_id,
                item_id,
                delta,
            },
            self.tx,
        );
    }

    fn tool_start(&mut self, index: usize, id: &str, name: &str) {
        if self.session.is_subagent() {
            self.session.emit_child_live(
                self.runtime,
                self.tx,
                pig_protocol::ConversationItem::Tool {
                    id: format!("t{}-{index}", self.runtime.assistant_row),
                    tool: name.to_string(),
                    summary: String::new(),
                    detail: String::new(),
                    output: String::new(),
                    status: pig_protocol::ConversationToolStatus::Input,
                    turn_id: None,
                    edit: None,
                    live_note: None,
                    agents: Vec::new(),
                    approval_pending: false,
                },
            );
            return;
        }
        let item_id = self
            .runtime
            .live_tool_inputs
            .get(&index)
            .map(|(_, item_id)| item_id.clone())
            .unwrap_or_else(|| {
                let turn_id = self.runtime.root_parts().0.to_string();
                self.session.next_tool_item_id(&turn_id, id)
            });
        let tool = name.to_string();
        let event_item_id = item_id.clone();
        self.session.emit(
            move |session_id| Event::ToolInputStart {
                session_id,
                item_id: event_item_id,
                tool,
            },
            self.tx,
        );
        self.runtime
            .live_tool_inputs
            .insert(index, (id.to_string(), item_id));
    }

    fn tool_delta(&mut self, index: usize, _id: &str, name: &str, arguments: &str, delta: &str) {
        if self.session.is_subagent() {
            self.session.emit_child_live(
                self.runtime,
                self.tx,
                pig_protocol::ConversationItem::Tool {
                    id: format!("t{}-{index}", self.runtime.assistant_row),
                    tool: name.to_string(),
                    summary: String::new(),
                    detail: arguments.to_string(),
                    output: String::new(),
                    status: pig_protocol::ConversationToolStatus::Input,
                    turn_id: None,
                    edit: None,
                    live_note: None,
                    agents: Vec::new(),
                    approval_pending: false,
                },
            );
            return;
        }
        if let Some((_, item_id)) = self.runtime.live_tool_inputs.get(&index) {
            let item_id = item_id.clone();
            let delta = delta.to_string();
            self.session.emit(
                move |session_id| Event::ToolInputDelta {
                    session_id,
                    item_id,
                    delta,
                },
                self.tx,
            );
        }
    }

    fn retry(&mut self, notice: &RetryNotice) {
        if self.session.is_subagent() {
            return;
        }
        let status = super::model::retry_status(notice);
        self.session.emit(
            move |session_id| Event::RetryStatus { session_id, status },
            self.tx,
        );
    }
}

/// One usage report from the provider for this request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProviderUsage {
    pub input: u64,
    pub cache_read: u64,
    pub output: u64,
    pub used: u64,
    pub total: u64,
    pub reasoning_output: u64,
}

/// Live forwarding for one attempt. Implementations own their event/topic
/// naming; the engine owns accumulation and timing.
pub(crate) trait AttemptSink {
    fn reasoning(&mut self, delta: &str, full: &str);
    fn text(&mut self, delta: &str, full: &str);
    fn tool_start(&mut self, index: usize, id: &str, name: &str);
    /// `arguments` is the full accumulated raw JSON of the call.
    fn tool_delta(&mut self, index: usize, id: &str, name: &str, arguments: &str, delta: &str);
    fn retry(&mut self, notice: &RetryNotice);
}

/// Everything the host needs after the stream ends.
pub(crate) struct AttemptOutcome {
    pub text: String,
    pub reasoning: String,
    pub replay: Option<AssistantReplay>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub failure: Option<CoreError>,
    /// Any model-authored content reached the UI/history; a transparent retry
    /// is only safe when this is false.
    pub forwarded_content: bool,
    pub first_token_at: Option<Instant>,
    /// Streamed reasoning duration, closed when text or tool calls begin.
    pub reasoning_ms: Option<u64>,
    pub usage: Option<ProviderUsage>,
}

/// Drain one provider request. The caller owns spawning/aborting
/// `stream_chat`; this engine owns the event semantics.
pub(crate) async fn stream_attempt<S: AttemptSink + ?Sized>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ProviderEvent>,
    cancel: &CancellationToken,
    api_tracker: &mut ApiCallTracker,
    sink: &mut S,
) -> AttemptOutcome {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut replay = None;
    let mut tool_calls = Vec::new();
    let mut finish_reason = FinishReason::Stop;
    let mut failure = None;
    let mut forwarded_content = false;
    let mut first_token_at: Option<Instant> = None;
    let mut reasoning_started: Option<Instant> = None;
    let mut reasoning_ms = None;
    let mut usage = None;
    // Tool-call name/arguments per streaming index, used to give the sink the
    // full accumulated arguments on every delta.
    let mut live_tools: HashMap<usize, (String, String, String)> = HashMap::new();

    loop {
        let event = tokio::select! {
            event = rx.recv() => event,
            _ = cancel.cancelled() => None,
        };
        let Some(event) = event else {
            if !cancel.is_cancelled() {
                failure = Some(CoreError::Internal {
                    detail: "Provider event stream ended without a finish reason".to_string(),
                });
            }
            break;
        };
        api_tracker.observe(&event);
        match event {
            ProviderEvent::Reasoning(delta) => {
                let at = Instant::now();
                first_token_at.get_or_insert(at);
                reasoning_started.get_or_insert(at);
                forwarded_content = true;
                reasoning.push_str(&delta);
                sink.reasoning(&delta, &reasoning);
            }
            ProviderEvent::Text(delta) => {
                let at = Instant::now();
                first_token_at.get_or_insert(at);
                if let Some(started) = reasoning_started.take() {
                    reasoning_ms = Some(started.elapsed().as_millis() as u64);
                }
                forwarded_content = true;
                text.push_str(&delta);
                sink.text(&delta, &text);
            }
            ProviderEvent::AssistantReplay(snapshot) => {
                if !snapshot.items.is_empty() {
                    forwarded_content = true;
                }
                replay = Some(snapshot);
            }
            ProviderEvent::ToolCalls(calls) => {
                if let Some(started) = reasoning_started.take() {
                    reasoning_ms = Some(started.elapsed().as_millis() as u64);
                }
                forwarded_content |= !calls.is_empty();
                tool_calls = calls;
            }
            ProviderEvent::ToolCallStart { index, id, name } => {
                let at = Instant::now();
                first_token_at.get_or_insert(at);
                if let Some(started) = reasoning_started.take() {
                    reasoning_ms = Some(started.elapsed().as_millis() as u64);
                }
                forwarded_content = true;
                live_tools.insert(index, (id.clone(), name.clone(), String::new()));
                sink.tool_start(index, &id, &name);
            }
            ProviderEvent::ToolCallDelta { index, delta } => {
                if !delta.is_empty() {
                    let at = Instant::now();
                    first_token_at.get_or_insert(at);
                }
                if let Some((id, name, arguments)) = live_tools.get_mut(&index) {
                    arguments.push_str(&delta);
                    sink.tool_delta(index, id, name, arguments, &delta);
                }
            }
            ProviderEvent::Usage {
                input,
                cache_read,
                output,
                used,
                total,
                reasoning_output,
            } => {
                usage = Some(ProviderUsage {
                    input,
                    cache_read,
                    output,
                    used,
                    total,
                    reasoning_output,
                });
            }
            ProviderEvent::ResponseMetadata(_) => {}
            ProviderEvent::Retrying(notice) => sink.retry(&notice),
            ProviderEvent::Finished(reason) => {
                finish_reason = reason;
                break;
            }
            ProviderEvent::Failed(error) => {
                failure = Some(error);
                break;
            }
        }
    }

    AttemptOutcome {
        text,
        reasoning,
        replay,
        tool_calls,
        finish_reason,
        failure,
        forwarded_content,
        first_token_at,
        reasoning_ms,
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::FinishReason;
    use pig_provider::{RetryReason, ToolCall};
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingSink {
        reasoning: Vec<(String, String)>,
        text: Vec<(String, String)>,
        starts: Vec<(usize, String, String)>,
        deltas: Vec<(usize, String, String, String)>,
        retries: usize,
    }

    impl AttemptSink for RecordingSink {
        fn reasoning(&mut self, delta: &str, full: &str) {
            self.reasoning.push((delta.to_string(), full.to_string()));
        }
        fn text(&mut self, delta: &str, full: &str) {
            self.text.push((delta.to_string(), full.to_string()));
        }
        fn tool_start(&mut self, index: usize, id: &str, name: &str) {
            self.starts.push((index, id.to_string(), name.to_string()));
        }
        fn tool_delta(&mut self, index: usize, id: &str, name: &str, arguments: &str, delta: &str) {
            self.deltas.push((
                index,
                format!("{id}/{name}"),
                arguments.to_string(),
                delta.to_string(),
            ));
        }
        fn retry(&mut self, _notice: &RetryNotice) {
            self.retries += 1;
        }
    }

    #[tokio::test]
    async fn one_attempt_accumulates_and_forwards_the_shared_stream() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let call = ToolCall {
            id: "call-1".into(),
            name: "Read".into(),
            arguments: "{\"path\":\"a\"}".into(),
        };
        for event in [
            ProviderEvent::Reasoning("think".into()),
            ProviderEvent::Text("answer".into()),
            ProviderEvent::ToolCallStart {
                index: 0,
                id: "call-1".into(),
                name: "Read".into(),
            },
            ProviderEvent::ToolCallDelta {
                index: 0,
                delta: "{\"path\"".into(),
            },
            ProviderEvent::Retrying(RetryNotice {
                attempt: 1,
                max_attempts: 3,
                delay: Duration::from_millis(10),
                reason: RetryReason::Server(500),
            }),
            ProviderEvent::Usage {
                input: 10,
                cache_read: 20,
                output: 30,
                used: 60,
                total: 1000,
                reasoning_output: 5,
            },
            ProviderEvent::ToolCalls(vec![call.clone()]),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ] {
            tx.send(event).expect("send");
        }
        drop(tx);

        let mut tracker = ApiCallTracker::default();
        let mut sink = RecordingSink::default();
        let outcome =
            stream_attempt(&mut rx, &CancellationToken::new(), &mut tracker, &mut sink).await;

        assert_eq!(outcome.text, "answer");
        assert_eq!(outcome.reasoning, "think");
        assert_eq!(outcome.tool_calls.len(), 1);
        assert_eq!(outcome.tool_calls[0].id, "call-1");
        assert_eq!(outcome.finish_reason, FinishReason::ToolCalls);
        assert!(outcome.failure.is_none());
        assert!(outcome.forwarded_content);
        assert!(outcome.first_token_at.is_some());
        assert!(outcome.reasoning_ms.is_some());
        let usage = outcome.usage.expect("usage");
        assert_eq!(
            (usage.input, usage.cache_read, usage.output, usage.used),
            (10, 20, 30, 60)
        );

        assert_eq!(
            sink.reasoning,
            vec![("think".to_string(), "think".to_string())]
        );
        assert_eq!(
            sink.text,
            vec![("answer".to_string(), "answer".to_string())]
        );
        assert_eq!(
            sink.starts,
            vec![(0, "call-1".to_string(), "Read".to_string())]
        );
        assert_eq!(sink.deltas.len(), 1);
        assert_eq!(sink.deltas[0].0, 0);
        assert_eq!(sink.deltas[0].1, "call-1/Read");
        assert_eq!(sink.deltas[0].2, "{\"path\"");
        assert_eq!(sink.deltas[0].3, "{\"path\"");
        assert_eq!(sink.retries, 1);
    }
}
