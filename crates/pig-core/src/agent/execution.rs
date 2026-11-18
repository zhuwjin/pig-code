use std::collections::BTreeSet;

use futures_util::future::BoxFuture;
use pig_protocol::Event;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::session::{AgentRunResult, Session};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionCapability {
    Background,
    Steer,
    Resume,
    Reconnect,
    Stop,
    ForceStop,
    StructuredOutput,
    Approval,
    Media,
    Worktree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRunIdentity {
    pub session_id: String,
    pub agent_id: String,
    pub run_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEventCursor {
    pub run: AgentRunIdentity,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentExecutionActivation {
    Start,
    Resume,
    Reconnect,
}

pub struct AgentExecutionRequest {
    pub identity: AgentRunIdentity,
    pub events: async_channel::Sender<Event>,
    pub cancel: CancellationToken,
    pub activation: AgentExecutionActivation,
    pub cursor: Option<AgentEventCursor>,
    pub required_capabilities: BTreeSet<AgentExecutionCapability>,
}

impl AgentExecutionRequest {
    pub fn new(
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
            required_capabilities: BTreeSet::new(),
        }
    }
}

/// A backend owns its execution context (a local Session or a remote
/// connection). The request carries only identity, activation, control and
/// output; a dropped future and its cancellation token share the same stop
/// boundary.
pub trait AgentExecutionBackend: Send {
    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability>;
    fn supports(&self, capability: AgentExecutionCapability) -> bool {
        self.capabilities().contains(&capability)
    }
    fn execute(
        self: Box<Self>,
        request: AgentExecutionRequest,
    ) -> BoxFuture<'static, Result<super::AgentBackendResult, super::AgentBackendError>>;
}

pub(crate) struct LocalAgentBackendFactory;

impl super::AgentBackendFactory for LocalAgentBackendFactory {
    type Context = Session;

    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
        LocalAgentExecutionBackend::capabilities()
    }

    fn create(
        &self,
        context: Self::Context,
    ) -> BoxFuture<'static, Result<Box<dyn AgentExecutionBackend>, super::AgentBackendError>> {
        Box::pin(async move {
            Ok(Box::new(LocalAgentExecutionBackend::new(context))
                as Box<dyn AgentExecutionBackend>)
        })
    }
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
    ) -> BoxFuture<'static, Result<super::AgentBackendResult, super::AgentBackendError>> {
        let identity = self.identity();
        if request.identity != identity
            || request
                .cursor
                .as_ref()
                .is_some_and(|cursor| cursor.run != identity)
        {
            return Box::pin(async move {
                Err(super::AgentBackendError::Failed(format!(
                    "execution identity mismatch for {}:{}",
                    identity.agent_id, identity.run_id
                )))
            });
        }
        if (request.activation == AgentExecutionActivation::Reconnect || request.cursor.is_some())
            && !self.supports(AgentExecutionCapability::Reconnect)
        {
            return Box::pin(async move {
                Err(super::AgentBackendError::UnsupportedCapability {
                    backend_id: "local".to_string(),
                    capability: AgentExecutionCapability::Reconnect,
                })
            });
        }
        Box::pin(async move {
            let mut session = self.session;
            Ok(super::AgentBackendResult::from_run(
                session
                    .run_subagent_task(&request.events, &request.cancel)
                    .await,
            ))
        })
    }
}

pub(crate) fn execute_local(
    backends: std::sync::Arc<super::AgentBackendRegistry>,
    session: Session,
    events: async_channel::Sender<Event>,
    cancel: CancellationToken,
    activation: AgentExecutionActivation,
) -> BoxFuture<'static, AgentRunResult> {
    let identity = AgentRunIdentity {
        session_id: session.event_session_id().to_string(),
        agent_id: session.agent.id.clone(),
        run_id: session.agent.run_id().expect("child run identity"),
    };
    let request = AgentExecutionRequest::new(identity.clone(), events, cancel, activation);
    Box::pin(async move {
        match backends.execute_reserved("local", session, request).await {
            Ok(result) => result.into_run(&identity),
            Err(super::AgentBackendError::Cancelled) => AgentRunResult::child(
                pig_protocol::AgentRunStatus::Cancelled,
                "cancelled".to_string(),
                String::new(),
                0,
                (0, 0, 0),
            )
            .with_identity(&identity.agent_id, identity.run_id),
            Err(error) => execution_failure(
                identity,
                "local backend execution failed",
                &error.to_string(),
            ),
        }
    })
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
