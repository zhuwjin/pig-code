use super::*;

impl Session {
    pub(crate) fn record_output_delta(&mut self, delta: &str) {
        if self.rate_started_at.is_none() {
            self.rate_started_at = Some(Instant::now());
            self.rate_characters = 0;
        }
        self.rate_characters += delta.encode_utf16().count() as u64;
    }

    fn output_rate(&self) -> u64 {
        let Some(started) = self.rate_started_at else {
            return 0;
        };
        let elapsed = started.elapsed().as_secs_f64().max(0.05);
        (self.rate_characters as f64 / 4.0 / elapsed)
            .round()
            .max(0.0) as u64
    }

    pub(crate) fn plugin_stats(
        &self,
    ) -> (
        pig_protocol::PluginSessionStats,
        Vec<pig_protocol::PluginTurnStats>,
    ) {
        let mut turns = self.stats_turns.clone();
        if self.turn_active {
            turns.push(pig_protocol::PluginTurnStats {
                id: self.usage_turn_id.clone(),
                index: turns.len() as u64,
                duration_ms: self.turn_api_ms,
                stats: Some(pig_protocol::TurnUsageStats {
                    input: self.turn_input,
                    cache_read: self.turn_cache_read,
                    output: self.turn_output,
                    duration_ms: self.turn_api_ms,
                    api_ms: self.turn_api_ms,
                    ttft_ms: self.turn_ttft_ms,
                    ttft_steps: self.turn_ttft_steps,
                    api_steps: self.turn_api_steps,
                    tool_ms: self.turn_tool_ms,
                    ended_at_ms: 0,
                }),
            });
        }
        let mut stats = pig_protocol::PluginSessionStats {
            running: self.turn_active,
            output_rate: self.output_rate(),
            turns: self
                .stats_turns
                .iter()
                .filter(|turn| turn.duration_ms > 0 || turn.stats.is_some())
                .count() as u64,
            usage: self.last_total_tokens.map(|used| {
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
                    cache_read_total: self.cache_read_total,
                    input_total: self.input_total,
                }
            }),
            breakdown: Some(super::estimate::context_breakdown(
                &self.agent.history,
                &self.root_schemas(),
            )),
            ..Default::default()
        };
        for turn in &self.stats_turns {
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
