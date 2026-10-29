//! Conservative context-window overflow classification.
//!
//! Providers report an oversized input in several shapes: HTTP 400/422 bodies
//! with a machine code, business-error envelopes inside a 200 stream, and
//! format-specific error frames. Core must never string-match
//! [`pig_protocol::CoreError::Internal`], so classification happens here and
//! surfaces the typed [`pig_protocol::CoreError::ContextWindowExceeded`].
//!
//! Structured JSON errors are classified from their `error.message`/`code`
//! fields only, never from unrelated fields that may echo the user's prompt.
//! The textual fallback is used only for non-JSON bodies. A bare 413 is not an
//! overflow; it needs an explicit context or request-too-large message.

use pig_protocol::CoreError;

/// Machine-readable provider codes that unambiguously mean "input too large".
/// Codes are compared exactly, never as substrings (so `1261` cannot match a
/// token count such as `12610`).
const CONTEXT_CODES: &[&str] = &[
    "context_length_exceeded",
    "context_window_exceeded",
    "prompt_too_long",
    "input_too_long",
];

/// Strong human-readable phrases used by the first-party providers and the
/// project's presets.
const CONTEXT_PHRASES: &[&str] = &[
    "maximum context length",
    "exceeds the context window",
    "context window exceeded",
    "context length exceeded",
    "context length is too long",
    "prompt is too long",
    "input is too long",
    "input token length too long",
    "model token limit",
    "prompt exceeds max length",
    "exceeds the maximum number of tokens",
    "reduce the length of the messages",
    "reduce the length of your messages",
];

/// Explicit 413 messages that mean the request itself is too large.
const REQUEST_TOO_LARGE_PHRASES: &[&str] = &[
    "request too large",
    "request entity too large",
    "request body too large",
    "payload too large",
];

/// Whether an error detail is a context-window overflow report.
pub(crate) fn is_context_window(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    CONTEXT_CODES
        .iter()
        .chain(CONTEXT_PHRASES.iter())
        .any(|pattern| detail.contains(pattern))
}

fn is_context_code(code: &str) -> bool {
    CONTEXT_CODES
        .iter()
        .any(|known| code.eq_ignore_ascii_case(known))
}

fn is_request_too_large(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    REQUEST_TOO_LARGE_PHRASES
        .iter()
        .any(|pattern| detail.contains(pattern))
}

fn string_field<'a>(value: &'a serde_json::Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(serde_json::Value::as_str)
}

fn code_field(value: &serde_json::Value) -> Option<String> {
    value
        .pointer("/error/code")
        .or_else(|| value.get("code"))
        .and_then(|code| {
            code.as_str()
                .map(str::to_string)
                .or_else(|| code.as_i64().map(|number| number.to_string()))
        })
}

/// Classify from a structured error envelope when the body parses, so fields
/// that merely echo the user's prompt cannot trigger compaction.
fn body_is_context_window(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        // A truncated JSON body may echo the prompt; do not scan its raw text.
        let trimmed = body.trim_start();
        return !(trimmed.starts_with('{') || trimmed.starts_with('[')) && is_context_window(body);
    };
    let message = string_field(&value, "/error/message")
        .or_else(|| string_field(&value, "/error"))
        .or_else(|| string_field(&value, "/message"))
        .or_else(|| string_field(&value, "/msg"));
    let code = code_field(&value);
    code.as_deref().is_some_and(is_context_code) || message.is_some_and(is_context_window)
}

fn body_is_request_too_large(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        let trimmed = body.trim_start();
        return !(trimmed.starts_with('{') || trimmed.starts_with('['))
            && is_request_too_large(body);
    };
    string_field(&value, "/error/message")
        .or_else(|| string_field(&value, "/error"))
        .or_else(|| string_field(&value, "/message"))
        .is_some_and(is_request_too_large)
}

/// Classify a non-2xx HTTP response. Only 400/422 (and 413 with an explicit
/// context/request-too-large message) can be an overflow; a bare 413, 429,
/// 500 or unrelated 400 keeps the ordinary internal-error shape.
pub(crate) fn http_error(status: reqwest::StatusCode, detail: &str) -> CoreError {
    let is_overflow = match status.as_u16() {
        400 | 422 => body_is_context_window(detail),
        413 => body_is_context_window(detail) || body_is_request_too_large(detail),
        _ => false,
    };
    let detail = format!("HTTP {status}: {detail}");
    if is_overflow {
        CoreError::ContextWindowExceeded { detail }
    } else {
        CoreError::Internal { detail }
    }
}

/// Classify an in-band/business error detail (a 200 stream or sidecar body).
pub(crate) fn stream_error(detail: impl Into<String>) -> CoreError {
    let detail = detail.into();
    if is_context_window(&detail) {
        CoreError::ContextWindowExceeded { detail }
    } else {
        CoreError::Internal { detail }
    }
}

