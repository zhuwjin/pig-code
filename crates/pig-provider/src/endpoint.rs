//! Exact-host endpoint capability classification.
//!
//! These are pure functions on top of the configured base URL. Core calls them
//! while resolving a model and stores the resulting switches on
//! [`crate::ResolvedModel`]; the encoders only read those switches, so local
//! mock tests can exercise official-only behavior without faking a host.
//!
//! The allowlists are deliberately conservative: a missing host means the
//! capability is off, never inferred from the vendor name. MiniMax is not on
//! either list: its Anthropic-compatible endpoint is not documented to accept
//! `cache_control`, and its OpenAI-compatible endpoint does not document
//! `prompt_cache_key`.

use pig_protocol::ApiFormat;

/// Whether the active wire format injects a provider-hosted search tool.
pub fn native_web_search_enabled(config: &crate::ResolvedModel) -> bool {
    match config.api_format {
        ApiFormat::OpenAiChat => false,
        ApiFormat::OpenAiResponses => crate::responses_web_search_tool(config).is_some(),
        ApiFormat::AnthropicMessages => crate::anthropic_web_search_tool(config).is_some(),
    }
}

/// Join a configured base URL with one endpoint path. This is the only
/// concatenation point for live, sidecar and connectivity requests, so a
/// trailing `/` (or several) behaves identically on every path.
pub(crate) fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// `POST {base}/chat/completions`.
pub(crate) fn chat_completions_url(base: &str) -> String {
    join_url(base, "chat/completions")
}

/// `POST {base}/responses`.
pub(crate) fn responses_url(base: &str) -> String {
    join_url(base, "responses")
}

/// `POST {base}/v1/messages`, except that a base already ending in `/v1`
/// keeps its version segment.
pub(crate) fn anthropic_messages_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") {
        join_url(base, "messages")
    } else {
        join_url(base, "v1/messages")
    }
}

