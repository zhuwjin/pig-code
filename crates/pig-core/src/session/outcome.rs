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

/// How a turn ended (carried out of the turn future so the manager's
/// wrap-up can apply queue-drain policy)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnOutcome {
    Completed,
    Aborted,
    Failed,
}

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
