use std::time::Duration;

use pig_protocol::CoreError;

use crate::events::{CallControl, ProviderEvent, RetryNotice, RetryReason};

/// Retry policy: exponential backoff, at most 10 retries; starting at 1s, doubling, capped at 30s.
/// Retries happen only "before the response is established" (connect/TLS/send failures, 429/5xx) --
/// once the response stream is established there is no retry, avoiding duplicate output of already-streamed content.
pub(crate) const MAX_RETRIES: u32 = 10;
/// One-shot sidecars are bounded much tighter than a live turn: they block a
/// user-visible maintenance action, so two retries (1s + 2s) is the budget.
pub(crate) const SIDECAR_MAX_RETRIES: u32 = 2;
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_CAP: Duration = Duration::from_secs(30);
/// Retry-After values above this are clamped (a stuck server must not park the call for minutes)
const RETRY_AFTER_CAP_STREAM: Duration = Duration::from_secs(120);
/// A sidecar blocks a user-facing action, so its server-provided pacing is
/// capped to the same small scale as its backoff budget.
const RETRY_AFTER_CAP_SIDECAR: Duration = Duration::from_secs(2);

/// One retry loop for streams and one-shots: the same transient
/// classification and exponential backoff, with only the budget and the
/// Retry-After ceiling differing per call kind. `retry_now` is present for
/// live streams (the UI can skip a wait) and absent for sidecars.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetryPolicy {
    pub max_retries: u32,
    pub retry_after_cap: Duration,
}

pub(crate) const STREAM_RETRY_POLICY: RetryPolicy = RetryPolicy {
    max_retries: MAX_RETRIES,
    retry_after_cap: RETRY_AFTER_CAP_STREAM,
};

pub(crate) const SIDECAR_RETRY_POLICY: RetryPolicy = RetryPolicy {
    max_retries: SIDECAR_MAX_RETRIES,
    retry_after_cap: RETRY_AFTER_CAP_SIDECAR,
};

#[derive(Debug)]
pub(crate) enum SendOutcome {
    Response(reqwest::Response),
    Cancelled,
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// The numeric Retry-After a 429/5xx carried, in seconds, clamped to the
/// policy's ceiling; None when absent or non-numeric
fn retry_after_secs(response: &reqwest::Response, cap: Duration) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|secs| secs.min(cap.as_secs()))
}

fn backoff_delay(
    retry: u32,
    response: Option<&reqwest::Response>,
    policy: RetryPolicy,
) -> Duration {
    // When a 429/5xx carries Retry-After, respect the server's pacing first
    if let Some(secs) =
        response.and_then(|response| retry_after_secs(response, policy.retry_after_cap))
    {
        return Duration::from_secs(secs);
    }
    RETRY_BASE
        .saturating_mul(2u32.saturating_pow(retry))
        .min(RETRY_CAP)
}

/// reqwest's Display only says "error sending request"; the real cause (DNS/certificate/proxy/connection refused)
/// is on the source chain. Fold the deepest cause into one line for easier diagnosis.
fn net_root(e: &reqwest::Error) -> String {
    let mut root: Option<String> = None;
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        root = Some(s.to_string());
        source = s.source();
    }
    match root {
        Some(root) if root != e.to_string() => format!("{e} ({root})"),
        _ => e.to_string(),
    }
}

pub(crate) fn net_err(e: reqwest::Error) -> CoreError {
    CoreError::Network {
        detail: net_root(&e),
    }
}

/// Transport failures worth another attempt. Builder/body errors are local
/// request-construction bugs: retrying them can never succeed.
pub(crate) fn is_retryable_send_error(error: &reqwest::Error) -> bool {
    !error.is_builder() && !error.is_body()
}

/// Map a request-send failure: genuine network failures keep the `Network`
/// shape, while builder/body failures surface as internal diagnostics.
pub(crate) fn send_error(error: reqwest::Error) -> CoreError {
    if error.is_builder() || error.is_body() {
        CoreError::Internal {
            detail: net_root(&error),
        }
    } else {
        net_err(error)
    }
}

/// Streaming requests cannot set an overall timeout (the response body may not end for a long time); only the connect time is limited.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

