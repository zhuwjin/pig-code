use super::*;

pub(super) enum StepOutcome {
    TextOnly {
        finish_reason: Option<pig_protocol::FinishReason>,
    },
    /// Anthropic `pause_turn`: the assistant step is durable (including any
    /// executed local tools), but the turn continues with the same history.
    Paused,
    ToolsExecuted,
    /// The turn cannot continue: `aborted` distinguishes a user/internal
    /// cancel (TurnAborted emitted) from a provider failure
    Ended {
        aborted: bool,
    },
}

/// Cap on automatic continuations of an Anthropic `pause_turn` within one
/// turn (per run for subagents). The provider may legitimately pause several
/// times while a hosted server tool runs, but an unbounded loop is a failure.
pub(crate) const MAX_PAUSE_CONTINUATIONS: usize = 3;

pub(crate) fn pause_limit_detail() -> String {
    format!(
        "Pause continuation limit reached: the provider paused the turn more than {MAX_PAUSE_CONTINUATIONS} times without finishing; stopping instead of looping"
    )
}

/// English typed failure for an exhausted pause budget; surfaced as
/// `CoreError::Internal` so app's `CoreError` mapping needs no new variant.
pub(crate) fn pause_limit_error() -> CoreError {
    CoreError::Internal {
        detail: pause_limit_detail(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRunStatus {
    Queued,
    Running,
    Completed,
    Cancelled,
    Failed,
    Lost,
}

impl AgentRunStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Lost => "lost",
        }
    }
}

/// Text/result artifact captured by one agent run. Root turns project their
/// response through existing turn events and do not need a second artifact.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentArtifact {
    pub(crate) text: String,
    pub(crate) path: Option<std::path::PathBuf>,
}

/// One root or child run's authoritative outcome. Presentation strings and
/// registry states are derived from `status` rather than independent booleans.
#[derive(Clone, Debug)]
pub(crate) struct AgentRunResult {
    pub(crate) agent_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) turn_kind: pig_protocol::TurnKind,
    pub(crate) status: AgentRunStatus,
    pub(crate) stop_reason: String,
    pub(crate) turns: usize,
    pub(crate) usage: (u64, u64, u64),
    pub(crate) artifact: Option<AgentArtifact>,
}

impl AgentRunResult {
    pub(crate) fn root(status: AgentRunStatus, turn_kind: pig_protocol::TurnKind) -> Self {
        Self {
            agent_id: None,
            run_id: None,
            turn_kind,
            status,
            stop_reason: status.as_str().to_string(),
            turns: 0,
            usage: (0, 0, 0),
            artifact: None,
        }
    }

    pub(crate) fn child(
        status: AgentRunStatus,
        stop_reason: String,
        text: String,
        turns: usize,
        usage: (u64, u64, u64),
    ) -> Self {
        Self {
            agent_id: None,
            run_id: None,
            turn_kind: pig_protocol::TurnKind::Regular,
            status,
            stop_reason,
            turns,
            usage,
            artifact: Some(AgentArtifact { text, path: None }),
        }
    }

    pub(crate) fn with_identity(mut self, agent_id: &str, run_id: String) -> Self {
        self.agent_id = Some(agent_id.to_string());
        self.run_id = Some(run_id);
        self
    }

    pub(crate) fn is_completed(&self) -> bool {
        self.status == AgentRunStatus::Completed
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.status == AgentRunStatus::Cancelled
    }

    pub(crate) fn is_error(&self) -> bool {
        self.status == AgentRunStatus::Failed
    }

    pub(crate) fn text(&self) -> &str {
        self.artifact
            .as_ref()
            .map_or("", |artifact| artifact.text.as_str())
    }

    pub(crate) fn status_line(&self) -> String {
        match self.status {
            AgentRunStatus::Queued | AgentRunStatus::Running | AgentRunStatus::Lost => {
                self.status.as_str().to_string()
            }
            AgentRunStatus::Cancelled => "cancelled".to_string(),
            AgentRunStatus::Failed => format!("failed: {}", self.stop_reason),
            AgentRunStatus::Completed if self.stop_reason == "completed" => {
                format!("completed ({} steps)", self.turns)
            }
            AgentRunStatus::Completed if self.stop_reason.is_empty() => "completed".to_string(),
            AgentRunStatus::Completed => format!("completed {}", self.stop_reason),
        }
    }
}

