//! The shared agent context.
//!
//! Root sessions and subagents are the same kind of agent: a child is a full
//! `Session` whose `subagent` payload carries the parent conversation and its
//! persistence target. This module owns the identity/model/history/tool surface
//! shared by both.

use std::path::PathBuf;

use pig_provider::{ChatMsg, ResolvedModel};

use crate::tool;

/// Context shared by every agent instance.
pub(crate) struct Agent {
    pub(crate) id: String,
    /// Latest activation, including its stable identity and terminal result.
    pub(crate) run: Option<crate::session::AgentRunResult>,
    pub(crate) cwd: PathBuf,
    /// 0 for the root session; one more than the parent for every child.
    /// Resume keeps the persisted depth as a monotonic lower bound. The
    /// session's resolved `[agents]` config controls further delegation.
    pub(crate) delegation_depth: u32,
    /// Resolved model for this agent. Root sessions set it at each step
    /// (model/provider can switch); child agents set it at preparation.
    pub(crate) config: Option<ResolvedModel>,
    /// Opaque session-scoped provider transport (manages persistent Responses
    /// WebSocket v2 connections and continuation chains without leaking
    /// provider/format branches into `session/`).
    pub(crate) provider_session: pig_provider::ProviderSession,
    pub(crate) history: Vec<ChatMsg>,
    pub(crate) tools: Vec<Box<dyn tool::Tool>>,
    pub(crate) schemas: Vec<serde_json::Value>,
}

impl Agent {
    pub(crate) fn run_id(&self) -> Option<String> {
        self.run.as_ref().and_then(|run| run.run_id.clone())
    }

    pub(crate) fn config(&self) -> &ResolvedModel {
        self.config
            .as_ref()
            .expect("the agent model must be resolved before sampling")
    }
}