fn host(base_url: &str) -> Option<String> {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

fn host_matches(base_url: &str, hosts: &[&str]) -> bool {
    host(base_url).is_some_and(|host| hosts.contains(&host.as_str()))
}

/// `api.openai.com` (the official first-party endpoint).
pub fn is_official_openai(base_url: &str) -> bool {
    host_matches(base_url, &["api.openai.com"])
}

/// Whether the endpoint supports OpenAI Responses WebSocket v2 (`ws://` /
/// `wss://` explicitly, or the official OpenAI API host which upgrades from
/// `https://api.openai.com` to `wss://api.openai.com/v1/responses`).
pub fn responses_ws_supported(base_url: &str) -> bool {
    base_url.starts_with("ws://")
        || base_url.starts_with("wss://")
        || is_official_openai(base_url)
}

/// `api.anthropic.com` (the official first-party endpoint).
pub fn is_official_anthropic(base_url: &str) -> bool {
    host_matches(base_url, &["api.anthropic.com"])
}

/// Chat Completions token-limit field: the official endpoint requires the
/// newer `max_completion_tokens`; compatible endpoints keep `max_tokens`.
pub fn chat_token_limit_key(base_url: &str) -> &'static str {
    if is_official_openai(base_url) {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

/// Hosts whose Anthropic-compatible endpoint accepts explicit
/// `cache_control` breakpoints. Only exact hosts, never a vendor substring.
const ANTHROPIC_CACHE_HOSTS: &[&str] = &[
    "api.anthropic.com",
    "api.kimi.com",
    "api.moonshot.ai",
    "api.moonshot.cn",
    "api.z.ai",
    "open.bigmodel.cn",
    "api.deepseek.com",
];

/// Whether explicit Anthropic prompt-cache breakpoints are safe to send.
pub fn anthropic_cache_supported(base_url: &str) -> bool {
    host_matches(base_url, ANTHROPIC_CACHE_HOSTS)
}

/// Hosts whose OpenAI-compatible endpoint accepts `prompt_cache_key`.
const PROMPT_CACHE_KEY_HOSTS: &[&str] = &[
    "api.openai.com",
    "api.kimi.com",
    "api.moonshot.ai",
    "api.moonshot.cn",
    "api.deepseek.com",
    "api.z.ai",
    "open.bigmodel.cn",
];

/// Whether the session-routing `prompt_cache_key` is safe to send.
pub fn prompt_cache_key_supported(base_url: &str) -> bool {
    host_matches(base_url, PROMPT_CACHE_KEY_HOSTS)
}

/// Whether native hosted-search items may be replayed on the wire. Only the
/// first-party endpoints are trusted with their own opaque search blocks;
/// compatible endpoints replay the text deterministically while the stored
/// replay still carries the native sources for the UI.
pub fn native_search_replay_supported(format: ApiFormat, base_url: &str) -> bool {
    match format {
        ApiFormat::OpenAiResponses => is_official_openai(base_url),
        ApiFormat::AnthropicMessages => is_official_anthropic(base_url),
        ApiFormat::OpenAiChat => false,
    }
}

/// Stable per-session cache-routing key. Retries and restarts reuse it because
/// it depends only on the session id; subagents derive their own key from the
/// parent session and agent id.
pub fn session_prompt_cache_key(session_id: &str) -> String {
    format!("pig:{session_id}")
}

/// Stable cache-routing key for a subagent: it must not reuse the parent
/// session's key, and a resumed agent keeps the same agent id.
pub fn subagent_prompt_cache_key(session_id: &str, agent_id: &str) -> String {
    format!("pig:{session_id}:{agent_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_join_normalizes_every_trailing_slash_shape() {
        for base in [
            "http://127.0.0.1:8080/v1",
            "http://127.0.0.1:8080/v1/",
            "http://127.0.0.1:8080/v1///",
        ] {
            assert_eq!(
                chat_completions_url(base),
                "http://127.0.0.1:8080/v1/chat/completions"
            );
            assert_eq!(responses_url(base), "http://127.0.0.1:8080/v1/responses");
            assert_eq!(
                anthropic_messages_url(base),
                "http://127.0.0.1:8080/v1/messages"
            );
        }
        assert_eq!(
            anthropic_messages_url("https://api.deepseek.com/anthropic/"),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            join_url("http://host/root/", "/a/b"),
            "http://host/root/a/b"
        );
    }

    #[test]
    fn official_hosts_are_exact_and_case_insensitive() {
        assert!(is_official_openai("https://api.openai.com/v1"));
        assert!(is_official_openai("HTTPS://API.OPENAI.COM/v1/"));
        assert!(is_official_anthropic("https://api.anthropic.com"));
        assert!(!is_official_openai(
            "https://api.openai.com.evil.example/v1"
        ));
        assert!(!is_official_openai("http://127.0.0.1:8080/v1"));
        assert!(!is_official_anthropic(
            "https://api.anthropic.com.evil.example"
        ));
    }

    #[test]
    fn chat_token_field_depends_on_the_official_host() {
        assert_eq!(
            chat_token_limit_key("https://api.openai.com/v1"),
            "max_completion_tokens"
        );
        assert_eq!(
            chat_token_limit_key("https://api.deepseek.com"),
            "max_tokens"
        );
        assert_eq!(
            chat_token_limit_key("http://127.0.0.1:1234/v1"),
            "max_tokens"
        );
    }

    #[test]
    fn cache_allowlists_are_exact_and_unknown_hosts_stay_off() {
        for host in [
            "https://api.anthropic.com",
            "https://api.moonshot.cn/anthropic",
            "https://open.bigmodel.cn/api/anthropic",
            "https://api.z.ai/api/anthropic",
            "https://api.deepseek.com/anthropic",
        ] {
            assert!(anthropic_cache_supported(host), "{host}");
        }
        for host in [
            "https://api.openai.com/v1",
            "https://openrouter.ai/api/v1",
            "https://api.minimax.io/anthropic",
            "https://api.minimaxi.com/anthropic",
            "http://127.0.0.1:8080",
            "https://api.kimi.com.evil.example",
        ] {
            assert!(!anthropic_cache_supported(host), "{host}");
        }

        for host in [
            "https://api.openai.com/v1",
            "https://api.deepseek.com",
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
        ] {
            assert!(prompt_cache_key_supported(host), "{host}");
        }
        for host in [
            "https://api.anthropic.com",
            "https://openrouter.ai/api/v1",
            "https://api.minimax.io",
            "https://api.minimaxi.com",
            "http://127.0.0.1:8080",
        ] {
            assert!(!prompt_cache_key_supported(host), "{host}");
        }
    }

    #[test]
    fn native_search_replay_is_first_party_only() {
        assert!(native_search_replay_supported(
            ApiFormat::OpenAiResponses,
            "https://api.openai.com/v1"
        ));
        assert!(native_search_replay_supported(
            ApiFormat::AnthropicMessages,
            "https://api.anthropic.com"
        ));
        assert!(!native_search_replay_supported(
            ApiFormat::OpenAiChat,
            "https://api.openai.com/v1"
        ));
        assert!(!native_search_replay_supported(
            ApiFormat::OpenAiResponses,
            "http://127.0.0.1:8080/v1"
        ));
        assert!(!native_search_replay_supported(
            ApiFormat::AnthropicMessages,
            "https://api.deepseek.com/anthropic"
        ));
    }

    #[test]
    fn native_web_search_matches_actual_tool_injection() {
        for format in [
            ApiFormat::OpenAiChat,
            ApiFormat::OpenAiResponses,
            ApiFormat::AnthropicMessages,
        ] {
            let mut model = crate::chat::test_model(format);
            model.cap_web_search = true;
            assert_eq!(
                native_web_search_enabled(&model),
                format != ApiFormat::OpenAiChat,
                "{format:?}"
            );
            model.cap_web_search = false;
            assert!(!native_web_search_enabled(&model), "{format:?}");
        }
    }

    #[test]
    fn session_key_is_stable_and_namespaced() {
        assert_eq!(session_prompt_cache_key("s-1"), "pig:s-1");
        assert_eq!(
            session_prompt_cache_key("s-1"),
            session_prompt_cache_key("s-1")
        );
        assert_ne!(
            session_prompt_cache_key("s-1"),
            session_prompt_cache_key("s-2")
        );
        assert_eq!(subagent_prompt_cache_key("s-1", "a-1"), "pig:s-1:a-1");
        assert_ne!(
            subagent_prompt_cache_key("s-1", "a-1"),
            session_prompt_cache_key("s-1"),
            "a subagent never reuses the parent key"
        );
    }
}
