//! Model API wire logging (opt-in diagnostic): every model API call's full
//! request body and accumulated response, emitted under the `pig_api` target.
//! Enabled by `PIG_LOG_API=1/true/on/yes` (read once at first use). pig-app
//! routes the target to its own daily rolling file `pig-api.log.*` under
//! `{data_dir}/logs` and excludes it from the main log (`pig_api=off` is
//! always appended to the main filter).
//!
//! Privacy: base64 image payloads inside request bodies are redacted
//! (`data:{mime};base64,<N bytes>`); api keys are never logged (auth headers
//! are not part of what is recorded).

use std::sync::OnceLock;

use crate::provider::ToolCall;

/// Whether the API wire log is on (PIG_LOG_API; checked once).
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("PIG_LOG_API")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"))
            .unwrap_or(false)
    })
}

/// One model API call's wire log accumulator. All methods are no-ops when
/// PIG_LOG_API is off, so call sites stay unconditional; construction is the
/// only cost (two string clones).
pub struct ApiCall {
    enabled: bool,
    /// Endpoint label, e.g. "openai.chat" / "anthropic.messages"
    api: &'static str,
    provider: String,
    model: String,
    text: String,
    reasoning: String,
    /// Model-reported token usage of the streamed usage frame
    /// (input/cache_read/output/used/total, formatted)
    usage: Option<String>,
    /// Raw wire payload, verbatim: every non-empty SSE line for streams
    /// (`event:`/`data:` lines), the whole response body for one-shot calls.
    /// Typed parsing drops provider-specific fields; this never does.
    raw: String,
}

impl ApiCall {
    pub fn new(api: &'static str, provider: &str, model: &str) -> Self {
        Self {
            enabled: enabled(),
            api,
            provider: provider.to_string(),
            model: model.to_string(),
            text: String::new(),
            reasoning: String::new(),
            usage: None,
            raw: String::new(),
        }
    }

    /// Record the model-reported token usage (the streamed usage frame);
    /// printed on the outcome line at finish. `reasoning_output` = the
    /// reasoning/thinking slice of the output total (OpenAI
    /// completion_tokens_details.reasoning_tokens / Anthropic
    /// output_tokens_details.thinking_tokens), printed when > 0 — the totals
    /// already include it.
    #[allow(clippy::too_many_arguments)]
    pub fn usage(
        &mut self,
        input: u64,
        cache_read: u64,
        output: u64,
        used: u64,
        total: u64,
        reasoning_output: u64,
    ) {
        if self.enabled {
            let mut usage = format!(
                "input={input} cache_read={cache_read} output={output} used={used} total={total}"
            );
            if reasoning_output > 0 {
                usage.push_str(&format!(" reasoning_output={reasoning_output}"));
            }
            self.usage = Some(usage);
        }
    }

    /// Log the outgoing request: url + full body (pretty-printed, base64
    /// image payloads redacted).
    pub fn request(&self, url: &str, body: &serde_json::Value) {
        if !self.enabled {
            return;
        }
        let mut body = body.clone();
        redact_images(&mut body);
        let body = serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string());
        tracing::info!(target: "pig_api", "==> {} provider={} model={}\nPOST {}\n{}", self.api, self.provider, self.model, url, body);
    }

    /// Accumulate a streamed text delta.
    pub fn text(&mut self, delta: &str) {
        if self.enabled {
            self.text.push_str(delta);
        }
    }

    /// Accumulate a streamed reasoning delta.
    pub fn reasoning(&mut self, delta: &str) {
        if self.enabled {
            self.reasoning.push_str(delta);
        }
    }

    /// Accumulate one raw wire line/body verbatim (SSE `event:`/`data:` lines,
    /// or the whole response body of one-shot calls).
    pub fn raw_line(&mut self, line: &str) {
        if self.enabled {
            self.raw.push_str(line);
            self.raw.push('\n');
        }
    }

    /// Log the completed call: outcome ("stop" / "tool_calls" / "cancelled" /
    /// "http 429: ..." / ...) plus the accumulated response. `tool_calls` is
    /// the streamed tool-call accumulation (&[] on one-shot calls).
    pub fn finish(self, outcome: &str, tool_calls: &[ToolCall]) {
        if !self.enabled {
            return;
        }
        let mut out = format!(
            "<== {} provider={} model={} outcome={outcome}",
            self.api, self.provider, self.model
        );
        if let Some(usage) = &self.usage {
            out.push_str(&format!(" usage=[{usage}]"));
        }
        if !self.reasoning.is_empty() {
            out.push_str(&format!("\n[reasoning]\n{}", self.reasoning));
        }
        if !self.text.is_empty() {
            out.push_str(&format!("\n[text]\n{}", self.text));
        }
        for call in tool_calls {
            out.push_str(&format!(
                "\n[tool_call] {} {}\n{}",
                call.id, call.name, call.arguments
            ));
        }
        if !self.raw.is_empty() {
            out.push_str(&format!("\n[raw]\n{}", self.raw.trim_end()));
        }
        tracing::info!(target: "pig_api", "{out}");
    }

    /// Log a failed call (network error / non-2xx / parse failure) before any
    /// response accumulated.
    pub fn fail(self, detail: &str) {
        self.finish(&format!("error: {detail}"), &[]);
    }
}

/// Deep-redact `data:*;base64,...` payloads in a request body (images would
/// otherwise flood the log with megabytes of base64).
fn redact_images(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(redacted) = redact_data_url(s) {
                *s = redacted;
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_images),
        serde_json::Value::Object(map) => map.values_mut().for_each(redact_images),
        _ => {}
    }
}

fn redact_data_url(s: &str) -> Option<String> {
    let rest = s.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    Some(format!("data:{mime};base64,<{} bytes>", data.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_base64_data_urls_recursively() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}},
                ]},
            ],
            "note": "data:text/plain;base64,eA==",
            "plain": "no redaction",
        });
        redact_images(&mut body);
        assert_eq!(
            body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,<8 bytes>"
        );
        assert_eq!(body["note"], "data:text/plain;base64,<4 bytes>");
        assert_eq!(body["plain"], "no redaction");
    }

    #[test]
    fn api_call_disabled_is_noop() {
        // PIG_LOG_API is unset in tests: everything is a no-op and must not panic
        let mut call = ApiCall::new("openai.chat", "p", "m");
        call.request("http://x", &serde_json::json!({"a": 1}));
        call.text("hello");
        call.reasoning("think");
        call.finish("stop", &[]);
        ApiCall::new("openai.chat", "p", "m").fail("boom");
    }
}
