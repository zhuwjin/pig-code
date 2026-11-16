//! Subagent orchestration.
//!
//! A child is a full `Session`: the shared engine drives its steps exactly
//! like a root turn. This module owns the lifecycle around that Session —
//! preparation, concurrency slots, context persistence, tool narrowing,
//! delegation depth and settlement.

use super::*;

mod compact;
mod control;
mod delegated;
mod lifecycle;
mod managed;
mod persist;
mod state;
mod swarm;
mod turn;

#[cfg(test)]
mod tests;

pub(crate) use control::DelegatedAgentStarted;
use lifecycle::ChildLifecycle;
use managed::{
    DelegatedUi, SubagentSlot, SwarmChildIdentity, aborted_swarm_child, drive_managed,
    spawn_detached_child, subagent_receipt, swarm_child_result,
};
pub(super) use state::SubagentState;
use state::{ProgressSink, SubagentMode, agent_card_model};
use turn::agent_result_path;

type SubagentResult = crate::session::AgentRunResult;
