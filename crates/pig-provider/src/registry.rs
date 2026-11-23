//! Extensible trait-based provider protocol registry.
//!
//! Replaces the closed 3-variant `ApiFormat` match inside `stream_chat` with an
//! open [`ProviderDriver`] trait + [`ProviderRegistry`]. Built-in drivers cover
//! OpenAI Responses (WebSocket v2 and HTTP/SSE), OpenAI Chat Completions, and
//! Anthropic Messages; callers can register additional protocol drivers (such
//! as Gemini native, AWS Bedrock, or custom gateways) without modifying
//! `pig-provider`'s core streaming loop.

use std::sync::{Arc, OnceLock, RwLock};

use futures_util::future::BoxFuture;
use pig_protocol::{ApiFormat, CoreError};

use crate::chat::{ChatMsg, ResolvedModel};
use crate::events::{CallControl, CallError, ProviderEvent};
use crate::{anthropic, openai, responses};

/// A single wire-protocol driver capable of streaming a model turn.
pub trait ProviderDriver: Send + Sync {
    /// Stable driver identifier (for diagnostics and registry overrides).
    fn id(&self) -> &str;

    /// Whether this driver should handle `config`. Earlier registered drivers
    /// take priority over later ones.
    fn handles(&self, config: &ResolvedModel) -> bool;

    /// Execute one streaming attempt, emitting live [`ProviderEvent`]s to `tx`.
    fn stream<'a>(
        &'a self,
        config: &'a ResolvedModel,
        messages: Vec<ChatMsg>,
        tools: Vec<serde_json::Value>,
        tx: &'a tokio::sync::mpsc::Sender<ProviderEvent>,
        control: &'a CallControl,
    ) -> BoxFuture<'a, Result<(), CallError>>;
}

/// Thread-safe registry of [`ProviderDriver`] implementations.
pub struct ProviderRegistry {
    drivers: RwLock<Vec<Arc<dyn ProviderDriver>>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl ProviderRegistry {
    /// Empty registry (useful for isolated unit tests).
    pub fn empty() -> Self {
        Self {
            drivers: RwLock::new(Vec::new()),
        }
    }

    /// Standard registry populated with all built-in protocol drivers.
    pub fn with_builtins() -> Self {
        Self {
            drivers: RwLock::new(vec![
                Arc::new(OpenAiResponsesWsDriver),
                Arc::new(OpenAiResponsesHttpDriver),
                Arc::new(OpenAiChatDriver),
                Arc::new(AnthropicMessagesDriver),
            ]),
        }
    }

    /// Process-wide default registry used by [`crate::stream_chat`].
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<ProviderRegistry> = OnceLock::new();
        GLOBAL.get_or_init(Self::with_builtins)
    }

    /// Register a driver at the front of the lookup chain so custom protocol
    /// drivers can extend or override built-ins cleanly.
    pub fn register(&self, driver: Arc<dyn ProviderDriver>) {
        let mut guard = self
            .drivers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.retain(|existing| existing.id() != driver.id());
        guard.insert(0, driver);
    }

    /// Resolve the first driver matching `config`.
    pub fn resolve(&self, config: &ResolvedModel) -> Option<Arc<dyn ProviderDriver>> {
        self.drivers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|driver| driver.handles(config))
            .cloned()
    }

    /// List registered driver IDs in resolution priority order.
    pub fn driver_ids(&self) -> Vec<String> {
        self.drivers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|driver| driver.id().to_string())
            .collect()
    }
}

/// Built-in driver: OpenAI Responses over WebSocket v2 (`ws://` or `wss://`).
pub struct OpenAiResponsesWsDriver;

impl ProviderDriver for OpenAiResponsesWsDriver {
    fn id(&self) -> &str {
        "openai.responses.websocket"
    }

    fn handles(&self, config: &ResolvedModel) -> bool {
        config.api_format == ApiFormat::OpenAiResponses
            && (config.base_url.starts_with("ws://") || config.base_url.starts_with("wss://"))
    }

    fn stream<'a>(
        &'a self,
        config: &'a ResolvedModel,
        messages: Vec<ChatMsg>,
        tools: Vec<serde_json::Value>,
        tx: &'a tokio::sync::mpsc::Sender<ProviderEvent>,
        control: &'a CallControl,
    ) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            responses::stream_responses_ws(config, messages, tools, None, tx, control).await
        })
    }
}

/// Built-in driver: OpenAI Responses over HTTP/SSE.
pub struct OpenAiResponsesHttpDriver;

impl ProviderDriver for OpenAiResponsesHttpDriver {
    fn id(&self) -> &str {
        "openai.responses.http"
    }

    fn handles(&self, config: &ResolvedModel) -> bool {
        config.api_format == ApiFormat::OpenAiResponses
    }

