//! Bridges MCP server-initiated `elicitation/create` requests onto Pig Code's
//! existing `Event::QuestionRequested` + `Op::QuestionReply` interactive UI
//! channel (`PendingQuestions`), so any stdio or streamable-HTTP MCP server can
//! prompt the user for structured input mid-tool-call.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pig_protocol::{Event, QuestionItem, QuestionOption};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use crate::mcp::capabilities::{ElicitationHandler, McpElicitationRequest, McpElicitationResponse};
use crate::session::PendingQuestions;

const ELICITATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
static ELICITATION_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ElicitationFieldKind {
    String,
    Boolean,
    Integer,
    Number,
    Array,
    ConfirmOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ElicitationField {
    pub name: String,
    pub kind: ElicitationFieldKind,
    pub required: bool,
}

/// Convert an MCP `elicitation/create` JSON Schema payload into Pig Code's
/// `QuestionItem` cards plus field-coercion metadata.
pub(crate) fn elicitation_to_questions(
    request: &McpElicitationRequest,
) -> (Vec<QuestionItem>, Vec<ElicitationField>) {
    let required_set: std::collections::HashSet<&str> = request
        .requested_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let Some(properties) = request
        .requested_schema
        .get("properties")
        .and_then(Value::as_object)
        .filter(|props| !props.is_empty())
    else {
        let header = Some(format!("MCP · {}", request.server));
        let question = if request.message.trim().is_empty() {
            format!("MCP server `{}` requests confirmation.", request.server)
        } else {
            request.message.clone()
        };
        return (
            vec![QuestionItem {
                question,
                header,
                multi_select: false,
                options: vec![
                    QuestionOption {
                        label: "Accept".to_string(),
                        description: Some("Approve this MCP server request".to_string()),
                    },
                    QuestionOption {
                        label: "Decline".to_string(),
                        description: Some("Decline this MCP server request".to_string()),
                    },
                ],
            }],
            vec![ElicitationField {
                name: String::new(),
                kind: ElicitationFieldKind::ConfirmOnly,
                required: true,
            }],
        );
    };

    let mut keys: Vec<&String> = properties.keys().collect();
    keys.sort();

    let mut questions = Vec::with_capacity(keys.len());
    let mut fields = Vec::with_capacity(keys.len());

    for (idx, key) in keys.into_iter().enumerate() {
        let prop = &properties[key];
        let required = required_set.contains(key.as_str());
        let prop_type = prop
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("string");
        let title = prop
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(key.as_str());
        let description = prop
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let (kind, multi_select, options) = match prop_type {
            "boolean" => (
                ElicitationFieldKind::Boolean,
                false,
                vec![
                    QuestionOption {
                        label: "Yes".to_string(),
                        description: Some("true".to_string()),
                    },
                    QuestionOption {
                        label: "No".to_string(),
                        description: Some("false".to_string()),
                    },
                ],
            ),
            "integer" => (
                ElicitationFieldKind::Integer,
                false,
                default_options_from_schema(prop),
            ),
            "number" => (
                ElicitationFieldKind::Number,
                false,
                default_options_from_schema(prop),
            ),
            "array" => {
                let item_options = prop
                    .get("items")
                    .map(enum_options_from_schema)
                    .unwrap_or_default();
                (ElicitationFieldKind::Array, true, item_options)
            }
            _ => {
                let mut opts = enum_options_from_schema(prop);
                if opts.is_empty() {
                    opts = default_options_from_schema(prop);
                }
                (ElicitationFieldKind::String, false, opts)
            }
        };

        let mut prompt_lines = Vec::new();
        if idx == 0 && !request.message.trim().is_empty() {
            prompt_lines.push(request.message.trim().to_string());
        }
        let field_line = match description {
            Some(desc) => format!("{title}: {desc}"),
            None => title.to_string(),
        };
        prompt_lines.push(field_line);

        questions.push(QuestionItem {
            question: prompt_lines.join("\n"),
            header: Some(format!("MCP · {} · {}", request.server, key)),
            multi_select,
            options,
        });
        fields.push(ElicitationField {
            name: key.clone(),
            kind,
            required,
        });
    }

    (questions, fields)
}

fn enum_options_from_schema(schema: &Value) -> Vec<QuestionOption> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let names = schema.get("enumNames").and_then(Value::as_array);
        return values
            .iter()
            .enumerate()
            .filter_map(|(i, val)| {
                let label = value_as_label(val)?;
                let description = names
                    .and_then(|arr| arr.get(i))
                    .and_then(Value::as_str)
                    .filter(|desc| *desc != label)
                    .map(str::to_string);
                Some(QuestionOption { label, description })
            })
            .collect();
    }
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        return one_of
            .iter()
            .filter_map(|entry| {
                let label = entry.get("const").and_then(value_as_label)?;
                let description = entry
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| *t != label)
                    .map(str::to_string);
                Some(QuestionOption { label, description })
            })
            .collect();
    }
    Vec::new()
}

