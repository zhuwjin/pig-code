use super::*;

pub(super) const MAX_PARALLEL_TOOLS: usize = 8;
/// Bounded quiescence window on batch cancellation: gives cooperative in-flight
/// tools time to observe `cancel` and release locks/handles before forcing drop.
const BATCH_CANCEL_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

pub(super) async fn run_tool_batch<T, F>(
    count: usize,
    cancel: &CancellationToken,
    start: impl FnMut(usize) -> F,
    finish: impl FnMut(usize, T),
) -> bool
where
    T: Send,
    F: std::future::Future<Output = T> + Send,
{
    run_tool_batch_with_grace(count, cancel, BATCH_CANCEL_GRACE, start, finish).await
}

pub(super) async fn run_tool_batch_with_grace<T, F>(
    count: usize,
    cancel: &CancellationToken,
    grace: std::time::Duration,
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
    if cancelled && !set.is_empty() {
        // Drain already-started tool bodies to quiescence within the grace
        // window instead of dropping live futures abruptly.
        let deadline = tokio::time::Instant::now() + grace;
        while !set.is_empty() {
            match tokio::time::timeout_at(deadline, set.next()).await {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
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
    async fn cancellation_drains_cooperative_in_flight_tools_to_quiescence() {
        let cancel = CancellationToken::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let cleaned_up = Arc::new(AtomicUsize::new(0));
        let cancel_clone = cancel.clone();
        let started_clone = started.clone();
        let cleaned_clone = cleaned_up.clone();
        let driver = tokio::spawn(async move {
            run_tool_batch_with_grace(
                2,
                &cancel_clone,
                std::time::Duration::from_secs(2),
                |index| {
                    let cancel = cancel_clone.clone();
                    let started = started_clone.clone();
                    let cleaned = cleaned_clone.clone();
                    async move {
                        started.notify_one();
                        cancel.cancelled().await;
                        // Simulate asynchronous cleanup/quiescence after observing cancel.
                        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                        cleaned.fetch_add(1, Ordering::SeqCst);
                        index
                    }
                },
                |_, _| {},
            )
            .await
        });
        started.notified().await;
        tokio::task::yield_now().await;
        cancel.cancel();
        let cancelled = driver.await.expect("driver");
        assert!(cancelled);
        assert!(
            cleaned_up.load(Ordering::SeqCst) >= 1,
            "started cooperative tools must finish cleanup before batch returns"
        );
    }

    #[tokio::test]
    async fn cancellation_bounds_uncooperative_in_flight_tools_to_grace_window() {
        let cancel = CancellationToken::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let cancel_clone = cancel.clone();
        let started_clone = started.clone();
        let driver = tokio::spawn(async move {
            run_tool_batch_with_grace(
                1,
                &cancel_clone,
                std::time::Duration::from_millis(40),
                |_index| {
                    let started = started_clone.clone();
                    async move {
                        started.notify_one();
                        std::future::pending::<usize>().await
                    }
                },
                |_, _| {},
            )
            .await
        });
        started.notified().await;
        let begin = std::time::Instant::now();
        cancel.cancel();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(2), driver)
            .await
            .expect("grace window must bound hung tools")
            .expect("driver");
        assert!(cancelled);
        assert!(
            begin.elapsed() < std::time::Duration::from_secs(1),
            "hung tool should be dropped shortly after grace window"
        );
    }

    #[tokio::test]
    async fn batch_reports_cancellation_after_all_tools_have_drained() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = run_tool_batch(0, &cancel, |index| async move { index }, |_, _| {}).await;
        assert!(cancelled);
    }
}
