use std::collections::HashSet;

use pig_provider::ToolCall;

use super::{RolloutRecord, ToolExecutionRecovery};

pub(crate) fn tool_execution_start_records(
    calls: &[ToolCall],
    execution_ids: &[String],
    start: usize,
    end: usize,
) -> Vec<RolloutRecord> {
    let started_at_ms = crate::model_io::now_ms();
    calls[start..end]
        .iter()
        .enumerate()
        .map(|(offset, call)| {
            let call_index = start + offset;
            RolloutRecord::ToolExecutionStart {
                execution_id: execution_ids[call_index].clone(),
                call_id: call.id.clone(),
                call_index,
                tool: call.name.clone(),
                started_at_ms,
            }
        })
        .collect()
}

struct PendingCall {
    execution_id: String,
    call: ToolCall,
    started: bool,
}

struct PendingRound {
    calls: Vec<PendingCall>,
    answered: usize,
}

pub(crate) fn tool_execution_recoveries(
    records: &[RolloutRecord],
) -> Result<Vec<RolloutRecord>, String> {
    let mut pending: Option<PendingRound> = None;
    for record in records {
        match record {
            RolloutRecord::Assistant {
                tool_calls,
                tool_execution_ids,
                ..
            } => {
                ensure_settled(&pending)?;
                if tool_calls.is_empty() {
                    pending = None;
                    continue;
                }
                if tool_calls.len() != tool_execution_ids.len() {
                    return Err(format!(
                        "Assistant has {} tool calls but {} execution ids",
                        tool_calls.len(),
                        tool_execution_ids.len()
                    ));
                }
                pending = Some(PendingRound {
                    calls: tool_calls
                        .iter()
                        .zip(tool_execution_ids)
                        .map(|(call, execution_id)| PendingCall {
                            execution_id: execution_id.clone(),
                            call: ToolCall {
                                id: call.id.clone(),
                                name: call.function.name.clone(),
                                arguments: call.function.arguments.clone(),
                            },
                            started: false,
                        })
                        .collect(),
                    answered: 0,
                });
            }
            RolloutRecord::ToolExecutionStart {
                execution_id,
                call_id,
                call_index,
                ..
            } => {
                let round = pending
                    .as_mut()
                    .ok_or_else(|| format!("Tool start {execution_id} has no active assistant"))?;
                let call = round
                    .calls
                    .get_mut(*call_index)
                    .filter(|call| call.execution_id == *execution_id && call.call.id == *call_id)
                    .ok_or_else(|| {
                        format!("Tool start {execution_id} does not match its assistant")
                    })?;
                call.started = true;
            }
            RolloutRecord::ToolCall { execution_id, .. } => {
                let round = pending
                    .as_mut()
                    .ok_or_else(|| format!("Tool result {execution_id} has no active assistant"))?;
                let expected = round.calls.get(round.answered).ok_or_else(|| {
                    format!("Tool result {execution_id} exceeds the assistant calls")
                })?;
                if expected.execution_id != *execution_id {
                    return Err(format!(
                        "Tool result {execution_id} does not match expected execution {}",
                        expected.execution_id
                    ));
                }
                round.answered += 1;
                if round.answered == round.calls.len() {
                    pending = None;
                }
            }
            RolloutRecord::User { .. }
            | RolloutRecord::TaskNotificationBatch { .. }
            | RolloutRecord::Compact { .. }
            | RolloutRecord::HookContext { .. }
            | RolloutRecord::Recall => {
                ensure_settled(&pending)?;
                pending = None;
            }
            RolloutRecord::Meta { .. }
            | RolloutRecord::System { .. }
            | RolloutRecord::PromptSnapshot { .. }
            | RolloutRecord::Reasoning { .. }
            | RolloutRecord::Text { .. }
            | RolloutRecord::TurnChanges { .. }
            | RolloutRecord::TurnStats { .. }
            | RolloutRecord::StepUsage { .. }
            | RolloutRecord::AgentRun { .. } => {}
        }
    }
    let Some(round) = pending else {
        return Ok(Vec::new());
    };
    let answered = round.answered;
    let mut seen = HashSet::new();
    Ok(round
        .calls
        .into_iter()
        .skip(answered)
        .map(|pending| {
            if !seen.insert(pending.execution_id.clone()) {
                return Err(format!("Duplicate execution id {}", pending.execution_id));
            }
            let kind = if pending.started {
                ToolExecutionRecovery::OutcomeUnknown
            } else {
                ToolExecutionRecovery::NotStarted
            };
            let output = match kind {
                ToolExecutionRecovery::NotStarted => "The tool call was interrupted before the Harness recorded it as started. Retry it if it is still needed.",
                ToolExecutionRecovery::OutcomeUnknown => "The tool call was interrupted after it was recorded as started, but no result was durably recorded. Its outcome is unknown. Retry only if the operation is read-only or idempotent; otherwise verify external state or ask the user before retrying.",
            };
            Ok(RolloutRecord::ToolCall {
                execution_id: pending.execution_id,
                tool: pending.call.name.clone(),
                summary: crate::tool::summarize(&pending.call),
                arguments: pending.call.arguments.clone(),
                output: output.to_string(),
                is_error: true,
                stopped: false,
                edit: None,
                agent_card: None,
                agent_cards: Vec::new(),
                images: Vec::new(),
                documents: Vec::new(),
                delivered_notification_ids: Vec::new(),
                recovery: Some(kind),
            })
        })
        .collect::<Result<Vec<_>, String>>()?)
}

