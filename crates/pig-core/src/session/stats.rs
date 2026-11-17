use super::*;
use std::time::Instant;

impl Session {
    pub(crate) fn record_output_delta(&mut self, delta: &str) {
        if self.run_state.rate_started_at.is_none() {
            self.run_state.rate_started_at = Some(Instant::now());
            self.run_state.rate_characters = 0;
        }
        self.run_state.rate_characters += delta.encode_utf16().count() as u64;
    }

    fn output_rate(&self) -> u64 {
        let Some(started) = self.run_state.rate_started_at else {
            return 0;
        };
        let elapsed = started.elapsed().as_secs_f64().max(0.05);
        (self.run_state.rate_characters as f64 / 4.0 / elapsed)
            .round()
            .max(0.0) as u64
    }

    pub(crate) fn plugin_stats(
        &self,
    ) -> (
        pig_protocol::PluginSessionStats,
        Vec<pig_protocol::PluginTurnStats>,
    ) {
        let mut turns = self.run_state.history.clone();
        if self.run_state.active {
            turns.push(pig_protocol::PluginTurnStats {
                id: self.run_state.usage_turn_id.clone(),
                index: turns.len() as u64,
                duration_ms: self.run_state.api_ms,
                stats: Some(pig_protocol::TurnUsageStats {
                    input: self.run_state.input,
                    cache_read: self.run_state.cache_read,
                    output: self.run_state.output,
                    duration_ms: self.run_state.api_ms,
                    api_ms: self.run_state.api_ms,
                    ttft_ms: self.run_state.ttft_ms,
                    ttft_steps: self.run_state.ttft_steps,
                    api_steps: self.run_state.api_steps,
                    tool_ms: self.run_state.tool_ms,
                    ended_at_ms: 0,
                }),
            });
        }
        let mut stats = pig_protocol::PluginSessionStats {
            running: self.run_state.active,
            output_rate: self.output_rate(),
            turns: self
                .run_state
                .history
                .iter()
                .filter(|turn| turn.duration_ms > 0 || turn.stats.is_some())
                .count() as u64,
            usage: self.run_state.last_total_tokens.map(|used| {
                let config = self.runtime.as_deref().map(|runtime| runtime.config());
                let provider = self
                    .model_override
                    .as_ref()
                    .map(|selection| selection.provider_id.as_str())
                    .or_else(|| config.map(|config| config.default_provider.as_str()));
                let model = self
                    .model_override
                    .as_ref()
                    .map(|selection| selection.model_id.as_str())
                    .or_else(|| config.map(|config| config.default_model.as_str()));
                let total = config
                    .and_then(|config| {
                        config
                            .providers
                            .iter()
                            .find(|candidate| Some(candidate.id.as_str()) == provider)
                    })
                    .and_then(|provider| {
                        provider
                            .models
                            .iter()
                            .find(|candidate| Some(candidate.id.as_str()) == model)
                    })
                    .map(|model| model.context_window)
                    .unwrap_or(128_000);
                pig_protocol::PluginContextUsage {
                    used,
                    total,
                    cache_read_total: self.run_state.cache_read_total,
                    input_total: self.run_state.input_total,
                }
            }),
            breakdown: Some(super::estimate::context_breakdown(
                &self.agent.history,
                &self.root_schemas(),
            )),
            ..Default::default()
        };
        for turn in &self.run_state.history {
            let Some(value) = turn.stats else {
                continue;
            };
            stats.steps += value.api_steps;
            stats.api_ms += value.api_ms;
            stats.tool_ms += value.tool_ms;
            stats.ttft_ms += value.ttft_ms;
            stats.ttft_steps += value.ttft_steps;
            let decode_ms = value.api_ms.saturating_sub(value.ttft_ms);
            if value.ttft_ms > 0 && decode_ms > 0 {
                stats.output += value.output;
                stats.decode_ms += decode_ms;
            }
        }
        (stats, turns)
    }
}
