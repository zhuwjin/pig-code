use super::*;

pub(super) const MAX_PARALLEL_TOOLS: usize = 8;

pub(super) async fn run_tool_batch<T, F>(
    count: usize,
    cancel: &CancellationToken,
    mut start: impl FnMut(usize) -> F,
    mut finish: impl FnMut(usize, T),
) -> bool
where
    T: Send,
    F: std::future::Future<Output = T> + Send,
{
    let mut set = FuturesUnordered::new();
    let mut next = 0usize;
    let cancelled = loop {
        while next < count && set.len() < MAX_PARALLEL_TOOLS && !cancel.is_cancelled() {
            let index = next;
            let task = start(index);
            set.push(async move { (index, task.await) });
            next += 1;
        }
        if cancel.is_cancelled() {
            break true;
        }
        if set.is_empty() {
            break false;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break true,
            joined = set.next() => {
                if let Some((index, output)) = joined {
                    finish(index, output);
                }
            }
        }
    };
    if cancelled {
        set.clear();
    }
    cancelled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn batch_runs_tools_concurrently() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let cancelled = run_tool_batch(
            2,
            &CancellationToken::new(),
            |index| {
                let active = active.clone();
                let maximum = maximum.clone();
                let release = release.clone();
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    if index == 0 {
                        release.notified().await;
                    } else {
                        release.notify_one();
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                    index
                }
            },
            |_, _| {},
        )
        .await;
        assert!(!cancelled);
        assert!(maximum.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn batch_does_not_start_a_tool_after_cancellation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let starts = Arc::new(AtomicUsize::new(0));
        let cancelled = run_tool_batch(
            1,
            &cancel,
            |index| {
                starts.fetch_add(1, Ordering::SeqCst);
                async move { index }
            },
            |_, _| {},
        )
        .await;
        assert!(cancelled);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn batch_reports_cancellation_after_all_tools_have_drained() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = run_tool_batch(0, &cancel, |index| async move { index }, |_, _| {}).await;
        assert!(cancelled);
    }
}