/// Shared retry driver for streams and one-shots. Every accepted retry is
/// announced through `on_retry` right before its wait. Cancellation is checked
/// before the request is built and is polled first (biased) so a pre-cancelled
/// call can never send; a live stream's wait can also be skipped via
/// [CallControl::retry_now].
async fn send_with_policy(
    // Receives the 0-based attempt index (mirrors the SDK's X-Stainless-Retry-Count)
    build: impl Fn(u32) -> reqwest::RequestBuilder,
    cancel: &tokio_util::sync::CancellationToken,
    retry_now: Option<&tokio::sync::Notify>,
    policy: RetryPolicy,
    mut on_retry: impl FnMut(RetryNotice) -> futures_util::future::BoxFuture<'static, ()>,
) -> Result<SendOutcome, CoreError> {
    if cancel.is_cancelled() {
        return Ok(SendOutcome::Cancelled);
    }
    let mut retry = 0;
    loop {
        // `biased` plus the cancellation branch first makes the guarantee
        // deterministic: select no longer randomly polls the send future when
        // both a pending cancel and a ready request are observable.
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(SendOutcome::Cancelled),
            result = build(retry).send() => result,
        };
        // Errors in the send phase are network-class failures; builder/body
        // errors are permanent and must not consume the retry budget.
        let reason = match &result {
            Ok(response) if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                Some(RetryReason::RateLimit {
                    retry_after: retry_after_secs(response, policy.retry_after_cap)
                        .map(Duration::from_secs),
                })
            }
            Ok(response) if retryable_status(response.status()) => {
                Some(RetryReason::Server(response.status().as_u16()))
            }
            Ok(_) => None,
            Err(error) if is_retryable_send_error(error) => {
                Some(RetryReason::Network(net_root(error)))
            }
            Err(_) => None,
        };
        let Some(reason) = reason else {
            // After status-code error retries are exhausted, the response body is left for the caller to format (HTTP xxx: ...)
            return match result {
                Ok(response) => Ok(SendOutcome::Response(response)),
                Err(error) => Err(send_error(error)),
            };
        };
        if retry >= policy.max_retries {
            return match result {
                Ok(response) => Ok(SendOutcome::Response(response)),
                Err(error) => Err(send_error(error)),
            };
        }
        let delay = backoff_delay(retry, result.as_ref().ok(), policy);
        retry += 1;
        on_retry(RetryNotice {
            attempt: retry,
            max_attempts: policy.max_retries,
            delay,
            reason,
        })
        .await;
        if wait_before_retry(delay, cancel, retry_now).await {
            return Ok(SendOutcome::Cancelled);
        }
    }
}

/// Wait out a retry delay unless cancellation (always wins) or a retry-now
/// request arrives first. Returns true when the caller must stop as cancelled.
async fn wait_before_retry(
    delay: Duration,
    cancel: &tokio_util::sync::CancellationToken,
    retry_now: Option<&tokio::sync::Notify>,
) -> bool {
    match retry_now {
        Some(notify) => tokio::select! {
            biased;
            _ = cancel.cancelled() => true,
            _ = tokio::time::sleep(delay) => false,
            _ = notify.notified() => false,
        },
        None => tokio::select! {
            biased;
            _ = cancel.cancelled() => true,
            _ = tokio::time::sleep(delay) => false,
        },
    }
}

/// Streaming send: the shared policy with the stream budget and a live
/// [ProviderEvent::Retrying] notice for every accepted retry.
pub(crate) async fn send_with_retry(
    build: impl Fn(u32) -> reqwest::RequestBuilder,
    control: &CallControl,
    events: &tokio::sync::mpsc::Sender<ProviderEvent>,
) -> Result<SendOutcome, CoreError> {
    send_with_policy(
        build,
        control.cancel_token(),
        Some(control.retry_notify()),
        STREAM_RETRY_POLICY,
        move |notice| {
            let events = events.clone();
            Box::pin(async move {
                let _ = events.send(ProviderEvent::Retrying(notice)).await;
            })
        },
    )
    .await
}

