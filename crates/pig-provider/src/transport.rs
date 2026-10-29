//! Shared transport policy: an idle (not overall) deadline for response-body
//! reads and the detail wording every body/stream timeout uses. A stream may
//! legitimately run for minutes, so only the gap between chunks is bounded.

use std::time::Duration;

use pig_protocol::CoreError;

/// Maximum silence between two response-body chunks. Streamed answers and
/// sidecar JSON bodies share the same budget; servers keep long requests alive
/// with SSE comments, so silence beyond this means the connection is wedged.
pub(crate) const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) enum BodyRead {
    Text(String),
    Cancelled,
}

/// A body read that failed part-way. `partial` keeps the bytes already
/// received so a non-2xx caller can still classify the HTTP status (for
/// example a context-window overflow that arrived before the stall) instead
/// of replacing that classification with a bare idle/stream error.
pub(crate) struct BodyReadError {
    pub error: CoreError,
    pub partial: String,
}

pub(crate) fn idle_detail(what: &str, idle: Duration) -> String {
    format!("{what} idle for {:.1}s", idle.as_secs_f64())
}

fn partial_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// Read a response body with a per-chunk idle deadline and cancellation.
/// Never an overall deadline: a large but healthy body keeps flowing for as
/// long as chunks arrive.
pub(crate) async fn read_body_with_idle(
    response: reqwest::Response,
    cancel: &tokio_util::sync::CancellationToken,
    idle: Duration,
) -> Result<BodyRead, BodyReadError> {
    use futures_util::StreamExt as _;

    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        // Cancellation is checked first so an already-cancelled read never
        // waits for (or consumes) another chunk.
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(BodyRead::Cancelled),
            next = tokio::time::timeout(idle, stream.next()) => match next {
                Ok(next) => next,
                Err(_) => {
                    return Err(BodyReadError {
                        error: CoreError::StreamRead {
                            detail: idle_detail("Response body", idle),
                        },
                        partial: partial_text(&body),
                    });
                }
            },
        };
        match next {
            Some(Ok(bytes)) => body.extend_from_slice(&bytes),
            Some(Err(error)) => {
                return Err(BodyReadError {
                    error: CoreError::StreamRead {
                        detail: error.to_string(),
                    },
                    partial: partial_text(&body),
                });
            }
            None => break,
        }
    }
    // A cancellation that raced the final byte must not become a successful
    // response; the caller repeats this check after parsing as the last gate.
    if cancel.is_cancelled() {
        return Ok(BodyRead::Cancelled);
    }
    match String::from_utf8(body) {
        Ok(text) => Ok(BodyRead::Text(text)),
        Err(error) => {
            let partial = partial_text(error.as_bytes());
            let error = CoreError::Internal {
                detail: format!("Failed to decode response body: {error}"),
            };
            Err(BodyReadError { error, partial })
        }
    }
}
