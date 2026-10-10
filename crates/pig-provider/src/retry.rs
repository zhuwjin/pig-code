use std::time::Duration;

use pig_protocol::CoreError;

use crate::events::{CallControl, ProviderEvent, RetryNotice, RetryReason};

/// Retry policy: exponential backoff, at most 10 retries; starting at 1s, doubling, capped at 30s.
/// Retries happen only "before the response is established" (connect/TLS/send failures, 429/5xx) --
/// once the response stream is established there is no retry, avoiding duplicate output of already-streamed content.
pub(crate) const MAX_RETRIES: u32 = 10;
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_CAP: Duration = Duration::from_secs(30);
/// Retry-After values above this are clamped (a stuck server must not park the call for minutes)
const RETRY_AFTER_CAP_SECS: u64 = 120;

pub(crate) enum SendOutcome {
    Response(reqwest::Response),
    Cancelled,
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// The numeric Retry-After a 429/5xx carried, in seconds, clamped to
/// [RETRY_AFTER_CAP_SECS]; None when absent or non-numeric
fn retry_after_secs(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|secs| secs.min(RETRY_AFTER_CAP_SECS))
}

fn backoff_delay(retry: u32, response: Option<&reqwest::Response>) -> Duration {
    // When a 429/5xx carries Retry-After, respect the server's pacing first
    if let Some(secs) = response.and_then(retry_after_secs) {
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

/// Streaming requests cannot set an overall timeout (the response body may not end for a long time); only the connect time is limited.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

/// Send with the retry policy above. Every accepted retry is announced on
/// `events` (a [ProviderEvent::Retrying] notice) right before its wait, and
/// the wait can be skipped mid-flight via [CallControl::retry_now].
pub(crate) async fn send_with_retry(
    // Receives the 0-based attempt index (mirrors the SDK's X-Stainless-Retry-Count)
    build: impl Fn(u32) -> reqwest::RequestBuilder,
    control: &CallControl,
    events: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
) -> Result<SendOutcome, CoreError> {
    let mut retry = 0;
    loop {
        let result = tokio::select! {
            result = build(retry).send() => result,
            _ = control.cancelled() => return Ok(SendOutcome::Cancelled),
        };
        // Errors in the send phase can only be network-class errors (request construction errors already fail at the builder stage)
        let reason = match &result {
            Ok(response) if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                Some(RetryReason::RateLimit {
                    retry_after: retry_after_secs(response).map(Duration::from_secs),
                })
            }
            Ok(response) if retryable_status(response.status()) => {
                Some(RetryReason::Server(response.status().as_u16()))
            }
            Ok(_) => None,
            Err(e) => Some(RetryReason::Network(net_root(e))),
        };
        let Some(reason) = reason else {
            // After status-code error retries are exhausted, the response body is left for the caller to format (HTTP xxx: ...)
            return match result {
                Ok(response) => Ok(SendOutcome::Response(response)),
                Err(e) => Err(net_err(e)),
            };
        };
        if retry >= MAX_RETRIES {
            return match result {
                Ok(response) => Ok(SendOutcome::Response(response)),
                Err(e) => Err(net_err(e)),
            };
        }
        let delay = backoff_delay(retry, result.as_ref().ok());
        retry += 1;
        let _ = events.send(ProviderEvent::Retrying(RetryNotice {
            attempt: retry,
            max_attempts: MAX_RETRIES,
            delay,
            reason,
        }));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = control.cancelled() => return Ok(SendOutcome::Cancelled),
            _ = control.retry_notify().notified() => {}
        }
    }
}
