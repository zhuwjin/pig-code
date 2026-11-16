use std::sync::Arc;
use std::time::Duration;

use pig_protocol::{CoreError, FinishReason};

use crate::chat::{AssistantReplay, ToolCall};
use crate::metadata::ProviderResponseMetadata;

/// Internal provider -> session events during streaming
#[derive(Debug)]
pub enum ProviderEvent {
    Reasoning(String),
    Text(String),
    /// Ordered provider-native items for continuation after the live text deltas.
    AssistantReplay(AssistantReplay),
    ToolCalls(Vec<ToolCall>),
    /// A tool call became identifiable mid-stream. Chat uses a logical slot;
    /// Anthropic/Responses use their content/output index. The authoritative
    /// list still arrives via ToolCalls at stream end.
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    /// One incremental raw-JSON argument fragment for a started tool call,
    /// newline-batched (one event per content line; a 4KB fallback covers
    /// long single-line arguments)
    ToolCallDelta {
        index: usize,
        delta: String,
    },
    /// Token usage for a single request: input = uncached input (cache_read is the other
    /// portion of it served from cache; the two sum to total input), output is for accounting aggregation,
    /// used is the model-reported total consumption of this request (for the usage watermark check), total is the model context window,
    /// reasoning_output is the reasoning/thinking slice of output (informational; already counted in output)
    Usage {
        input: u64,
        cache_read: u64,
        output: u64,
        used: u64,
        total: u64,
        reasoning_output: u64,
    },
    /// Provider response headers and body object ID for the current attempt.
    /// A later snapshot replaces the earlier one for the same logical call.
    ResponseMetadata(ProviderResponseMetadata),
    /// Transient retry state for live rendering (never persisted): emitted
    /// right before a retry wait starts, and for the immediate in-stream
    /// silent retries (delay = 0). Does not count as forwarded content.
    Retrying(RetryNotice),
    Finished(FinishReason),
    /// Request/streaming failure: the structured error goes straight to the UI (passed through as Event::Error)
    Failed(CoreError),
}

/// Why a call is retrying, kept structured so the UI can localize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetryReason {
    /// HTTP 429; `retry_after` is the honored Retry-After when the server sent one
    RateLimit { retry_after: Option<Duration> },
    /// HTTP 5xx
    Server(u16),
    /// Send-phase network failure (DNS/TLS/connect/...); detail is the folded root cause
    Network(String),
    /// A 200 stream ended with no text/reasoning/tool calls (one silent retry)
    EmptyCompletion,
    /// Retryable in-band 200 error (Anthropic overloaded/rate_limit/api_error); detail is the surfaced text
    InBand(String),
}

/// One retry's visible state: which retry of how many, how long the wait is,
/// and why. `attempt` is 1-based over the retry budget (the initial attempt
/// is not a retry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryNotice {
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay: Duration,
    pub reason: RetryReason,
}

/// Per-call control surface handed to [`crate::stream_chat`]: cancellation
/// plus the "retry now" trigger that skips the remaining retry wait.
#[derive(Clone, Debug)]
pub struct CallControl {
    cancel: tokio_util::sync::CancellationToken,
    retry_now: Arc<tokio::sync::Notify>,
    idle_timeout: Duration,
    structured: Option<crate::StructuredOutput<'static>>,
}

impl CallControl {
    pub fn new(cancel: tokio_util::sync::CancellationToken) -> Self {
        Self {
            cancel,
            retry_now: Arc::new(tokio::sync::Notify::new()),
            idle_timeout: crate::transport::DEFAULT_IDLE_TIMEOUT,
            structured: None,
        }
    }

    /// Override the per-chunk idle deadline (tests use a short value; callers
    /// that never call this keep the transport default).
    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    pub(crate) fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    pub fn with_structured_output(mut self, schema: serde_json::Value) -> Self {
        self.structured = Some(crate::StructuredOutput {
            name: "agent_result",
            schema,
        });
        self
    }

    pub(crate) fn structured_output(&self) -> Option<&crate::StructuredOutput<'static>> {
        self.structured.as_ref()
    }

    pub fn cancel_token(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancel
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Skip the remaining retry wait: the next attempt starts immediately and
    /// consumes no extra budget. A click just before a wait begins also skips
    /// it (Notify keeps one stored permit); a click with no wait in flight is
    /// lost, so this only ever accelerates, never double-fires.
    pub fn retry_now(&self) {
        self.retry_now.notify_one();
    }

    pub(crate) async fn cancelled(&self) {
        self.cancel.cancelled().await
    }

    pub(crate) fn retry_notify(&self) -> &tokio::sync::Notify {
        &self.retry_now
    }
}

/// Internal stream-call error: the CoreError to surface plus whether a silent
/// retry is allowed. Retryable = in-band 200 errors whose type maps to a
/// retryable status (Anthropic overloaded_error/rate_limit_error/api_error);
/// the retry is additionally gated on "no content forwarded yet" in
/// stream_chat (the same boundary as the empty-completion retry). Errors after
/// send_with_retry's pre-response budget are never retryable here.
#[derive(Debug)]
pub(crate) struct CallError {
    pub error: CoreError,
    pub retryable: bool,
}

impl From<CoreError> for CallError {
    fn from(error: CoreError) -> Self {
        Self {
            error,
            retryable: false,
        }
    }
}
