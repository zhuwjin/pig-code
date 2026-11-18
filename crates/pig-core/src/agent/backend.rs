use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use futures_util::future::BoxFuture;

use super::{
    AgentExecutionBackend, AgentExecutionCapability, AgentExecutionHandle, AgentExecutionInput,
    AgentExecutionReceipt, AgentExecutionRelease, AgentExecutionRequest, AgentExecutionStop,
    AgentExecutionUpdate,
};
use crate::session::AgentRunResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentBackendResult {
    pub status: pig_protocol::AgentRunStatus,
    pub stop_reason: String,
    pub text: String,
    pub turns: usize,
    pub usage: (u64, u64, u64),
}

impl AgentBackendResult {
    pub(crate) fn from_run(result: AgentRunResult) -> Self {
        let text = result.text().to_string();
        Self {
            status: result.status,
            stop_reason: result.stop_reason,
            text,
            turns: result.turns,
            usage: result.usage,
        }
    }

    pub(crate) fn into_run(self, identity: &super::AgentRunIdentity) -> AgentRunResult {
        AgentRunResult::child(
            self.status,
            self.stop_reason,
            self.text,
            self.turns,
            self.usage,
        )
        .with_identity(&identity.agent_id, identity.run_id.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentBackendError {
    NotRegistered(String),
    Duplicate(String),
    UnsupportedCapability {
        backend_id: String,
        capability: AgentExecutionCapability,
    },
    InvalidContext(String),
    Cancelled,
    Disconnected(String),
    Failed(String),
}

impl std::fmt::Display for AgentBackendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRegistered(id) => write!(formatter, "Agent backend {id} is not registered"),
            Self::Duplicate(id) => write!(formatter, "Agent backend {id} is already registered"),
            Self::UnsupportedCapability {
                backend_id,
                capability,
            } => write!(
                formatter,
                "Agent backend {backend_id} does not support required capability {capability:?}"
            ),
            Self::InvalidContext(detail) | Self::Disconnected(detail) | Self::Failed(detail) => {
                formatter.write_str(detail)
            }
            Self::Cancelled => formatter.write_str("Agent backend execution was cancelled"),
        }
    }
}

impl std::error::Error for AgentBackendError {}

pub trait AgentBackendFactory: Send + Sync + 'static {
    type Context: Send + 'static;

    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability>;
    fn create(
        &self,
        context: Self::Context,
    ) -> BoxFuture<'static, Result<Box<dyn AgentExecutionBackend>, AgentBackendError>>;
}

trait ErasedAgentBackendFactory: Send + Sync {
    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability>;
    fn create(
        &self,
        context: Box<dyn Any + Send>,
    ) -> BoxFuture<'static, Result<Box<dyn AgentExecutionBackend>, AgentBackendError>>;
}

struct TypedAgentBackendFactory<F>(F);

impl<F: AgentBackendFactory> ErasedAgentBackendFactory for TypedAgentBackendFactory<F> {
    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
        self.0.capabilities()
    }

    fn create(
        &self,
        context: Box<dyn Any + Send>,
    ) -> BoxFuture<'static, Result<Box<dyn AgentExecutionBackend>, AgentBackendError>> {
        let context = match context.downcast::<F::Context>() {
            Ok(context) => *context,
            Err(_) => {
                return Box::pin(async move {
                    Err(AgentBackendError::InvalidContext(format!(
                        "Agent backend expected context {}",
                        std::any::type_name::<F::Context>()
                    )))
                });
            }
        };
        self.0.create(context)
    }
}

struct ReservedAgentExecutionHandle {
    inner: Box<dyn AgentExecutionHandle>,
    permit: Option<super::AgentRunPermit>,
}