/// Build an error detail that carries an upstream machine code when the
/// message alone would not identify it.
pub(crate) fn detail_with_code(prefix: &str, message: &str, code: Option<&str>) -> String {
    match code.filter(|code| !code.is_empty() && !message.contains(code)) {
        Some(code) => format!("{prefix}{message} (code {code})"),
        None => format!("{prefix}{message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{detail_with_code, http_error, is_context_window, stream_error};
    use pig_protocol::CoreError;
    use reqwest::StatusCode;

    #[test]
    fn known_codes_and_strong_phrases_classify() {
        for detail in [
            r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#,
            "This model's maximum context length is 128000 tokens",
            "prompt is too long: 250000 tokens > 200000 maximum",
            "Input is too long for requested model",
            "Requested 9999 tokens, but the context window exceeded",
            "Invalid request: Your request exceeded model token limit: 262144 (requested: 269030)",
            "Input token length too long",
            "Prompt exceeds max length",
            "Context length is too long",
        ] {
            assert!(is_context_window(detail), "{detail}");
        }
    }

    #[test]
    fn ordinary_failures_are_not_misclassified() {
        for detail in [
            r#"{"error":{"message":"invalid api key","code":"invalid_api_key"}}"#,
            "rate limit exceeded",
            "internal server error",
            "insufficient balance (code 1005)",
            "model not found",
            "max_tokens is too large for this model",
            "requested 12610 tokens",
        ] {
            assert!(!is_context_window(detail), "{detail}");
        }
    }

    #[test]
    fn http_classification_is_limited_to_explicit_shapes() {
        assert!(matches!(
            http_error(
                StatusCode::BAD_REQUEST,
                "This model's maximum context length is 8192 tokens"
            ),
            CoreError::ContextWindowExceeded { .. }
        ));
        assert!(matches!(
            http_error(StatusCode::UNPROCESSABLE_ENTITY, "prompt is too long"),
            CoreError::ContextWindowExceeded { .. }
        ));
        assert!(matches!(
            http_error(
                StatusCode::BAD_REQUEST,
                r#"{"error":{"code":"1261","message":"Prompt exceeds max length"}}"#
            ),
            CoreError::ContextWindowExceeded { .. }
        ));
        for status in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
        ] {
            assert!(matches!(
                http_error(status, "prompt is too long"),
                CoreError::Internal { .. }
            ));
        }
        assert!(matches!(
            http_error(StatusCode::BAD_REQUEST, "invalid request"),
            CoreError::Internal { .. }
        ));
    }

    #[test]
    fn bare_413_is_not_overflow_but_an_explicit_one_is() {
        assert!(matches!(
            http_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                r#"{"error":{"message":"invalid request"}}"#
            ),
            CoreError::Internal { .. }
        ));
        assert!(matches!(
            http_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                r#"{"error":{"message":"request too large"}}"#
            ),
            CoreError::ContextWindowExceeded { .. }
        ));
        assert!(matches!(
            http_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "prompt is too long: 9000 tokens"
            ),
            CoreError::ContextWindowExceeded { .. }
        ));
    }

    #[test]
    fn echoed_user_prompt_in_an_unrelated_field_is_not_an_overflow() {
        let body = r#"{"error":{"code":"invalid_request_error","message":"invalid value"},
            "prompt":"This model's maximum context length is 8192 tokens"}"#;
        assert!(matches!(
            http_error(StatusCode::BAD_REQUEST, body),
            CoreError::Internal { .. }
        ));
        // A truncated JSON body is not scanned either.
        let truncated = r#"{"error":{"message":"invalid"},"prompt":"This model's maximum context length is 8192 tokens"#;
        assert!(matches!(
            http_error(StatusCode::BAD_REQUEST, truncated),
            CoreError::Internal { .. }
        ));
        // A non-JSON body still falls back to the strong phrase scan.
        assert!(matches!(
            http_error(
                StatusCode::BAD_REQUEST,
                "upstream said: prompt is too long: 9000 tokens"
            ),
            CoreError::ContextWindowExceeded { .. }
        ));
    }

    #[test]
    fn stream_errors_keep_the_same_boundary() {
        assert!(matches!(
            stream_error("Stream error: prompt is too long"),
            CoreError::ContextWindowExceeded { .. }
        ));
        assert!(matches!(
            stream_error("Stream error: model token limit exceeded"),
            CoreError::ContextWindowExceeded { .. }
        ));
        assert!(matches!(
            stream_error("Stream error: overloaded"),
            CoreError::Internal { .. }
        ));
    }

    #[test]
    fn code_is_appended_only_once() {
        assert_eq!(
            detail_with_code(
                "Response failed: ",
                "too long",
                Some("context_length_exceeded")
            ),
            "Response failed: too long (code context_length_exceeded)"
        );
        assert_eq!(
            detail_with_code(
                "Response failed: ",
                "context_length_exceeded: too long",
                Some("context_length_exceeded")
            ),
            "Response failed: context_length_exceeded: too long"
        );
    }
}