/// Sidecar/one-shot send: the same policy with the tighter sidecar budget and
/// a structured warn log instead of a live notice channel. Permanent
/// builder/body errors and non-retryable statuses return on the first attempt;
/// the caller formats the final response's body.
pub(crate) async fn send_sidecar_with_retry(
    build: impl Fn(u32) -> reqwest::RequestBuilder,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<SendOutcome, CoreError> {
    send_with_policy(build, cancel, None, SIDECAR_RETRY_POLICY, |notice| {
        tracing::warn!(
            attempt = notice.attempt,
            max_attempts = notice.max_attempts,
            delay_ms = notice.delay.as_millis() as u64,
            reason = ?notice.reason,
            "retrying one-shot request"
        );
        Box::pin(async {})
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Request-construction failures are permanent: one attempt, an internal
    /// diagnostic, and no retry budget consumed.
    #[tokio::test]
    async fn builder_errors_are_not_retried() {
        let attempts = AtomicU32::new(0);
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = send_sidecar_with_retry(
            |_| {
                attempts.fetch_add(1, Ordering::Relaxed);
                reqwest::Client::new().post("http://[::1")
            },
            &cancel,
        )
        .await
        .expect_err("invalid URL must fail");
        assert!(
            matches!(result, CoreError::Internal { .. }),
            "builder errors are internal diagnostics: {result:?}"
        );
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    /// A call cancelled before it starts must not even build a request; the
    /// old polling select could still poll the send future first.
    #[tokio::test]
    async fn pre_cancelled_send_never_builds_or_sends_a_request() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let outcome = send_with_policy(
            |_| panic!("a cancelled call must not build a request"),
            &cancel,
            None,
            SIDECAR_RETRY_POLICY,
            |_| panic!("a cancelled call cannot retry"),
        )
        .await
        .expect("cancellation is not an error");
        assert!(matches!(outcome, SendOutcome::Cancelled));
    }

    /// The retry-now trigger must skip the announced delay without sleeping.
    /// A paused Tokio clock makes this deterministic: a slept wait would
    /// auto-advance by 60s, so elapsed time is exactly zero only when the
    /// notify won.
    #[tokio::test(start_paused = true)]
    async fn retry_now_skips_the_backoff_without_advancing_time() {
        let control = CallControl::new(tokio_util::sync::CancellationToken::new());
        let started = tokio::time::Instant::now();
        let waiter = control.clone();
        let task = tokio::spawn(async move {
            wait_before_retry(
                Duration::from_secs(60),
                waiter.cancel_token(),
                Some(waiter.retry_notify()),
            )
            .await
        });
        tokio::task::yield_now().await;
        control.retry_now();
        assert!(!task.await.expect("waiter joins"));
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "the wait was skipped, not slept"
        );
    }

    /// The sidecar policy clamps a server's Retry-After to its small ceiling
    /// instead of parking a user-facing one-shot for the announced interval.
    #[tokio::test]
    async fn sidecar_retry_after_is_capped_to_the_sidecar_ceiling() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 60\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("write response");
            let _ = socket.shutdown().await;
        });

        let captured = Arc::new(Mutex::new(None));
        let observed = captured.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let token = cancel.clone();
        let client = reqwest::Client::new();
        let url = format!("http://{address}/v1/chat/completions");
        let outcome = send_with_policy(
            |_| client.post(&url),
            &cancel,
            None,
            SIDECAR_RETRY_POLICY,
            move |notice| {
                *observed.lock().expect("capture") = Some(notice.delay);
                token.cancel();
                Box::pin(async {})
            },
        )
        .await;
        assert!(matches!(outcome, Ok(SendOutcome::Cancelled)));
        assert_eq!(
            captured.lock().expect("capture").as_ref(),
            Some(&RETRY_AFTER_CAP_SIDECAR),
            "a 60s Retry-After is clamped for a sidecar"
        );
        server.await.expect("server joins");
    }

    #[test]
    fn retryable_classification_excludes_local_construction_failures() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let builder_error = runtime
            .block_on(async { reqwest::Client::new().post("://bad").send().await })
            .expect_err("invalid URL");
        assert!(builder_error.is_builder());
        assert!(!is_retryable_send_error(&builder_error));
        assert!(matches!(
            send_error(builder_error),
            CoreError::Internal { .. }
        ));
    }
}