impl AgentExecutionHandle for ReservedAgentExecutionHandle {
    fn identity(&self) -> &super::AgentExecutionIdentity {
        self.inner.identity()
    }

    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
        self.inner.capabilities()
    }

    fn send(
        &mut self,
        input: AgentExecutionInput,
    ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
        self.inner.send(input)
    }

    fn stop(
        &mut self,
        stop: AgentExecutionStop,
    ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
        self.inner.stop(stop)
    }

    fn detach(&mut self) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
        self.inner.detach()
    }

    fn next(&mut self) -> BoxFuture<'_, Result<AgentExecutionUpdate, AgentBackendError>> {
        self.inner.next()
    }

    fn release(&mut self) -> BoxFuture<'_, Result<AgentExecutionRelease, AgentBackendError>> {
        Box::pin(async move {
            let result = self.inner.release().await;
            if result.is_ok() {
                self.permit.take();
            }
            result
        })
    }
}

#[derive(Clone, Default)]
pub struct AgentBackendRegistry {
    backends: Arc<RwLock<BTreeMap<String, Arc<dyn ErasedAgentBackendFactory>>>>,
    scheduler: Arc<RwLock<Option<Arc<super::AgentScheduler>>>>,
}

impl AgentBackendRegistry {
    pub fn with_local() -> Self {
        let registry = Self::default();
        registry
            .register("local", crate::agent::execution::LocalAgentBackendFactory)
            .expect("the local backend is registered once");
        registry
    }

