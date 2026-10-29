//! Parallel tool-call policy and selective strict-tool preference.
//!
//! Tool choice is intentionally gone: every configured request runs with the
//! protocol's automatic choice. Chat and Anthropic keep their legacy default
//! omission; Responses keeps its explicit `"auto"`. Only the parallel
//! compatibility override remains:
//!
//! | decision          | Chat Completions               | Responses                      | Anthropic Messages                              |
//! |-------------------|--------------------------------|--------------------------------|-------------------------------------------------|
//! | tools, parallel   | omitted                        | `"auto"` + `parallel_tool_calls:true` | omitted                                  |
//! | tools, serial     | `parallel_tool_calls:false`    | `"auto"` + `parallel_tool_calls:false`| `{"type":"auto","disable_parallel_tool_use":true}` |
//! | no tools          | all policy fields omitted      | all policy fields omitted      | all policy fields omitted                       |
//!
//! Strict is a per-tool best-effort preference read from the OpenAI wire shape
//! (`function.strict == true`). Undeclared tools are never strictified. A
//! declared tool uses the strict subset only when the endpoint/model supports
//! it; otherwise the ordinary schema is sent and a debug/warning is logged.
//! Nothing fails the request.

use serde_json::Value;

/// Parallel tool-call policy carried on [`crate::ResolvedModel`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallSettings {
    pub parallel_tool_calls: bool,
}

impl Default for ToolCallSettings {
    fn default() -> Self {
        Self {
            parallel_tool_calls: true,
        }
    }
}

impl ToolCallSettings {
    pub fn new(parallel_tool_calls: bool) -> Self {
        Self {
            parallel_tool_calls,
        }
    }
}

/// Chat Completions only sends the non-default opt-out.
pub(crate) fn chat_parallel(settings: &ToolCallSettings, tools: &[Value]) -> Option<bool> {
    (!tools.is_empty() && !settings.parallel_tool_calls).then_some(false)
}

/// Responses always carries the explicit boolean when tools are present.
pub(crate) fn responses_parallel(settings: &ToolCallSettings, tools: &[Value]) -> Option<bool> {
    (!tools.is_empty()).then_some(settings.parallel_tool_calls)
}

/// Anthropic carries a non-default parallel decision inside `tool_choice` with
/// the inverted sign.
pub(crate) fn anthropic_parallel_choice(
    settings: &ToolCallSettings,
    tools: &[Value],
) -> Option<Value> {
    (!tools.is_empty() && !settings.parallel_tool_calls)
        .then(|| serde_json::json!({"type": "auto", "disable_parallel_tool_use": true}))
}

/// A tool opts into the strict subset by declaring `function.strict: true`.
/// `false` and an absent field both mean "ordinary schema".
pub(crate) fn declared_strict(tool: &Value) -> bool {
    tool["function"]["strict"].as_bool() == Some(true)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrictSupport {
    Disabled,
    UnsupportedModel,
    Supported,
}

/// Resolve one schema under the endpoint/model strict capability. This never
/// fails: an unavailable capability or an inexpressible schema downgrades to
/// the ordinary schema (the model still sees the description).
pub(crate) fn strict_schema(
    support: StrictSupport,
    prefer: bool,
    schema: &Value,
    tool_name: &str,
    model_id: &str,
) -> Option<Value> {
    if !prefer {
        return None;
    }
    if support != StrictSupport::Supported {
        tracing::debug!(
            tool = tool_name,
            model = model_id,
            "strict preference not honored: endpoint/model capability unavailable"
        );
        return None;
    }
    match crate::strict_tools::strictify_tool_schema(schema) {
        Some(strict) => Some(strict),
        None => {
            tracing::warn!(
                tool = tool_name,
                "strict preference not honored: schema is not expressible in the strict subset"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        StrictSupport, ToolCallSettings, anthropic_parallel_choice, chat_parallel, declared_strict,
        responses_parallel, strict_schema,
    };
    use serde_json::json;

    fn tools() -> Vec<serde_json::Value> {
        vec![json!({
            "type": "function",
            "function": {"name": "Read", "description": "d", "parameters": {"type": "object"}}
        })]
    }

    #[test]
    fn chat_omits_the_default_and_sends_only_the_opt_out() {
        assert_eq!(chat_parallel(&ToolCallSettings::default(), &tools()), None);
        assert_eq!(
            chat_parallel(&ToolCallSettings::new(false), &tools()),
            Some(false)
        );
        assert_eq!(
            chat_parallel(&ToolCallSettings::new(false), &[]),
            None,
            "tool-less requests omit the whole policy"
        );
    }

    #[test]
    fn responses_keeps_the_explicit_parallel_boolean() {
        assert_eq!(
            responses_parallel(&ToolCallSettings::default(), &tools()),
            Some(true)
        );
        assert_eq!(
            responses_parallel(&ToolCallSettings::new(false), &tools()),
            Some(false)
        );
        assert_eq!(responses_parallel(&ToolCallSettings::default(), &[]), None);
    }

    #[test]
    fn anthropic_inverts_the_serial_flag_inside_tool_choice() {
        assert_eq!(
            anthropic_parallel_choice(&ToolCallSettings::default(), &tools()),
            None
        );
        assert_eq!(
            anthropic_parallel_choice(&ToolCallSettings::new(false), &tools()),
            Some(json!({"type": "auto", "disable_parallel_tool_use": true}))
        );
        assert_eq!(
            anthropic_parallel_choice(&ToolCallSettings::new(false), &[]),
            None
        );
    }

    #[test]
    fn only_an_explicit_true_declares_strict() {
        assert!(declared_strict(&json!({
            "function": {"name": "Read", "strict": true}
        })));
        assert!(!declared_strict(&json!({
            "function": {"name": "Read", "strict": false}
        })));
        assert!(!declared_strict(&json!({"function": {"name": "Read"}})));
    }

    #[test]
    fn strict_preference_degrades_without_failing() {
        let schema = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        assert!(strict_schema(StrictSupport::Supported, false, &schema, "Read", "m").is_none());
        assert!(
            strict_schema(StrictSupport::Disabled, true, &schema, "Read", "m").is_none(),
            "capability off downgrades"
        );
        assert!(
            strict_schema(
                StrictSupport::UnsupportedModel,
                true,
                &schema,
                "Read",
                "claude-3-5-sonnet"
            )
            .is_none(),
            "known unsupported model downgrades"
        );
        assert!(
            strict_schema(
                StrictSupport::Supported,
                true,
                &json!({"$ref": "#/definitions/x"}),
                "Read",
                "m"
            )
            .is_none(),
            "inexpressible schema downgrades"
        );
        let strict = strict_schema(StrictSupport::Supported, true, &schema, "Read", "m")
            .expect("declared, supported and expressible");
        assert_eq!(strict["additionalProperties"], false);
        assert_eq!(strict["required"], json!(["path"]));
    }
}
