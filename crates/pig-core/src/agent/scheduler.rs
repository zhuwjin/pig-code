use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

pub(crate) const MAX_CONCURRENT_AGENT_RUNS: usize = 8;

pub(crate) struct AgentScheduler {
    slots: Arc<tokio::sync::Semaphore>,
    capacity: usize,
    backends: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
}

impl AgentScheduler {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(capacity)),
            capacity,
            backends: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn available(&self) -> usize {
        self.slots.available_permits()
    }

    pub(crate) fn configure_backend(&self, backend_id: &str, capacity: usize) {
        self.backends
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(backend_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(capacity)));
    }

    fn backend_slots(&self, backend_id: &str) -> Arc<tokio::sync::Semaphore> {
        self.backends
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(backend_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(self.capacity)))
            .clone()
    }

    pub(crate) async fn acquire(&self, cancel: &CancellationToken) -> Option<AgentRunPermit> {
        let slots = self.slots.clone();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            permit = slots.acquire_owned() => permit.ok().map(|permit| AgentRunPermit::Tree {
                _tree: Some(permit),
                _backend: None,
            }),
        }
    }

    async fn acquire_backend_slot(
        &self,
        backend_id: &str,
        cancel: &CancellationToken,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let backend = self.backend_slots(backend_id);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            permit = backend.acquire_owned() => permit.ok(),
        }
    }

    pub(crate) async fn acquire_backend(
        &self,
        backend_id: &str,
        cancel: &CancellationToken,
    ) -> Option<AgentRunPermit> {
        let slots = self.slots.clone();
        let tree = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            permit = slots.acquire_owned() => permit.ok()?,
        };
        let backend = self.acquire_backend_slot(backend_id, cancel).await?;
        Some(AgentRunPermit::Tree {
            _tree: Some(tree),
            _backend: Some(backend),
        })
    }

    pub(crate) async fn acquire_reserved_backend(
        &self,
        backend_id: &str,
        cancel: &CancellationToken,
    ) -> Option<AgentRunPermit> {
        let backend = self.acquire_backend_slot(backend_id, cancel).await?;
        Some(AgentRunPermit::Tree {
            _tree: None,
            _backend: Some(backend),
        })
    }
}

#[allow(dead_code)]
pub(crate) enum AgentRunPermit {
    Tree {
        _tree: Option<tokio::sync::OwnedSemaphorePermit>,
        _backend: Option<tokio::sync::OwnedSemaphorePermit>,
    },
    #[cfg(test)]
    Test,
    #[cfg(test)]
    TestPermit(tokio::sync::OwnedSemaphorePermit),
}

#[cfg(test)]
pub(crate) static AGENT_SCHEDULER_TEST_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scheduler_queues_at_capacity_and_cancels_waiting_runs() {
        let scheduler = AgentScheduler::new(MAX_CONCURRENT_AGENT_RUNS);
        let cancel = CancellationToken::new();
        let permit = scheduler.acquire(&cancel).await.expect("free slot");
        assert_eq!(scheduler.available(), MAX_CONCURRENT_AGENT_RUNS - 1);
        drop(permit);
        assert_eq!(scheduler.available(), MAX_CONCURRENT_AGENT_RUNS);

        let mut permits = Vec::new();
        for _ in 0..MAX_CONCURRENT_AGENT_RUNS {
            permits.push(scheduler.acquire(&cancel).await.expect("slot"));
        }
        assert_eq!(scheduler.available(), 0);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(scheduler.acquire(&cancelled).await.is_none());
        drop(permits);
        assert_eq!(scheduler.available(), MAX_CONCURRENT_AGENT_RUNS);
    }

    #[tokio::test]
    async fn tree_and_backend_capacity_are_independent() {
        let first_tree = AgentScheduler::new(1);
        let second_tree = AgentScheduler::new(1);
        let cancel = CancellationToken::new();
        let first = first_tree
            .acquire(&cancel)
            .await
            .expect("first tree permit");
        assert!(second_tree.acquire(&cancel).await.is_some());
        drop(first);

        first_tree.configure_backend("ipc", 1);
        let ipc = first_tree
            .acquire_backend("ipc", &cancel)
            .await
            .expect("IPC permit");
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            first_tree
                .acquire_backend("ipc", &cancelled)
                .await
                .is_none()
        );
        drop(ipc);
        assert!(first_tree.acquire_backend("ipc", &cancel).await.is_some());
    }

    #[tokio::test]
    async fn reserved_tree_execution_only_acquires_a_backend_permit() {
        let scheduler = AgentScheduler::new(1);
        let cancel = CancellationToken::new();
        let _tree = scheduler.acquire(&cancel).await.expect("tree permit");
        assert!(scheduler.available() == 0);
        assert!(
            scheduler
                .acquire_reserved_backend("local", &cancel)
                .await
                .is_some()
        );
    }
}
