use pig_provider::{ProviderEvent, ProviderResponseMetadata, ProviderUsage, ResolvedModel};

/// One logical model call's durable reporting fields. Transport and silent
/// retries stay in this row rather than becoming separate billable calls.
#[derive(Default)]
pub(crate) struct ApiUsageRecord {
    pub session_id: String,
    pub source: String,
    pub turn_id: Option<String>,
    pub step: Option<u32>,
    pub agent_id: Option<String>,
    pub background: bool,
    pub provider: String,
    pub model: String,
    pub api_format: String,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub usage_reported: bool,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub used_tokens: u64,
    pub context_window: u64,
    pub finish: String,
    pub error: Option<String>,
    pub retry_count: u32,
    pub response: ProviderResponseMetadata,
}

impl ApiUsageRecord {
    pub(crate) fn new(
        session_id: &str,
        source: &str,
        config: &ResolvedModel,
        started_at_ms: u64,
        duration_ms: u64,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            source: source.to_string(),
            provider: config.provider_name.clone(),
            model: config.model.clone(),
            api_format: format!("{:?}", config.api_format),
            started_at_ms,
            duration_ms,
            ..Self::default()
        }
    }

    pub(crate) fn set_usage(&mut self, usage: Option<ProviderUsage>) {
        let Some(usage) = usage else {
            return;
        };
        self.usage_reported = true;
        self.input_tokens = usage.input;
        self.cache_read_tokens = usage.cache_read;
        self.output_tokens = usage.output;
        self.reasoning_output_tokens = usage.reasoning_output;
        self.used_tokens = usage.used;
        self.context_window = usage.total;
    }

    pub(crate) fn from_sidecar(
        session_id: &str,
        source: &str,
        config: &ResolvedModel,
        started_at_ms: u64,
        duration_ms: u64,
        outcome: Result<pig_provider::CompleteCall, Box<pig_provider::CompleteCallError>>,
    ) -> (Self, Result<String, pig_protocol::CoreError>) {
        match outcome {
            Ok(call) => {
                let pig_provider::CompleteCall {
                    text,
                    usage,
                    response,
                } = call;
                let mut record = Self::new(session_id, source, config, started_at_ms, duration_ms);
                record.finish = "stop".to_string();
                record.set_usage(usage);
                record.response = response;
                (record, Ok(text))
            }
            Err(call) => {
                let pig_provider::CompleteCallError {
                    error,
                    usage,
                    response,
                } = *call;
                let mut record = Self::new(session_id, source, config, started_at_ms, duration_ms);
                record.finish = "error".to_string();
                record.error = Some(pig_provider::core_error_en(&error));
                record.set_usage(usage);
                record.response = response;
                (record, Err(error))
            }
        }
    }
}

/// Streaming-call accumulator shared by the root session and subagents.
/// Token counts are additive across attempts that actually reported usage;
/// response metadata describes only the latest non-retry attempt.
#[derive(Default)]
pub(crate) struct ApiCallTracker {
    usage: ProviderUsage,
    usage_reported: bool,
    response: ProviderResponseMetadata,
    retry_count: u32,
}

impl ApiCallTracker {
    pub(crate) fn observe(&mut self, event: &ProviderEvent) {
        match event {
            ProviderEvent::Usage {
                input,
                cache_read,
                output,
                used,
                total,
                reasoning_output,
            } => {
                self.usage_reported = true;
                self.usage.input += input;
                self.usage.cache_read += cache_read;
                self.usage.output += output;
                self.usage.reasoning_output += reasoning_output;
                self.usage.used = *used;
                self.usage.total = *total;
            }
            ProviderEvent::ResponseMetadata(response) => {
                self.response = response.clone();
            }
            ProviderEvent::Retrying(_) => {
                self.retry_count = self.retry_count.saturating_add(1);
                self.response = ProviderResponseMetadata::default();
            }
            ProviderEvent::Reasoning(_)
            | ProviderEvent::Text(_)
            | ProviderEvent::AssistantReplay(_)
            | ProviderEvent::ToolCalls(_)
            | ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::Finished(_)
            | ProviderEvent::Failed(_) => {}
        }
    }

    pub(crate) fn apply_to(self, record: &mut ApiUsageRecord) {
        record.set_usage(self.usage_reported.then_some(self.usage));
        record.retry_count = self.retry_count;
        record.response = self.response;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_provider::{RetryNotice, RetryReason};

    #[test]
    fn tracker_sums_usage_and_keeps_latest_response() {
        let mut tracker = ApiCallTracker::default();
        for input in [10, 20] {
            tracker.observe(&ProviderEvent::Usage {
                input,
                cache_read: 2,
                output: 3,
                used: input + 5,
                total: 100,
                reasoning_output: 1,
            });
        }
        tracker.observe(&ProviderEvent::ResponseMetadata(ProviderResponseMetadata {
            request_id: Some("request-1".into()),
            ..ProviderResponseMetadata::default()
        }));
        tracker.observe(&ProviderEvent::Retrying(RetryNotice {
            attempt: 1,
            max_attempts: 1,
            delay: std::time::Duration::ZERO,
            reason: RetryReason::EmptyCompletion,
        }));
        assert_eq!(tracker.retry_count, 1);
        assert_eq!(tracker.response, ProviderResponseMetadata::default());

        tracker.observe(&ProviderEvent::ResponseMetadata(ProviderResponseMetadata {
            request_id: Some("request-2".into()),
            ..ProviderResponseMetadata::default()
        }));
        let mut record = ApiUsageRecord::default();
        tracker.apply_to(&mut record);
        assert!(record.usage_reported);
        assert_eq!(record.input_tokens, 30);
        assert_eq!(record.cache_read_tokens, 4);
        assert_eq!(record.output_tokens, 6);
        assert_eq!(record.reasoning_output_tokens, 2);
        assert_eq!(record.used_tokens, 25);
        assert_eq!(record.retry_count, 1);
        assert_eq!(record.response.request_id.as_deref(), Some("request-2"));
    }

    #[test]
    fn missing_usage_stays_distinct_from_zero() {
        let mut record = ApiUsageRecord::default();
        assert!(!record.usage_reported);
        record.set_usage(Some(ProviderUsage::default()));
        assert!(record.usage_reported);
        assert_eq!(record.input_tokens, 0);
    }
}
