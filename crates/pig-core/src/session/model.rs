use super::*;

/// Session-level model override (SetModel)
#[derive(Clone, Debug, Default)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning_level: Option<String>,
}

/// Model selection persisted in SessionMeta → ModelSelection (missing
/// provider/model means no override)
pub(super) fn meta_to_selection(meta: &SessionMeta) -> Option<ModelSelection> {
    match (&meta.provider_id, &meta.model_id) {
        (Some(provider_id), Some(model_id)) => Some(ModelSelection {
            provider_id: provider_id.clone(),
            model_id: model_id.clone(),
            reasoning_level: meta.reasoning_level.clone(),
        }),
        _ => None,
    }
}

/// pig-provider's retry notice -> the protocol mirror the UI renders (plain
/// integer fields; Duration does not serde)
pub(super) fn retry_status(notice: &pig_provider::RetryNotice) -> pig_protocol::RetryStatus {
    use pig_protocol::{RetryReason, RetryStatus};
    let reason = match &notice.reason {
        pig_provider::RetryReason::RateLimit { retry_after } => RetryReason::RateLimit {
            retry_after_secs: retry_after.map(|d| d.as_secs()),
        },
        pig_provider::RetryReason::Server(code) => RetryReason::Server(*code),
        pig_provider::RetryReason::Network(detail) => RetryReason::Network {
            detail: detail.clone(),
        },
        pig_provider::RetryReason::EmptyCompletion => RetryReason::EmptyCompletion,
        pig_provider::RetryReason::InBand(detail) => RetryReason::InBand {
            detail: detail.clone(),
        },
    };
    RetryStatus {
        attempt: notice.attempt,
        max_attempts: notice.max_attempts,
        delay_ms: notice.delay.as_millis() as u64,
        reason,
    }
}

const TRUSTED_PROMPT_PREFIX: &str = "trusted:";
const UNTRUSTED_PROMPT_PREFIX: &str = "untrusted:";

/// The prompt-cache state sentinel for skills: the skills state file plus the
/// workspace trust decision that admits project skills.
///
/// `PromptSnapshot` carries this string (it is already the "did the skill
/// listing change" key), so a snapshot created under a different trust decision
/// is recognizable on resume and its project-derived prompt fields are not
/// restored.
pub(super) fn skills_state_with_trust(data_dir: &Path, trusted: bool) -> String {
    let state = std::fs::read_to_string(data_dir.join("skills-state.json")).unwrap_or_default();
    crate::plugins::skills_state(data_dir, trusted, state)
}

/// Trust decision recorded in a snapshot's skills state (`None` for legacy
/// snapshots that predate the binding; those rebuild the prompt as well).
pub(super) fn snapshot_trusted(state: &str) -> Option<bool> {
    if state.starts_with(TRUSTED_PROMPT_PREFIX) {
        Some(true)
    } else if state.starts_with(UNTRUSTED_PROMPT_PREFIX) {
        Some(false)
    } else {
        None
    }
}

/// Resolve a session model through the workspace's [`crate::config::RuntimeConfig`]:
/// the trusted project layer participates, `${ENV_VAR}` is expanded only here,
/// and the endpoint URL is normalized. A configuration that cannot produce a
/// model is typed: no enabled provider/model is `NoModelConfigured`, an unusable
/// provider entry is `ProviderConfig` (the item/message that needed the model
/// stays persisted at the call site).
pub(crate) fn resolve_runtime_model(
    runtime: Option<&crate::config::RuntimeConfig>,
    selection: Option<&ModelSelection>,
    default_level: Option<&str>,
) -> Result<ResolvedModel, CoreError> {
    let Some(runtime) = runtime else {
        return Err(CoreError::NoModelConfigured);
    };
    let request = selection.map(|selection| crate::config::ModelRequest {
        provider_id: Some(selection.provider_id.clone()),
        model_id: Some(selection.model_id.clone()),
        reasoning_level: selection.reasoning_level.clone(),
    });
    runtime
        .resolve_model(request.as_ref(), default_level)
        .map_err(|error| match error.kind() {
            crate::config::RuntimeConfigErrorKind::NoModelConfigured => {
                CoreError::NoModelConfigured
            }
            crate::config::RuntimeConfigErrorKind::Configuration => CoreError::ProviderConfig {
                detail: error.detail().to_string(),
            },
        })
}