    pub(crate) fn ensure_scheduler(&self) -> Arc<super::AgentScheduler> {
        if let Some(scheduler) = self
            .scheduler
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return scheduler;
        }
        let mut selected = self
            .scheduler
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        selected
            .get_or_insert_with(|| {
                Arc::new(super::AgentScheduler::new(super::MAX_CONCURRENT_AGENT_RUNS))
            })
            .clone()
    }

    pub(crate) fn attach_scheduler(&self, scheduler: Arc<super::AgentScheduler>) {
        scheduler.configure_backend("local", super::MAX_CONCURRENT_AGENT_RUNS);
        *self
            .scheduler
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(scheduler);
    }

    pub fn register<F: AgentBackendFactory>(
        &self,
        backend_id: impl Into<String>,
        factory: F,
    ) -> Result<(), AgentBackendError> {
        let backend_id = backend_id.into();
        let mut backends = self
            .backends
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if backends.contains_key(&backend_id) {
            return Err(AgentBackendError::Duplicate(backend_id));
        }
        backends.insert(backend_id, Arc::new(TypedAgentBackendFactory(factory)));
        Ok(())
    }

    fn factory(
        &self,
        backend_id: &str,
    ) -> Result<Arc<dyn ErasedAgentBackendFactory>, AgentBackendError> {
        self.backends
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(backend_id)
            .cloned()
            .ok_or_else(|| AgentBackendError::NotRegistered(backend_id.to_string()))
    }

    pub fn capabilities(
        &self,
        backend_id: &str,
    ) -> Result<BTreeSet<AgentExecutionCapability>, AgentBackendError> {
        Ok(self.factory(backend_id)?.capabilities())
    }

    pub fn preflight(
        &self,
        backend_id: &str,
        requirements: &BTreeSet<AgentExecutionCapability>,
    ) -> Result<(), AgentBackendError> {
        let capabilities = self.capabilities(backend_id)?;
        if let Some(capability) = requirements.difference(&capabilities).next() {
            return Err(AgentBackendError::UnsupportedCapability {
                backend_id: backend_id.to_string(),
                capability: *capability,
            });
        }
        Ok(())
    }

    pub async fn execute<C: Send + 'static>(
        &self,
        backend_id: &str,
        context: C,
        request: AgentExecutionRequest,
    ) -> Result<AgentBackendResult, AgentBackendError> {
        self.execute_inner(backend_id, context, request, true).await
    }

    pub(crate) async fn execute_reserved<C: Send + 'static>(
        &self,
        backend_id: &str,
        context: C,
        request: AgentExecutionRequest,
    ) -> Result<AgentBackendResult, AgentBackendError> {
        self.execute_inner(backend_id, context, request, false)
            .await
    }

    pub async fn start<C: Send + 'static>(
        &self,
        backend_id: &str,
        context: C,
        request: AgentExecutionRequest,
    ) -> Result<Box<dyn AgentExecutionHandle>, AgentBackendError> {
        self.start_inner(backend_id, context, request, true).await
    }

    async fn start_inner<C: Send + 'static>(
        &self,
        backend_id: &str,
        context: C,
        request: AgentExecutionRequest,
        reserve_tree: bool,
    ) -> Result<Box<dyn AgentExecutionHandle>, AgentBackendError> {
        let factory = self.factory(backend_id)?;
        self.preflight(backend_id, &request.required_capabilities)?;
        let scheduler = self.ensure_scheduler();
        let permit = if reserve_tree {
            scheduler.acquire_backend(backend_id, &request.cancel).await
        } else {
            scheduler
                .acquire_reserved_backend(backend_id, &request.cancel)
                .await
        }
        .ok_or(AgentBackendError::Cancelled)?;
        let backend = factory.create(Box::new(context)).await?;
        let inner = backend.start(request).await?;
        Ok(Box::new(ReservedAgentExecutionHandle {
            inner,
            permit: Some(permit),
        }))
    }

    async fn execute_inner<C: Send + 'static>(
        &self,
        backend_id: &str,
        context: C,
        request: AgentExecutionRequest,
        reserve_tree: bool,
    ) -> Result<AgentBackendResult, AgentBackendError> {
        let factory = self.factory(backend_id)?;
        self.preflight(backend_id, &request.required_capabilities)?;
        let scheduler = self.ensure_scheduler();
        let _permit = if reserve_tree {
            scheduler.acquire_backend(backend_id, &request.cancel).await
        } else {
            scheduler
                .acquire_reserved_backend(backend_id, &request.cancel)
                .await
        }
        .ok_or(AgentBackendError::Cancelled)?;
        let backend = factory.create(Box::new(context)).await?;
        backend.execute(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio_util::sync::CancellationToken;

    use super::*;

    struct FakeFactory {
        creates: Arc<AtomicUsize>,
    }

    struct FakeExecution;

    impl AgentExecutionBackend for FakeExecution {
        fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
            [
                AgentExecutionCapability::Stop,
                AgentExecutionCapability::Reconnect,
            ]
            .into_iter()
            .collect()
        }

        fn start(
            self: Box<Self>,
            request: AgentExecutionRequest,
        ) -> BoxFuture<'static, Result<Box<dyn AgentExecutionHandle>, AgentBackendError>> {
            Box::pin(async move {
                Ok(Box::new(PermitHandle::new(request)) as Box<dyn AgentExecutionHandle>)
            })
        }

        fn execute(
            self: Box<Self>,
            request: AgentExecutionRequest,
        ) -> BoxFuture<'static, Result<AgentBackendResult, AgentBackendError>> {
            Box::pin(async move {
                if request.identity.run_id == "disconnect" {
                    return Err(AgentBackendError::Disconnected(
                        "injected disconnect".into(),
                    ));
                }
                Ok(AgentBackendResult {
                    status: pig_protocol::AgentRunStatus::Completed,
                    stop_reason: "completed".into(),
                    text: "fake".into(),
                    turns: 1,
                    usage: (1, 2, 3),
                })
            })
        }
    }

    struct PermitHandle {
        identity: crate::agent::AgentExecutionIdentity,
        released: Option<AgentExecutionRelease>,
    }

    impl PermitHandle {
        fn new(request: AgentExecutionRequest) -> Self {
            Self {
                identity: request.execution,
                released: None,
            }
        }

        fn unsupported<T>() -> BoxFuture<'static, Result<T, AgentBackendError>> {
            Box::pin(async { Err(AgentBackendError::InvalidContext("not exercised".into())) })
        }
    }

    impl AgentExecutionHandle for PermitHandle {
        fn identity(&self) -> &crate::agent::AgentExecutionIdentity {
            &self.identity
        }

        fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
            BTreeSet::new()
        }

        fn send(
            &mut self,
            _input: AgentExecutionInput,
        ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
            Self::unsupported()
        }

        fn stop(
            &mut self,
            _stop: AgentExecutionStop,
        ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
            Self::unsupported()
        }

        fn detach(&mut self) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
            Self::unsupported()
        }

        fn next(&mut self) -> BoxFuture<'_, Result<AgentExecutionUpdate, AgentBackendError>> {
            Self::unsupported()
        }

        fn release(&mut self) -> BoxFuture<'_, Result<AgentExecutionRelease, AgentBackendError>> {
            Box::pin(async move {
                if let Some(release) = &self.released {
                    return Ok(release.clone());
                }
                let release = AgentExecutionRelease {
                    cursor: crate::agent::AgentExecutionCursor {
                        attempt: self.identity.attempt,
                        backend_epoch: self.identity.backend_epoch.clone(),
                        sequence: 0,
                    },
                    identity: self.identity.clone(),
                    released_at_ms: 0,
                };
                self.released = Some(release.clone());
                Ok(release)
            })
        }
    }

    impl AgentBackendFactory for FakeFactory {
        type Context = &'static str;

        fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
            [
                AgentExecutionCapability::Stop,
                AgentExecutionCapability::Reconnect,
            ]
            .into_iter()
            .collect()
        }

        fn create(
            &self,
            context: &'static str,
        ) -> BoxFuture<'static, Result<Box<dyn AgentExecutionBackend>, AgentBackendError>> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            assert_eq!(context, "fake-context");
            Box::pin(async { Ok(Box::new(FakeExecution) as Box<dyn AgentExecutionBackend>) })
        }
    }

    fn request(run_id: &str) -> AgentExecutionRequest {
        let (events, _receiver) = async_channel::unbounded();
        AgentExecutionRequest::new(
            super::super::AgentRunIdentity {
                session_id: "s1".into(),
                agent_id: "a1".into(),
                run_id: run_id.into(),
            },
            events,
            CancellationToken::new(),
            super::super::AgentExecutionActivation::Start,
        )
    }

    #[tokio::test]
    async fn backend_contract_preflights_contexts_and_preserves_disconnects() {
        let registry = AgentBackendRegistry::default();
        let creates = Arc::new(AtomicUsize::new(0));
        registry
            .register(
                "fake",
                FakeFactory {
                    creates: creates.clone(),
                },
            )
            .expect("register fake");

        let mut unsupported = request("unsupported");
        unsupported
            .required_capabilities
            .insert(AgentExecutionCapability::Media);
        assert!(matches!(
            registry.execute("fake", "fake-context", unsupported).await,
            Err(AgentBackendError::UnsupportedCapability {
                capability: AgentExecutionCapability::Media,
                ..
            })
        ));
        assert_eq!(creates.load(Ordering::SeqCst), 0);
        assert!(matches!(
            registry
                .execute("fake", 42_u32, request("wrong-context"))
                .await,
            Err(AgentBackendError::InvalidContext(_))
        ));
        assert_eq!(creates.load(Ordering::SeqCst), 0);
        assert_eq!(
            registry
                .execute("fake", "fake-context", request("complete"))
                .await
                .expect("completed fake")
                .usage,
            (1, 2, 3)
        );
        assert!(matches!(
            registry
                .execute("fake", "fake-context", request("disconnect"))
                .await,
            Err(AgentBackendError::Disconnected(_))
        ));
        assert_eq!(creates.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn registry_handle_holds_capacity_until_release() {
        let registry = AgentBackendRegistry::default();
        registry
            .register(
                "fake",
                FakeFactory {
                    creates: Arc::new(AtomicUsize::new(0)),
                },
            )
            .expect("register fake");
        let scheduler = registry.ensure_scheduler();
        let capacity = scheduler.available();
        let mut handle = registry
            .start("fake", "fake-context", request("permit"))
            .await
            .expect("start handle");
        assert_eq!(scheduler.available(), capacity - 1);
        handle.release().await.expect("release handle");
        assert_eq!(scheduler.available(), capacity);
    }
}