fn default_options_from_schema(schema: &Value) -> Vec<QuestionOption> {
    if let Some(default) = schema.get("default").and_then(value_as_label) {
        return vec![QuestionOption {
            label: default,
            description: Some("default".to_string()),
        }];
    }
    Vec::new()
}

fn value_as_label(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Convert a user's `QuestionReply` back into an MCP `McpElicitationResponse`.
pub(crate) fn answers_to_elicitation_response(
    fields: &[ElicitationField],
    answers: Option<Vec<Vec<String>>>,
) -> McpElicitationResponse {
    let Some(answers) = answers else {
        return McpElicitationResponse::decline();
    };
    if fields.len() == 1 && fields[0].kind == ElicitationFieldKind::ConfirmOnly {
        let accepted = answers
            .first()
            .and_then(|vals| vals.first())
            .is_some_and(|v| {
                v.eq_ignore_ascii_case("accept")
                    || v.eq_ignore_ascii_case("yes")
                    || v.eq_ignore_ascii_case("true")
            });
        return if accepted {
            McpElicitationResponse::accept(json!({}))
        } else {
            McpElicitationResponse::decline()
        };
    }

    let mut content = Map::new();
    for (idx, field) in fields.iter().enumerate() {
        let selected = answers.get(idx).map(Vec::as_slice).unwrap_or(&[]);
        if selected.is_empty() {
            if field.required {
                return McpElicitationResponse::cancel();
            }
            continue;
        }
        let first = selected[0].trim();
        let coerced = match field.kind {
            ElicitationFieldKind::Boolean => {
                let truthy = first.eq_ignore_ascii_case("yes")
                    || first.eq_ignore_ascii_case("true")
                    || first == "1";
                Value::Bool(truthy)
            }
            ElicitationFieldKind::Integer => match first.parse::<i64>() {
                Ok(n) => Value::Number(n.into()),
                Err(_) => Value::String(first.to_string()),
            },
            ElicitationFieldKind::Number => match first
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
            {
                Some(n) => Value::Number(n),
                None => Value::String(first.to_string()),
            },
            ElicitationFieldKind::Array => {
                Value::Array(selected.iter().map(|s| Value::String(s.clone())).collect())
            }
            ElicitationFieldKind::String | ElicitationFieldKind::ConfirmOnly => {
                Value::String(selected.join(", "))
            }
        };
        content.insert(field.name.clone(), coerced);
    }
    McpElicitationResponse::accept(Value::Object(content))
}

/// Build an [`ElicitationHandler`] bound to one live session's question queue
/// and event channel.
pub fn build_session_elicitation_handler(
    session_id: String,
    pending_questions: PendingQuestions,
    tx: async_channel::Sender<Event>,
) -> ElicitationHandler {
    Arc::new(move |request: McpElicitationRequest| {
        let session_id = session_id.clone();
        let pending_questions = pending_questions.clone();
        let tx = tx.clone();
        Box::pin(async move {
            let (questions, fields) = elicitation_to_questions(&request);
            let seq = ELICITATION_SEQ.fetch_add(1, Ordering::Relaxed);
            // Prefix with `{session_id}-` so `drop_session_questions` cleans it up on teardown.
            let request_id = format!("{session_id}-mcp-elicit-{seq}");
            let (reply_tx, reply_rx) = oneshot::channel();
            pending_questions
                .lock()
                .expect("pending questions lock")
                .insert(request_id.clone(), reply_tx);
            if tx
                .send(Event::QuestionRequested {
                    session_id: session_id.clone(),
                    request_id: request_id.clone(),
                    questions,
                })
                .await
                .is_err()
            {
                pending_questions
                    .lock()
                    .expect("pending questions lock")
                    .remove(&request_id);
                return McpElicitationResponse::cancel();
            }
            let reply = match tokio::time::timeout(ELICITATION_TIMEOUT, reply_rx).await {
                Ok(Ok(answers)) => answers,
                Ok(Err(_)) | Err(_) => {
                    pending_questions
                        .lock()
                        .expect("pending questions lock")
                        .remove(&request_id);
                    let _ = tx
                        .send(Event::QuestionResolved {
                            session_id: session_id.clone(),
                            request_id: request_id.clone(),
                        })
                        .await;
                    return McpElicitationResponse::cancel();
                }
            };
            let _ = tx
                .send(Event::QuestionResolved {
                    session_id: session_id.clone(),
                    request_id,
                })
                .await;
            answers_to_elicitation_response(&fields, reply)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::capabilities::McpElicitationAction;

    #[test]
    fn schema_to_questions_and_coercion_round_trips_types() {
        let req = McpElicitationRequest {
            server: "github".into(),
            message: "Configure release".into(),
            requested_schema: json!({
                "type": "object",
                "properties": {
                    "confirm": { "type": "boolean", "description": "Proceed?" },
                    "env": { "type": "string", "enum": ["staging", "prod"] },
                    "replicas": { "type": "integer", "default": 3 },
                    "tag": { "type": "string", "description": "Release tag" }
                },
                "required": ["confirm", "env", "tag"]
            }),
        };
        let (questions, fields) = elicitation_to_questions(&req);
        assert_eq!(questions.len(), 4);
        assert_eq!(fields.len(), 4);
        // Sorted keys: confirm, env, replicas, tag
        assert_eq!(fields[0].name, "confirm");
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(fields[1].name, "env");
        assert_eq!(questions[1].options.len(), 2);
        assert_eq!(fields[2].name, "replicas");
        assert_eq!(questions[2].options[0].label, "3");
        assert_eq!(fields[3].name, "tag");

        let resp = answers_to_elicitation_response(
            &fields,
            Some(vec![
                vec!["Yes".into()],
                vec!["prod".into()],
                vec!["5".into()],
                vec!["v1.2.3".into()],
            ]),
        );
        assert_eq!(resp.action, McpElicitationAction::Accept);
        assert_eq!(
            resp.content,
            Some(json!({
                "confirm": true,
                "env": "prod",
                "replicas": 5,
                "tag": "v1.2.3"
            }))
        );

        let declined = answers_to_elicitation_response(&fields, None);
        assert_eq!(declined.action, McpElicitationAction::Decline);
    }

    #[test]
    fn empty_schema_uses_confirm_only_prompt() {
        let req = McpElicitationRequest {
            server: "deploy".into(),
            message: "Deploy to production?".into(),
            requested_schema: json!({ "type": "object" }),
        };
        let (questions, fields) = elicitation_to_questions(&req);
        assert_eq!(questions.len(), 1);
        assert_eq!(fields[0].kind, ElicitationFieldKind::ConfirmOnly);
        assert_eq!(
            answers_to_elicitation_response(&fields, Some(vec![vec!["Accept".into()]])).action,
            McpElicitationAction::Accept
        );
        assert_eq!(
            answers_to_elicitation_response(&fields, Some(vec![vec!["Decline".into()]])).action,
            McpElicitationAction::Decline
        );
    }
}