    fn stream<'a>(
        &'a self,
        config: &'a ResolvedModel,
        messages: Vec<ChatMsg>,
        tools: Vec<serde_json::Value>,
        tx: &'a tokio::sync::mpsc::Sender<ProviderEvent>,
        control: &'a CallControl,
    ) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            responses::stream_responses(config, messages, tools, tx, control).await
        })
    }
}

/// Built-in driver: OpenAI Chat Completions over HTTP/SSE.
pub struct OpenAiChatDriver;

impl ProviderDriver for OpenAiChatDriver {
    fn id(&self) -> &str {
        "openai.chat"
    }

    fn handles(&self, config: &ResolvedModel) -> bool {
        config.api_format == ApiFormat::OpenAiChat
    }

    fn stream<'a>(
        &'a self,
        config: &'a ResolvedModel,
        messages: Vec<ChatMsg>,
        tools: Vec<serde_json::Value>,
        tx: &'a tokio::sync::mpsc::Sender<ProviderEvent>,
        control: &'a CallControl,
    ) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move { openai::stream_openai(config, messages, tools, tx, control).await })
    }
}

/// Built-in driver: Anthropic Messages over HTTP/SSE.
pub struct AnthropicMessagesDriver;

impl ProviderDriver for AnthropicMessagesDriver {
    fn id(&self) -> &str {
        "anthropic.messages"
    }

    fn handles(&self, config: &ResolvedModel) -> bool {
        config.api_format == ApiFormat::AnthropicMessages
    }

    fn stream<'a>(
        &'a self,
        config: &'a ResolvedModel,
        messages: Vec<ChatMsg>,
        tools: Vec<serde_json::Value>,
        tx: &'a tokio::sync::mpsc::Sender<ProviderEvent>,
        control: &'a CallControl,
    ) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            anthropic::stream_anthropic(config, messages, tools, tx, control).await
        })
    }
}

pub(crate) fn no_driver_error(config: &ResolvedModel) -> CallError {
    CoreError::Internal {
        detail: format!(
            "No provider driver registered for api_format={:?}, base_url={}",
            config.api_format, config.base_url
        ),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::test_model;
    use pig_protocol::FinishReason;

    struct CustomGeminiDriver;

    impl ProviderDriver for CustomGeminiDriver {
        fn id(&self) -> &str {
            "custom.gemini"
        }

        fn handles(&self, config: &ResolvedModel) -> bool {
            config.base_url.contains("generativelanguage.googleapis.com")
        }

        fn stream<'a>(
            &'a self,
            _config: &'a ResolvedModel,
            _messages: Vec<ChatMsg>,
            _tools: Vec<serde_json::Value>,
            tx: &'a tokio::sync::mpsc::Sender<ProviderEvent>,
            _control: &'a CallControl,
        ) -> BoxFuture<'a, Result<(), CallError>> {
            Box::pin(async move {
                let _ = tx.send(ProviderEvent::Text("hello from custom driver".into())).await;
                let _ = tx.send(ProviderEvent::Finished(FinishReason::Stop)).await;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn custom_driver_extends_registry_without_modifying_stream_chat() {
        let registry = ProviderRegistry::with_builtins();
        registry.register(Arc::new(CustomGeminiDriver));

        let mut model = test_model(ApiFormat::OpenAiChat);
        model.base_url = "https://generativelanguage.googleapis.com/v1beta".into();
        let resolved = registry.resolve(&model).expect("resolved");
        assert_eq!(resolved.id(), "custom.gemini");

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let control = CallControl::new(tokio_util::sync::CancellationToken::new());
        crate::stream::stream_chat_with_registry(&registry, model, vec![], vec![], tx, control)
            .await;

        let mut saw_text = String::new();
        let mut finished = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                ProviderEvent::Text(t) => saw_text.push_str(&t),
                ProviderEvent::Finished(FinishReason::Stop) => finished = true,
                _ => {}
            }
        }
        assert_eq!(saw_text, "hello from custom driver");
        assert!(finished);
    }

    #[test]
    fn websocket_driver_wins_for_ws_responses_urls_and_http_driver_handles_http() {
        let registry = ProviderRegistry::with_builtins();
        let mut ws_model = test_model(ApiFormat::OpenAiResponses);
        ws_model.base_url = "wss://api.openai.com/v1".into();
        assert_eq!(
            registry.resolve(&ws_model).unwrap().id(),
            "openai.responses.websocket"
        );

        let mut http_model = test_model(ApiFormat::OpenAiResponses);
        http_model.base_url = "https://api.openai.com/v1".into();
        assert_eq!(
            registry.resolve(&http_model).unwrap().id(),
            "openai.responses.http"
        );
    }
}
