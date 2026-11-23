use super::*;

/// Baseline for the per-step context preflight. `history_len`/`tools_tokens`
/// identify the request shape that produced `used`; only messages and schema
/// growth after that point are added back.
#[derive(Clone, Debug)]
pub(crate) struct UsageAnchor {
    pub model_key: String,
    pub history_len: usize,
    pub tools_tokens: u64,
    pub used: u64,
    /// Fingerprint of the leading system messages; a re-rendered system prompt
    /// (skills/memory/access changes) invalidates the old usage baseline even
    /// when the model id is unchanged.
    pub system_fingerprint: u64,
}

/// Identity of a resolved model for anchor invalidation (a model switch must
/// not project the previous model's usage onto the new one).
pub(crate) fn model_key(config: &ResolvedModel) -> String {
    format!(
        "{}|{}|{}|{:?}",
        config.provider_name, config.model, config.base_url, config.api_format
    )
}

/// Content fingerprint of the leading system prompt/compaction notes. Using
/// the rendered text (not just its token estimate) also catches equal-sized
/// rewrites from skills/memory/access changes.
pub(crate) fn system_fingerprint(history: &[ChatMsg]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for message in history
        .iter()
        .take_while(|message| message.role == "system")
    {
        message.role.hash(&mut hasher);
        message.content.as_deref().unwrap_or("").hash(&mut hasher);
    }
    hasher.finish()
}

impl Session {
    /// Persist the turn's accumulated usage (rollout TurnStats record for
    /// replay) and build the UI footer stats. Called on both the normal turn
    /// end and the interrupt/failure wrap-up so interrupted turns still count;
    /// the rollout record doubles as the "counted" marker for the
    /// crash-recovery scan in Session::load. The in-flight request's usage is
    /// unrecoverable either way (usage arrives at the stream's end). Returns
    /// None when no request of the turn reported usage.
    pub(super) fn flush_turn_stats(
        &mut self,
        duration_ms: u64,
        finish_reason: Option<pig_protocol::FinishReason>,
    ) -> Option<pig_protocol::TurnUsageStats> {
        self.run_state.active = false;
        let has_usage =
            self.run_state.input + self.run_state.cache_read + self.run_state.output > 0;
        if !has_usage && finish_reason.is_none() {
            return None;
        }
        let ended_at_ms = crate::model_io::now_ms();
        // An incomplete finish is durable even when the provider omitted usage.
        self.record(&RolloutRecord::TurnStats {
            input: self.run_state.input,
            cache_read: self.run_state.cache_read,
            output: self.run_state.output,
            duration_ms,
            ended_at_ms,
            api_ms: self.run_state.api_ms,
            ttft_ms: self.run_state.ttft_ms,
            ttft_steps: self.run_state.ttft_steps,
            api_steps: self.run_state.api_steps,
            tool_ms: self.run_state.tool_ms,
            has_usage,
            finish_reason,
            turn_id: self.run_state.usage_turn_id.clone(),
        });
        let stats = has_usage.then_some(pig_protocol::TurnUsageStats {
            input: self.run_state.input,
            cache_read: self.run_state.cache_read,
            output: self.run_state.output,
            duration_ms,
            api_ms: self.run_state.api_ms,
            ttft_ms: self.run_state.ttft_ms,
            ttft_steps: self.run_state.ttft_steps,
            api_steps: self.run_state.api_steps,
            tool_ms: self.run_state.tool_ms,
            ended_at_ms,
        });
        self.run_state.history.push(pig_protocol::PluginTurnStats {
            id: self.run_state.usage_turn_id.clone(),
            index: self.run_state.history.len() as u64,
            duration_ms,
            stats: stats.clone(),
        });
        stats
    }
}
