use std::collections::BTreeSet;

use futures_util::future::BoxFuture;
use pig_protocol::Event;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{AgentBackendError, AgentExecutionHandle, AgentExecutionRelease, AgentExecutionUpdate};
use crate::session::{AgentRunResult, AgentRunSupervisor, Session};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionCapability {
    Background,
    Steer,
    Resume,
    Reconnect,
    Stop,
    ForceStop,
    Detach,
    StructuredOutput,
    Approval,
    Media,
    Worktree,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    Reconnect,
}

pub struct AgentExecutionRequest {
    pub identity: AgentRunIdentity,
    pub execution: super::AgentExecutionIdentity,
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
            execution: super::AgentExecutionIdentity {
                run: identity.clone(),
                attempt: 1,
                backend_epoch: super::runtime::new_runtime_id("epoch"),
            },
            identity,
            events,
            cancel,
            activation,
            cursor: None,
            required_capabilities: BTreeSet::new(),
        }
    }

    pub fn with_execution_identity(mut self, execution: super::AgentExecutionIdentity) -> Self {
        self.identity = execution.run.clone();
        self.execution = execution;
        self
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
    fn start(
        self: Box<Self>,
        request: AgentExecutionRequest,
    ) -> BoxFuture<'static, Result<Box<dyn super::AgentExecutionHandle>, super::AgentBackendError>>;
}

pub(crate) struct LocalAgentBackendFactory;

impl super::AgentBackendFactory for LocalAgentBackendFactory {
    type Context = (Session, AgentRunSupervisor);

    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
        LocalAgentExecutionBackend::capabilities()
    }

    fn create(
        &self,
        context: Self::Context,
    ) -> BoxFuture<'static, Result<Box<dyn AgentExecutionBackend>, super::AgentBackendError>> {
        Box::pin(async move {
            Ok(
                Box::new(LocalAgentExecutionBackend::with_supervisor(context))
                    as Box<dyn AgentExecutionBackend>,
            )
        })
    }
}

pub(crate) struct LocalAgentExecutionBackend {
    session: Session,
    supervisor: Option<AgentRunSupervisor>,
}

impl LocalAgentExecutionBackend {
    pub(crate) fn with_supervisor(
        context: <LocalAgentBackendFactory as super::AgentBackendFactory>::Context,
    ) -> Self {
        Self {
            session: context.0,
            supervisor: Some(context.1),
        }
    }

    pub(crate) fn capabilities() -> BTreeSet<AgentExecutionCapability> {
        [
            AgentExecutionCapability::Background,
            AgentExecutionCapability::Steer,
            AgentExecutionCapability::Resume,
            AgentExecutionCapability::Stop,
            AgentExecutionCapability::ForceStop,
            AgentExecutionCapability::StructuredOutput,
            AgentExecutionCapability::Approval,
            AgentExecutionCapability::Media,
            AgentExecutionCapability::Worktree,
        ]
        .into_iter()
        .collect()
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

    fn start(
        self: Box<Self>,
        request: AgentExecutionRequest,
    ) -> BoxFuture<'static, Result<Box<dyn super::AgentExecutionHandle>, super::AgentBackendError>>
    {
        Box::pin(async move {
            let mut this = self;
            let supervisor = this.supervisor.take().ok_or_else(|| {
                super::AgentBackendError::InvalidContext(
                    "Local execution requires its Supervisor".to_string(),
                )
            })?;
            let handle =
                crate::session::LocalAgentExecutionHandle::new(this.session, supervisor, request)?;
            Ok(Box::new(handle) as Box<dyn super::AgentExecutionHandle>)
        })
    }
}

enum LocalExecutionDriver {
    Direct(Box<dyn AgentExecutionHandle>),
    Runtime(super::runtime::AgentRuntime),
}

impl LocalExecutionDriver {
    async fn next(&mut self, run_id: &str) -> Result<AgentExecutionUpdate, AgentBackendError> {
        match self {
            Self::Direct(handle) => handle.next().await,
            Self::Runtime(runtime) => runtime
                .next_execution(run_id.to_string())
                .await
                .map_err(|error| AgentBackendError::Failed(error.to_string())),
        }
    }

