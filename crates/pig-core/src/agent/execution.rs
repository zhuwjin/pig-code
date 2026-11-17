use std::collections::BTreeSet;

use futures_util::future::BoxFuture;
use pig_protocol::Event;
use tokio_util::sync::CancellationToken;

use crate::session::{AgentRunResult, Session};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AgentExecutionCapability {
    Background,
    Steer,
    Resume,
    Reconnect,
    Stop,
    StructuredOutput,
    Approval,
    Media,
    Worktree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentRunIdentity {
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    pub(crate) run_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentEventCursor {
    pub(crate) run: AgentRunIdentity,
    pub(crate) sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentExecutionActivation {
    Start,
    Resume,
    Reconnect,
}

pub(crate) struct AgentExecutionRequest {
    pub(crate) identity: AgentRunIdentity,
    pub(crate) events: async_channel::Sender<Event>,
    pub(crate) cancel: CancellationToken,
    pub(crate) activation: AgentExecutionActivation,
    pub(crate) cursor: Option<AgentEventCursor>,
}

impl AgentExecutionRequest {
    pub(crate) fn new(
        identity: AgentRunIdentity,
        events: async_channel::Sender<Event>,
        cancel: CancellationToken,
        activation: AgentExecutionActivation,
    ) -> Self {
        Self {
            identity,
            events,
            cancel,
            activation,
            cursor: None,
        }
    }
}

/// A backend owns its execution context (a local Session or a remote
/// connection). The request carries only identity, activation, control and
/// output; a dropped future and its cancellation token share the same stop
/// boundary.
pub(crate) trait AgentExecutionBackend: Send {
    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability>;
    fn supports(&self, capability: AgentExecutionCapability) -> bool {
        self.capabilities().contains(&capability)
    }
    fn execute(
        self: Box<Self>,
        request: AgentExecutionRequest,
    ) -> BoxFuture<'static, AgentRunResult>;
}

pub(crate) struct LocalAgentExecutionBackend {
    session: Session,
}

impl LocalAgentExecutionBackend {
    pub(crate) fn new(session: Session) -> Self {
        Self { session }
    }

    pub(crate) fn capabilities() -> BTreeSet<AgentExecutionCapability> {
        [
            AgentExecutionCapability::Background,
            AgentExecutionCapability::Steer,
            AgentExecutionCapability::Resume,
            AgentExecutionCapability::Stop,
            AgentExecutionCapability::StructuredOutput,
            AgentExecutionCapability::Approval,
            AgentExecutionCapability::Media,
            AgentExecutionCapability::Worktree,
        ]
        .into_iter()
        .collect()
    }

    fn identity(&self) -> AgentRunIdentity {
        AgentRunIdentity {
            session_id: self.session.event_session_id().to_string(),
            agent_id: self.session.agent.id.clone(),
            run_id: self.session.agent.run_id().expect("child run identity"),
        }
    }
}

fn execution_failure(identity: AgentRunIdentity, reason: &str, detail: &str) -> AgentRunResult {
    AgentRunResult::child(
        pig_protocol::AgentRunStatus::Failed,
        reason.to_string(),
        detail.to_string(),
        0,
        (0, 0, 0),
    )
    .with_identity(&identity.agent_id, identity.run_id)
}

impl AgentExecutionBackend for LocalAgentExecutionBackend {
    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
        Self::capabilities()
    }

    fn execute(
        self: Box<Self>,
        request: AgentExecutionRequest,
    ) -> BoxFuture<'static, AgentRunResult> {
        let identity = self.identity();
        if request.identity != identity
            || request
                .cursor
                .as_ref()
                .is_some_and(|cursor| cursor.run != identity)
        {
            return Box::pin(async move {
                execution_failure(
                    identity,
                    "execution identity mismatch",
                    "The execution request does not match the selected Agent run",
                )
            });
        }
        if (request.activation == AgentExecutionActivation::Reconnect || request.cursor.is_some())
            && !self.supports(AgentExecutionCapability::Reconnect)
        {
            return Box::pin(async move {
                execution_failure(
                    identity,
                    "local execution cannot reconnect an event cursor",
                    "Local execution does not support reconnecting an external event cursor",
                )
            });
        }
        Box::pin(async move {
            let mut session = self.session;
            session
                .run_subagent_task(&request.events, &request.cancel)
                .await
        })
    }
}

pub(crate) fn execute_local(
    session: Session,
    events: async_channel::Sender<Event>,
    cancel: CancellationToken,
    activation: AgentExecutionActivation,
) -> BoxFuture<'static, AgentRunResult> {
    let backend = LocalAgentExecutionBackend::new(session);
    let request = AgentExecutionRequest::new(backend.identity(), events, cancel, activation);
    Box::new(backend).execute(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_backend_declares_the_current_agent_execution_surface() {
        let capabilities = LocalAgentExecutionBackend::capabilities();
        assert!(capabilities.contains(&AgentExecutionCapability::Background));
        assert!(capabilities.contains(&AgentExecutionCapability::Steer));
        assert!(capabilities.contains(&AgentExecutionCapability::Resume));
        assert!(capabilities.contains(&AgentExecutionCapability::Stop));
        assert!(capabilities.contains(&AgentExecutionCapability::StructuredOutput));
        assert!(capabilities.contains(&AgentExecutionCapability::Approval));
        assert!(!capabilities.contains(&AgentExecutionCapability::Reconnect));
        assert!(capabilities.contains(&AgentExecutionCapability::Media));
        assert!(capabilities.contains(&AgentExecutionCapability::Worktree));
    }
}
