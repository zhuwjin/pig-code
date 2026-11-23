//! Provider resilience: per-endpoint circuit breaker and multi-provider
//! fallback chain.
//!
//! Tracks consecutive transient failures per `(base_url, model)` endpoint:
//! - `Closed` -> `Open` once `failure_threshold` consecutive failures occur.
//! - `Open` -> `HalfOpen` after `cooldown` elapses, admitting a single probe.
//! - Probe success -> `Closed`; probe failure -> `Open` again.
//!
//! [`stream_chat_with_fallback`] runs an ordered candidate list through
//! [`ProviderRegistry`], skipping open circuits and failing over cleanly when a
//! candidate fails before emitting any visible content.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pig_protocol::CoreError;

use crate::chat::{ChatMsg, ResolvedModel};
use crate::core_error_en;
use crate::events::{CallControl, ProviderEvent, RetryNotice, RetryReason};
use crate::registry::ProviderRegistry;
use crate::stream::stream_chat_with_registry;

/// Configuration for one endpoint circuit breaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CircuitBreakerConfig {
    /// Consecutive transient failures required to trip the breaker open.
    pub failure_threshold: u32,
    /// Minimum open duration before allowing a half-open probe request.
    pub cooldown: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            cooldown: Duration::from_secs(30),
        }
    }
}

/// Observable circuit breaker state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Clone, Debug)]
struct BreakerEntry {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    half_open_probe_in_flight: bool,
}

impl Default for BreakerEntry {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            opened_at: None,
            half_open_probe_in_flight: false,
        }
    }
}

/// Process- or session-scoped endpoint circuit-breaker registry.
pub struct EndpointHealthRegistry {
    config: CircuitBreakerConfig,
    entries: Mutex<HashMap<String, BreakerEntry>>,
}

impl Default for EndpointHealthRegistry {
    fn default() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }
}

impl EndpointHealthRegistry {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn endpoint_key(model: &ResolvedModel) -> String {
        format!("{}::{}", model.base_url.trim_end_matches('/'), model.model)
    }

    /// Inspect current circuit state at `now`.
    pub fn state_at(&self, key: &str, now: Instant) -> CircuitState {
        let guard = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = guard.get(key) else {
            return CircuitState::Closed;
        };
        match entry.opened_at {
            None => CircuitState::Closed,
            Some(opened) if now.saturating_duration_since(opened) >= self.config.cooldown => {
                CircuitState::HalfOpen
            }
            Some(_) => CircuitState::Open,
        }
    }

    pub fn state(&self, key: &str) -> CircuitState {
        self.state_at(key, Instant::now())
    }

    /// Returns `true` if a request may proceed to `key` (admitting at most one
    /// concurrent probe when transitioning `Open` -> `HalfOpen`).
    pub fn allow_request(&self, key: &str) -> bool {
        self.allow_request_at(key, Instant::now())
    }

    pub fn allow_request_at(&self, key: &str, now: Instant) -> bool {
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = guard.entry(key.to_string()).or_default();
        match entry.opened_at {
            None => true,
            Some(opened) => {
                if now.saturating_duration_since(opened) >= self.config.cooldown {
                    if entry.half_open_probe_in_flight {
                        false
                    } else {
                        entry.half_open_probe_in_flight = true;
                        true
                    }
                } else {
                    false
                }
            }
        }
    }

    /// Record a successful call: closes the circuit and resets consecutive failures.
    pub fn record_success(&self, key: &str) {
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = guard.get_mut(key) {
            entry.consecutive_failures = 0;
            entry.opened_at = None;
            entry.half_open_probe_in_flight = false;
        }
    }

    /// Record a transient endpoint failure. Trips the circuit open once
    /// `failure_threshold` is reached (or immediately re-opens on a failed half-open probe).
    pub fn record_failure(&self, key: &str) {
        self.record_failure_at(key, Instant::now());
    }

    pub fn record_failure_at(&self, key: &str, now: Instant) {
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = guard.entry(key.to_string()).or_default();
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        if entry.half_open_probe_in_flight
            || entry.consecutive_failures >= self.config.failure_threshold.max(1)
        {
            entry.opened_at = Some(now);
            entry.half_open_probe_in_flight = false;
        }
    }
}