fn ensure_settled(pending: &Option<PendingRound>) -> Result<(), String> {
    if pending
        .as_ref()
        .is_some_and(|round| round.answered < round.calls.len())
    {
        return Err("An unfinished tool round is followed by another conversation boundary".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollout::rebuild_history;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "Read".into(),
            arguments: "{}".into(),
        }
    }

    fn assistant(calls: &[ToolCall], ids: &[&str]) -> RolloutRecord {
        RolloutRecord::Assistant {
            text: String::new(),
            reasoning: None,
            replay: None,
            tool_calls: calls.iter().map(ToolCall::to_wire).collect(),
            tool_execution_ids: ids.iter().map(|id| (*id).to_string()).collect(),
            ts_ms: 0,
            reasoning_ms: None,
        }
    }

    fn result(execution_id: &str, call: &ToolCall) -> RolloutRecord {
        RolloutRecord::ToolCall {
            execution_id: execution_id.into(),
            tool: call.name.clone(),
            summary: call.name.clone(),
            arguments: call.arguments.clone(),
            output: "done".into(),
            is_error: false,
            stopped: false,
            edit: None,
            agent_card: None,
            agent_cards: vec![],
            images: vec![],
            documents: vec![],
            delivered_notification_ids: vec![],
            recovery: None,
        }
    }

    #[test]
    fn recovery_distinguishes_unknown_and_not_started_and_is_idempotent() {
        let calls = vec![call("a"), call("b"), call("c")];
        let mut records = vec![
            assistant(&calls, &["e1", "e2", "e3"]),
            RolloutRecord::ToolExecutionStart {
                execution_id: "e1".into(),
                call_id: "a".into(),
                call_index: 0,
                tool: "Read".into(),
                started_at_ms: 1,
            },
            result("e1", &calls[0]),
            RolloutRecord::ToolExecutionStart {
                execution_id: "e2".into(),
                call_id: "b".into(),
                call_index: 1,
                tool: "Read".into(),
                started_at_ms: 2,
            },
        ];
        let recovered = tool_execution_recoveries(&records).expect("recovery");
        assert_eq!(recovered.len(), 2);
        assert!(matches!(
            &recovered[0],
            RolloutRecord::ToolCall {
                execution_id,
                recovery: Some(ToolExecutionRecovery::OutcomeUnknown),
                ..
            } if execution_id == "e2"
        ));
        assert!(matches!(
            &recovered[1],
            RolloutRecord::ToolCall {
                execution_id,
                recovery: Some(ToolExecutionRecovery::NotStarted),
                ..
            } if execution_id == "e3"
        ));
        records.extend(recovered);
        assert!(
            tool_execution_recoveries(&records)
                .expect("second recovery")
                .is_empty()
        );
        let (history, _) = rebuild_history(&records, "system".into());
        assert_eq!(
            history
                .iter()
                .filter(|message| message.role == "tool")
                .count(),
            3
        );
        assert!(
            history[3]
                .content
                .as_deref()
                .is_some_and(|text| text.contains("outcome is unknown"))
        );
        assert!(
            history[4]
                .content
                .as_deref()
                .is_some_and(|text| text.contains("before the Harness recorded it as started"))
        );
    }

    #[test]
    fn duplicate_provider_call_ids_use_distinct_execution_ids() {
        let calls = vec![call("same"), call("same")];
        let records = vec![
            assistant(&calls, &["first", "second"]),
            RolloutRecord::ToolExecutionStart {
                execution_id: "first".into(),
                call_id: "same".into(),
                call_index: 0,
                tool: "Read".into(),
                started_at_ms: 1,
            },
            RolloutRecord::ToolExecutionStart {
                execution_id: "second".into(),
                call_id: "same".into(),
                call_index: 1,
                tool: "Read".into(),
                started_at_ms: 1,
            },
            result("first", &calls[0]),
        ];
        let recovered = tool_execution_recoveries(&records).expect("recovery");
        assert_eq!(recovered.len(), 1);
        assert!(matches!(
            &recovered[0],
            RolloutRecord::ToolCall { execution_id, .. } if execution_id == "second"
        ));
    }

    #[test]
    fn execution_identity_is_required_by_the_json_schema() {
        let tool = serde_json::from_str::<RolloutRecord>(
            r#"{"type":"tool_call","tool":"Read","summary":"s","arguments":"{}","output":"ok","is_error":false,"edit":null,"agent_card":null,"agent_cards":[]}"#,
        );
        assert!(tool.is_err());
        let assistant = serde_json::from_str::<RolloutRecord>(
            r#"{"type":"assistant","text":"","tool_calls":[{"id":"call","type":"function","function":{"name":"Read","arguments":"{}"}}]}"#,
        );
        assert!(assistant.is_err());
    }

    #[test]
    fn malformed_execution_association_is_rejected() {
        let calls = vec![call("a")];
        let error =
            tool_execution_recoveries(&[assistant(&calls, &[])]).expect_err("length mismatch");
        assert!(error.contains("1 tool calls but 0 execution ids"));
    }
}