    async fn release(&mut self, run_id: &str) -> Result<AgentExecutionRelease, AgentBackendError> {
        match self {
            Self::Direct(handle) => handle.release().await,
            Self::Runtime(runtime) => runtime
                .release_execution_handle(run_id.to_string())
                .await
                .map_err(|error| AgentBackendError::Failed(error.to_string())),
        }
    }
}

pub(crate) fn execute_local(
    backends: std::sync::Arc<super::AgentBackendRegistry>,
    session: Session,
    events: async_channel::Sender<Event>,
    cancel: CancellationToken,
    supervisor: AgentRunSupervisor,
) -> BoxFuture<'static, crate::session::ManagedOutcome> {
    let identity = AgentRunIdentity {
        session_id: session.event_session_id().to_string(),
        agent_id: session.agent.id.clone(),
        run_id: session.agent.run_id().expect("child run identity"),
    };
    let runtime = session.execution_runtime();
    let mut request = AgentExecutionRequest::new(
        identity.clone(),
        events,
        cancel,
        AgentExecutionActivation::Start,
    );
    let output = request.events.clone();
    Box::pin(async move {
        let failed = |result| crate::session::ManagedOutcome {
            result,
            queued: false,
        };
        if runtime.is_some()
            && !session.begin_agent_execution(&identity.agent_id, identity.run_id.clone())
        {
            return failed(execution_failure(
                identity,
                "local backend admission failed",
                "AgentRuntime rejected the current Local run",
            ));
        }
        if let Some(runtime) = &runtime {
            let execution = match runtime
                .current_execution_identity(identity.run_id.clone())
                .await
            {
                Ok(execution) => execution,
                Err(error) => {
                    return failed(execution_failure(
                        identity,
                        "local execution identity lookup failed",
                        &error.to_string(),
                    ));
                }
            };
            request = request.with_execution_identity(execution);
        }
        let handle = match backends
            .start_reserved("local", (session, supervisor), request)
            .await
        {
            Ok(handle) => handle,
            Err(super::AgentBackendError::Cancelled) => {
                return failed(
                    AgentRunResult::child(
                        pig_protocol::AgentRunStatus::Cancelled,
                        "cancelled".to_string(),
                        String::new(),
                        0,
                        (0, 0, 0),
                    )
                    .with_identity(&identity.agent_id, identity.run_id.clone()),
                );
            }
            Err(error) => {
                return failed(execution_failure(
                    identity,
                    "local backend execution failed",
                    &error.to_string(),
                ));
            }
        };
        let mut driver = if let Some(runtime) = runtime {
            match runtime.register_execution(handle).await {
                Ok(_) => LocalExecutionDriver::Runtime(runtime),
                Err(error) => {
                    return failed(execution_failure(
                        identity,
                        "local execution registration failed",
                        &error.to_string(),
                    ));
                }
            }
        } else {
            LocalExecutionDriver::Direct(handle)
        };
        loop {
            match driver.next(&identity.run_id).await {
                Ok(super::AgentExecutionUpdate::Output { event, .. }) => {
                    let _ = output.send(event).await;
                }
                Ok(super::AgentExecutionUpdate::Settled { result, .. }) => {
                    if let Err(error) = driver.release(&identity.run_id).await {
                        return failed(execution_failure(
                            identity,
                            "local backend release failed",
                            &error.to_string(),
                        ));
                    }
                    let queued = result.queued;
                    return crate::session::ManagedOutcome {
                        result: result.into_run(&identity),
                        queued,
                    };
                }
                Ok(super::AgentExecutionUpdate::Disconnected { detail, .. }) => {
                    return failed(execution_failure(
                        identity,
                        "local backend disconnected",
                        &detail,
                    ));
                }
                Err(error) => {
                    return failed(execution_failure(
                        identity,
                        "local backend execution failed",
                        &error.to_string(),
                    ));
                }
            }
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