/// Whether an error is an endpoint/availability failure eligible for failover
/// to a secondary provider (never fail over on context-window overflow or user cancel).
fn is_failover_eligible(error: &CoreError) -> bool {
    match error {
        CoreError::Network { .. } | CoreError::StreamRead { .. } => true,
        CoreError::Internal { detail } => {
            detail.contains("HTTP 429")
                || detail.contains("HTTP 5")
                || detail.contains("overloaded")
                || detail.contains("rate_limit")
        }
        CoreError::Contextual { error, .. } => is_failover_eligible(error),
        _ => false,
    }
}

/// Stream a chat turn across an ordered list of fallback candidate models.
///
/// Candidates whose circuit breaker is currently `Open` are skipped unless all
/// candidates are open (in which case the first candidate is still attempted so
/// recovery remains possible). Failover happens transparently ONLY if a
/// candidate fails before emitting any visible content to `tx`.
pub async fn stream_chat_with_fallback(
    registry: &ProviderRegistry,
    health: &EndpointHealthRegistry,
    candidates: Vec<ResolvedModel>,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: tokio::sync::mpsc::Sender<ProviderEvent>,
    control: CallControl,
) {
    if candidates.is_empty() {
        let _ = tx
            .send(ProviderEvent::Failed(CoreError::Internal {
                detail: "No candidate models provided for fallback stream".to_string(),
            }))
            .await;
        return;
    }

    // Prefer healthy / half-open candidates; if every circuit is open, try them
    // in order anyway so the session is never permanently wedged.
    let mut ordered: Vec<ResolvedModel> = Vec::with_capacity(candidates.len());
    let mut skipped_open: Vec<ResolvedModel> = Vec::new();
    for candidate in candidates {
        let key = EndpointHealthRegistry::endpoint_key(&candidate);
        if health.allow_request(&key) {
            ordered.push(candidate);
        } else {
            skipped_open.push(candidate);
        }
    }
    if ordered.is_empty() {
        ordered = skipped_open;
    }

    let total = ordered.len();
    let mut tx = tx;
    for (idx, candidate) in ordered.into_iter().enumerate() {
        let has_next = idx + 1 < total;
        let key = EndpointHealthRegistry::endpoint_key(&candidate);
        let (itx, mut irx) = tokio::sync::mpsc::channel::<ProviderEvent>(256);
        let saw_content = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&saw_content);
        let forward = tokio::spawn(async move {
            let mut failed: Option<CoreError> = None;
            while let Some(event) = irx.recv().await {
                match event {
                    ProviderEvent::Failed(error) => {
                        failed = Some(error);
                        continue;
                    }
                    other => {
                        let content = match &other {
                            ProviderEvent::Text(_)
                            | ProviderEvent::Reasoning(_)
                            | ProviderEvent::ToolCalls(_) => true,
                            ProviderEvent::AssistantReplay(replay) => !replay.items.is_empty(),
                            _ => false,
                        };
                        if content {
                            flag.store(true, Ordering::Relaxed);
                        }
                        if tx.send(other).await.is_err() {
                            break;
                        }
                    }
                }
            }
            (tx, failed)
        });

        stream_chat_with_registry(
            registry,
            candidate,
            messages.clone(),
            tools.clone(),
            itx,
            control.clone(),
        )
        .await;

        let Ok((back, failed)) = forward.await else {
            return;
        };
        tx = back;

        match failed {
            None => {
                health.record_success(&key);
                return;
            }
            Some(error) => {
                if is_failover_eligible(&error) {
                    health.record_failure(&key);
                }
                if has_next
                    && !saw_content.load(Ordering::Relaxed)
                    && !control.is_cancelled()
                    && is_failover_eligible(&error)
                {
                    tracing::warn!(
                        endpoint = %key,
                        error = %core_error_en(&error),
                        "primary provider failed before emitting content; failing over to next candidate"
                    );
                    let _ = tx
                        .send(ProviderEvent::Retrying(RetryNotice {
                            attempt: (idx + 1) as u32,
                            max_attempts: (total - 1) as u32,
                            delay: Duration::ZERO,
                            reason: RetryReason::Network(format!(
                                "Failover after {}",
                                core_error_en(&error)
                            )),
                        }))
                        .await;
                    continue;
                }
                let _ = tx.send(ProviderEvent::Failed(error)).await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::test_model;
    use crate::events::CallError;
    use crate::registry::ProviderDriver;
    use futures_util::future::BoxFuture;
    use pig_protocol::{ApiFormat, FinishReason};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn circuit_breaker_transitions_closed_open_half_open_and_recovers() {
        let reg = EndpointHealthRegistry::new(CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(10),
        });
        let t0 = Instant::now();
        assert_eq!(reg.state_at("ep", t0), CircuitState::Closed);
        assert!(reg.allow_request_at("ep", t0));

        reg.record_failure_at("ep", t0);
        assert_eq!(reg.state_at("ep", t0), CircuitState::Closed);
        reg.record_failure_at("ep", t0);
        assert_eq!(reg.state_at("ep", t0), CircuitState::Open);
        assert!(!reg.allow_request_at("ep", t0 + Duration::from_secs(5)));

        // After cooldown, transitions to HalfOpen and admits exactly one probe.
        let t1 = t0 + Duration::from_secs(11);
        assert_eq!(reg.state_at("ep", t1), CircuitState::HalfOpen);
        assert!(reg.allow_request_at("ep", t1));
        assert!(!reg.allow_request_at("ep", t1), "only one concurrent probe allowed");

        // Probe succeeds -> Closed.
        reg.record_success("ep");
        assert_eq!(reg.state_at("ep", t1), CircuitState::Closed);
        assert!(reg.allow_request_at("ep", t1));
    }

    struct RoutingMockDriver {
        primary_calls: Arc<AtomicUsize>,
        secondary_calls: Arc<AtomicUsize>,
    }

    impl ProviderDriver for RoutingMockDriver {
        fn id(&self) -> &str {
            "test.routing"
        }

        fn handles(&self, _config: &ResolvedModel) -> bool {
            true
        }

        fn stream<'a>(
            &'a self,
            config: &'a ResolvedModel,
            _messages: Vec<ChatMsg>,
            _tools: Vec<serde_json::Value>,
            tx: &'a tokio::sync::mpsc::Sender<ProviderEvent>,
            _control: &'a CallControl,
        ) -> BoxFuture<'a, Result<(), CallError>> {
            let is_primary = config.base_url.contains("primary");
            let primary_calls = Arc::clone(&self.primary_calls);
            let secondary_calls = Arc::clone(&self.secondary_calls);
            Box::pin(async move {
                if is_primary {
                    primary_calls.fetch_add(1, Ordering::SeqCst);
                    Err(CoreError::Network {
                        detail: "connection refused".into(),
                    }
                    .into())
                } else {
                    secondary_calls.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(ProviderEvent::Text("served by secondary".into())).await;
                    let _ = tx.send(ProviderEvent::Finished(FinishReason::Stop)).await;
                    Ok(())
                }
            })
        }
    }

    #[tokio::test]
    async fn fallback_chain_fails_over_and_skips_open_circuit_on_next_turn() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let secondary_calls = Arc::new(AtomicUsize::new(0));
        let registry = ProviderRegistry::empty();
        registry.register(Arc::new(RoutingMockDriver {
            primary_calls: Arc::clone(&primary_calls),
            secondary_calls: Arc::clone(&secondary_calls),
        }));
        let health = EndpointHealthRegistry::new(CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_secs(60),
        });

        let mut primary = test_model(ApiFormat::OpenAiChat);
        primary.base_url = "https://primary.example/v1".into();
        let mut secondary = test_model(ApiFormat::OpenAiChat);
        secondary.base_url = "https://secondary.example/v1".into();

        // Turn 1: primary fails -> breaker trips open -> secondary succeeds.
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        stream_chat_with_fallback(
            &registry,
            &health,
            vec![primary.clone(), secondary.clone()],
            vec![],
            vec![],
            tx,
            CallControl::new(tokio_util::sync::CancellationToken::new()),
        )
        .await;
        let mut text = String::new();
        while let Some(ev) = rx.recv().await {
            if let ProviderEvent::Text(chunk) = ev {
                text.push_str(&chunk);
            }
        }
        assert_eq!(text, "served by secondary");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(secondary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            health.state(&EndpointHealthRegistry::endpoint_key(&primary)),
            CircuitState::Open
        );

        // Turn 2: primary circuit is open -> skipped directly -> secondary handles immediately!
        let (tx2, mut rx2) = tokio::sync::mpsc::channel(16);
        stream_chat_with_fallback(
            &registry,
            &health,
            vec![primary, secondary],
            vec![],
            vec![],
            tx2,
            CallControl::new(tokio_util::sync::CancellationToken::new()),
        )
        .await;
        let mut text2 = String::new();
        while let Some(ev) = rx2.recv().await {
            if let ProviderEvent::Text(chunk) = ev {
                text2.push_str(&chunk);
            }
        }
        assert_eq!(text2, "served by secondary");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1, "open primary must be skipped");
        assert_eq!(secondary_calls.load(Ordering::SeqCst), 2);
    }
}
