use serde::{Deserialize, Serialize};

use pig_protocol::ApiFormat;

use super::ResolvedModel;
use super::messages::ToolCallWire;

/// The endpoint/model/credential identity an assistant replay was produced
/// against. `base_url` stays a plain URL; `model` and `credential` are
/// explicit optional fields so records written before the identity existed
/// deserialize with `None` and degrade deterministically to the text/tool-only
/// projection. `credential` is a one-way fingerprint, never the key itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayOrigin {
    pub api_format: ApiFormat,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Stable SHA-256 fingerprint of the credential. This is a change detector,
/// not a secret: it is one-way and is never accompanied by the key itself (no
/// logs, rollout records or API bodies ever see the raw value).
pub(crate) fn credential_fingerprint(api_key: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(api_key.as_bytes());
    let mut out = String::with_capacity("sha256:".len() + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl ReplayOrigin {
    /// Build the full identity from already-resolved wire values. Test
    /// fixtures that seed durable replay records should use this instead of
    /// spelling a bare `base_url`, which is treated as an unverified legacy
    /// origin and therefore never replays natively.
    pub fn new(api_format: ApiFormat, base_url: &str, model: &str, api_key: &str) -> Self {
        Self {
            api_format,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: Some(model.to_string()),
            credential: Some(credential_fingerprint(api_key)),
        }
    }

    /// Build the full identity for one resolved model.
    pub fn for_model(config: &ResolvedModel) -> Self {
        Self::new(
            config.api_format,
            &config.base_url,
            &config.model,
            &config.api_key,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantReplay {
    pub origin: ReplayOrigin,
    pub items: Vec<AssistantReplayItem>,
}

impl AssistantReplay {
    pub fn new(config: &ResolvedModel, items: Vec<AssistantReplayItem>) -> Self {
        Self {
            origin: ReplayOrigin::for_model(config),
            items,
        }
    }

    pub fn origin_matches(&self, config: &ResolvedModel) -> bool {
        if self.origin.api_format != config.api_format
            || self.origin.base_url != config.base_url.trim_end_matches('/')
            // A legacy record has no identity fields and never matches, so
            // native replay degrades deterministically to text/tool-only.
            || self.origin.model.as_deref() != Some(config.model.as_str())
        {
            return false;
        }
        self.origin.credential.as_deref() == Some(credential_fingerprint(&config.api_key).as_str())
    }

    pub fn remove_tool_calls(&mut self) {
        self.items
            .retain(|item| !matches!(item, AssistantReplayItem::ToolCall { .. }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ResolvedModel {
        let mut config = crate::chat::test_model(ApiFormat::AnthropicMessages);
        config.base_url = "https://api.anthropic.com/v1/".into();
        config.api_key = "sk-ant-super-secret".into();
        config.model = "claude-sonnet-4-5".into();
        config
    }

    fn replay(config: &ResolvedModel) -> AssistantReplay {
        AssistantReplay::new(
            config,
            vec![AssistantReplayItem::Text {
                text: "answer".into(),
                citations: Vec::new(),
            }],
        )
    }

    #[test]
    fn origin_binds_endpoint_model_and_credential_change() {
        let config = model();
        let origin = replay(&config);
        assert!(origin.origin_matches(&config));

        let mut different_model = config.clone();
        different_model.model = "claude-opus-4-5".into();
        assert!(
            !origin.origin_matches(&different_model),
            "a model change must invalidate native replay"
        );

        let mut different_key = config.clone();
        different_key.api_key = "sk-ant-another-key".into();
        assert!(
            !origin.origin_matches(&different_key),
            "a credential change must invalidate native replay"
        );

        let mut different_endpoint = config.clone();
        different_endpoint.base_url = "https://gateway.example/v1".into();
        assert!(!origin.origin_matches(&different_endpoint));

        // Trailing slashes are cosmetic.
        let mut trailing = config.clone();
        trailing.base_url = "https://api.anthropic.com/v1///".into();
        assert!(origin.origin_matches(&trailing));
    }

    #[test]
    fn origin_serializes_only_a_fingerprint() {
        let config = model();
        let serialized = serde_json::to_string(&replay(&config)).expect("serialize replay");
        assert!(
            !serialized.contains("sk-ant-super-secret"),
            "raw API keys must never reach durable replay: {serialized}"
        );
        assert!(serialized.contains("sha256:"));
        assert!(serialized.contains("claude-sonnet-4-5"));
        assert!(
            serialized.contains("\"model\":\"claude-sonnet-4-5\""),
            "the model is an explicit origin field: {serialized}"
        );
        assert!(
            serialized.contains("\"credential\":\"sha256:"),
            "the credential is an explicit fingerprint field: {serialized}"
        );
        assert!(
            !serialized.contains("pig-origin-v2"),
            "the base URL is no longer a JSON envelope: {serialized}"
        );

        let restored: AssistantReplay = serde_json::from_str(&serialized).expect("restore replay");
        assert_eq!(restored, replay(&config));
        assert!(
            restored.origin_matches(&config),
            "same identity survives a restart round-trip"
        );
    }

    /// Records from before the explicit identity fields (plain base URL or the
    /// old `pig-origin-v2:` envelope) deserialize with `None` model/credential
    /// and must degrade to the text/tool-only projection.
    #[test]
    fn legacy_origins_never_native_replay() {
        let config = model();
        let legacy = AssistantReplay {
            origin: ReplayOrigin {
                api_format: config.api_format,
                base_url: config.base_url.trim_end_matches('/').to_string(),
                model: None,
                credential: None,
            },
            items: vec![AssistantReplayItem::Text {
                text: "answer".into(),
                citations: Vec::new(),
            }],
        };
        assert!(
            !legacy.origin_matches(&config),
            "an origin without identity must degrade to the safe projection"
        );

        let encoded: AssistantReplay = serde_json::from_value(serde_json::json!({
            "origin": {
                "api_format": config.api_format,
                "base_url": "https://api.anthropic.com/v1",
            },
            "items": [],
        }))
        .expect("legacy records deserialize");
        assert_eq!(encoded.origin.model, None);
        assert_eq!(encoded.origin.credential, None);
        assert!(!encoded.origin_matches(&config));

        let envelope = ReplayOrigin {
            api_format: config.api_format,
            base_url: format!(
                "pig-origin-v2:{{\"base_url\":\"https://api.anthropic.com/v1\",\"model\":\"{}\",\"credential\":\"{}\"}}",
                config.model,
                credential_fingerprint(&config.api_key)
            ),
            model: None,
            credential: None,
        };
        let envelope = AssistantReplay {
            origin: envelope,
            items: vec![],
        };
        assert!(
            !envelope.origin_matches(&config),
            "the old envelope is not reinterpreted"
        );
    }

    #[test]
    fn credential_fingerprint_is_stable_and_distinguishes_keys() {
        assert_eq!(
            credential_fingerprint("key-a"),
            credential_fingerprint("key-a")
        );
        assert_ne!(
            credential_fingerprint("key-a"),
            credential_fingerprint("key-b")
        );
        assert_ne!(credential_fingerprint(""), credential_fingerprint("key-a"));
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantReplayItem {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        citations: Vec<TextCitation>,
    },
    ToolCall {
        call: ToolCallWire,
    },
    ResponsesReasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summary: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    /// Responses hosted `web_search_call` item. `encrypted_content` is the
    /// provider's opaque replay credential and stays out of normalized sources.
    ResponsesWebSearchCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// The native `action` object (type/query/queries/sources), replayed verbatim
        #[serde(default)]
        action: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    /// Anthropic `server_tool_use` block (hosted web search). Its input is
    /// accumulated separately from local tool calls and never executed.
    AnthropicServerToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    /// Anthropic `web_search_tool_result` block; `content` is the native result
    /// array or error object, replayed as-is on the official endpoint.
    AnthropicWebSearchResult {
        tool_use_id: String,
        #[serde(default)]
        content: serde_json::Value,
    },
    AnthropicThinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    AnthropicRedactedThinking {
        data: String,
    },
}

/// One typed text citation. The URL/title/span fields are display data;
/// `encrypted_index` and `cited_text` are Anthropic replay credentials and are
/// never copied into `pig_protocol::Source`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextCitation {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_index: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_text: Option<String>,
}
