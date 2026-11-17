use tokio_util::sync::CancellationToken;

pub(crate) const MAX_CONCURRENT_AGENT_RUNS: usize = 8;

pub(crate) struct AgentScheduler {
    slots: std::sync::Arc<tokio::sync::Semaphore>,
}

impl AgentScheduler {
    fn new(capacity: usize) -> Self {
        Self {
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(capacity)),
        }
    }

    fn global() -> &'static Self {
        static SCHEDULER: std::sync::OnceLock<AgentScheduler> = std::sync::OnceLock::new();
        SCHEDULER.get_or_init(|| AgentScheduler::new(MAX_CONCURRENT_AGENT_RUNS))
    }

    pub(crate) fn available(&self) -> usize {
        self.slots.available_permits()
    }

    pub(crate) async fn acquire(&self, cancel: &CancellationToken) -> Option<AgentRunPermit> {
        let slots = self.slots.clone();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            permit = slots.acquire_owned() => permit.ok().map(AgentRunPermit::Global),
        }
    }
}

pub(crate) fn scheduler() -> &'static AgentScheduler {
    AgentScheduler::global()
}

#[allow(dead_code)]
pub(crate) enum AgentRunPermit {
    Global(tokio::sync::OwnedSemaphorePermit),
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
        let _test_lock = AGENT_SCHEDULER_TEST_LOCK.lock().await;
        let scheduler = scheduler();
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
}
