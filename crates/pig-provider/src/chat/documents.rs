use serde::{Deserialize, Serialize};

use super::ResolvedModel;
use super::messages::ChatMsg;

/// Cached token cost of one PDF document. The source is part of the value so a
/// window-derived reserve can never be mistaken for a measured provider cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DocumentTokenEstimate {
    /// Token cost charged by the context estimator.
    pub tokens: u64,
    /// Context window a bounded fallback was derived from; `None` means the
    /// cost came from a visible page tree and is independent of the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
}

/// Accept both the current tagged object and a bare number. No released
/// rollout contains a numeric cache (the field is newer); an intermediate
/// local build could have written one, and it has no provenance, so it is
/// treated as a measured cost rather than failing the whole rollout line.
impl<'de> Deserialize<'de> for DocumentTokenEstimate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Tagged {
                tokens: u64,
                #[serde(default)]
                window: Option<u64>,
            },
            Legacy(u64),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Tagged { tokens, window } => Self { tokens, window },
            Repr::Legacy(tokens) => Self::measured(tokens),
        })
    }
}

impl DocumentTokenEstimate {
    /// Cost measured from a visible page tree (never repriced).
    pub fn measured(tokens: u64) -> Self {
        Self {
            tokens,
            window: None,
        }
    }

    /// Bounded reserve for an unprovable page structure, derived from
    /// `window`. It is repriced (without reading the bytes) when the consuming
    /// model's window changes.
    pub fn window_fallback(tokens: u64, window: u64) -> Self {
        Self {
            tokens,
            window: Some(window),
        }
    }

    pub fn is_window_fallback(&self) -> bool {
        self.window.is_some()
    }
}

/// PDF document entering the context with a message (ReadMediaFile output).
/// Only `application/pdf` is accepted; the raw base64 never reaches a rollout
/// record or a log (core persists document references instead).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatDocument {
    /// Only `application/pdf` is accepted by the provider
    pub media_type: String,
    pub data_base64: String,
    /// Stable file name used by every wire projection (no timestamp/randomness)
    pub filename: String,
    /// Optional display label; the wire projections use `filename`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Token cost cached by core when the document is loaded or persisted.
    /// Context estimation reads this field instead of decoding/scanning the
    /// payload again; wire projections ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_estimate: Option<DocumentTokenEstimate>,
}

/// Fail closed before any HTTP request: documents are accepted only when the
/// resolved model declares PDF input, and only as `application/pdf`. Core is
/// expected to project a deterministic text fallback when the capability is
/// off; reaching the provider with documents in that state is a bug.
pub(crate) fn validate_documents(
    config: &ResolvedModel,
    messages: &[ChatMsg],
) -> Result<(), pig_protocol::CoreError> {
    for message in messages {
        if !message.documents.is_empty() && !matches!(message.role.as_str(), "user" | "tool") {
            return Err(pig_protocol::CoreError::Internal {
                detail: format!(
                    "Documents are only supported on user and tool messages, not \"{}\"",
                    message.role
                ),
            });
        }
        for document in &message.documents {
            if !config.input_pdf {
                return Err(pig_protocol::CoreError::Internal {
                    detail: format!(
                        "PDF document input is not supported for model \"{}\" (input_pdf is disabled)",
                        config.model
                    ),
                });
            }
            if !document.media_type.eq_ignore_ascii_case("application/pdf") {
                return Err(pig_protocol::CoreError::Internal {
                    detail: format!(
                        "Unsupported document media type \"{}\": only application/pdf is accepted",
                        document.media_type
                    ),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_accepts_tagged_and_legacy_numeric_shapes() {
        let measured: DocumentTokenEstimate =
            serde_json::from_value(serde_json::json!({"tokens": 6_000})).expect("measured");
        assert_eq!(measured, DocumentTokenEstimate::measured(6_000));

        let fallback: DocumentTokenEstimate =
            serde_json::from_value(serde_json::json!({"tokens": 16_000, "window": 64_000}))
                .expect("fallback");
        assert_eq!(
            fallback,
            DocumentTokenEstimate::window_fallback(16_000, 64_000)
        );

        let legacy: DocumentTokenEstimate =
            serde_json::from_value(serde_json::json!(6_000)).expect("legacy numeric cache");
        assert_eq!(legacy, DocumentTokenEstimate::measured(6_000));

        assert_eq!(
            serde_json::to_value(DocumentTokenEstimate::window_fallback(16_000, 64_000))
                .expect("serialize"),
            serde_json::json!({"tokens": 16_000, "window": 64_000})
        );
        assert_eq!(
            serde_json::to_value(DocumentTokenEstimate::measured(6_000)).expect("serialize"),
            serde_json::json!({"tokens": 6_000})
        );
    }
}