pub(crate) fn new_agent_run_id() -> String {
    static NEXT_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = NEXT_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("run-{timestamp}-{sequence}")
}

impl Session {
    pub(crate) fn begin_root_run(&mut self, turn_kind: pig_protocol::TurnKind) {
        self.run_turns = 0;
        let agent_id = self.agent.id.clone();
        self.agent.run = Some(
            AgentRunResult::root(AgentRunStatus::Running, turn_kind)
                .with_identity(&agent_id, new_agent_run_id()),
        );
    }

    pub(crate) fn run_envelope(&self) -> Option<pig_protocol::AgentRunEnvelope> {
        let run = self.agent.run.as_ref()?;
        Some(pig_protocol::AgentRunEnvelope {
            agent_id: self.agent.id.clone(),
            run_id: run.run_id.clone()?,
            turn_kind: run.turn_kind,
        })
    }

    pub(crate) fn emit_turn_aborted(&mut self, tx: &async_channel::Sender<Event>) {
        let run = self.run_envelope();
        self.emit(|session_id| Event::TurnAborted { session_id, run }, tx);
    }

    pub(crate) fn root_result(&mut self, status: AgentRunStatus) -> AgentRunResult {
        if !matches!(
            self.agent.run.as_ref().map(|run| run.status),
            Some(AgentRunStatus::Running)
        ) {
            self.begin_root_run(pig_protocol::TurnKind::Regular);
        }
        let agent_id = self.agent.id.clone();
        let (run_id, turn_kind) = self
            .agent
            .run
            .as_ref()
            .and_then(|run| run.run_id.clone().map(|run_id| (run_id, run.turn_kind)))
            .expect("root run identity");
        let mut result = AgentRunResult::root(status, turn_kind).with_identity(&agent_id, run_id);
        result.turns = self.run_turns;
        result.usage = (self.turn_input, self.turn_cache_read, self.turn_output);
        self.agent.run = Some(result.clone());
        result
    }
}

pub(crate) type TurnOutcome = AgentRunResult;

/// Result of gated tool execution (exec_tool_gated)
pub(crate) enum GatedToolOutcome {
    /// Executed; the caller owns history/rollout/ToolCallEnd (parent and
    /// child each write their own)
    Executed {
        output: String,
        is_error: bool,
        /// Diff of this edit (for inline rendering in the parent session's
        /// tool card; ignored on the subagent path)
        edit: Option<pig_protocol::EditDiff>,
        images: Vec<pig_provider::ChatImage>,
        documents: Vec<pig_provider::ChatDocument>,
        delivered_notification_ids: Vec<String>,
    },
    /// Rejected by approval/rules (note is the message for the model); the
    /// caller owns history/rollout/ToolCallEnd
    Rejected { note: String },
    /// Cancelled (interrupted while awaiting approval or executing); the
    /// caller decides TurnAborted and the wrap-up
    Cancelled,
}

/// Result of subagent delegation (run_subagent/run_swarm)
pub(crate) enum SubagentOutcome {
    /// The subagent has finished (success or failure is in the note; a
    /// background dispatch yields an immediate receipt); the caller owns
    /// history/rollout/ToolCallEnd. card = single agent card metadata (the
    /// Agent path; persisted with the rollout ToolCall record, replay
    /// rebuilds the agent card); cards = batch agent cards (the AgentSwarm
    /// path, one per subagent). Early exits from argument/profile/model
    /// resolution failure have no agent_id, leaving both empty
    Finished {
        note: String,
        is_error: bool,
        card: Option<crate::rollout::AgentCardRecord>,
        cards: Vec<crate::rollout::AgentCardRecord>,
    },
    /// Cancelled (interrupted while awaiting approval or sampling/executing);
    /// the caller emits TurnAborted and wraps up. card/cards = agent card
    /// metadata (cancellation is likewise persisted with the rollout; replay
    /// rebuilds the agent cards)
    Cancelled {
        card: Option<crate::rollout::AgentCardRecord>,
        cards: Vec<crate::rollout::AgentCardRecord>,
    },
}
