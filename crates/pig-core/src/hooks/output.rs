//! Hook output contract (Claude Code-compatible): exit code 0 with optional
//! stdout JSON decides, exit code 2 blocks (reason synthesized from
//! stderr/stdout), anything else is a non-blocking failure. Parsed JSON
//! fields are interpreted per event; a known field with a wrong type fails
//! that hook in isolation (the chain continues).

use serde_json::Value;

use super::HookEvent;

/// Permission adjustment a hook may return (PreToolUse hookSpecificOutput.
/// permissionDecision, or PermissionRequest decision:"approve"). Merge
/// precedence across hooks: deny > ask > allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookPermission {
    Allow,
    Ask,
    Deny,
}

impl HookPermission {
    fn rank(self) -> u8 {
        match self {
            HookPermission::Allow => 0,
            HookPermission::Ask => 1,
            HookPermission::Deny => 2,
        }
    }
}

/// Merged effects of one hook chain. `blocked` semantics differ per event:
/// for most events it stops the action; the Stop site instead reads
/// `stop_signal` (any explicit continue/decision signal) plus non-empty
/// `additional_contexts` to force one more model round (capped at 3).
#[derive(Default, Debug)]
pub struct HookEffects {
    /// continue:false / decision:"block" / exit 2 — the hook-supplied reason
    pub blocked: Option<String>,
    pub additional_contexts: Vec<String>,
    pub permission: Option<HookPermission>,
    /// Replacement tool input (PreToolUse; last hook wins)
    pub updated_input: Option<Value>,
    /// Stop: the hook asked the agent to keep going (the site also requires
    /// non-empty contexts and enforces the cap)
    pub stop_signal: bool,
    /// PostToolUse: replace the tool output shown to the model with a
    /// placeholder
    pub suppress_output: bool,
}

impl HookEffects {
    /// Merge a later hook's effects into this chain result: block is sticky
    /// (first reason kept), contexts concatenate, permission keeps the
    /// strongest, updated_input is last-writer-wins.
    pub(crate) fn merge(&mut self, other: HookEffects) {
        if other.blocked.is_some() {
            self.blocked = self.blocked.take().or(other.blocked);
        }
        self.additional_contexts.extend(other.additional_contexts);
        self.permission = match (self.permission, other.permission) {
            (Some(a), Some(b)) if b.rank() > a.rank() => Some(b),
            (None, Some(b)) => Some(b),
            (earlier, _) => earlier,
        };
        if other.updated_input.is_some() {
            self.updated_input = other.updated_input;
        }
        self.stop_signal |= other.stop_signal;
        self.suppress_output |= other.suppress_output;
    }
}

/// Raw process result of one hook run (see runner::run_entry)
pub(crate) struct HookRaw {
    pub outcome: RawOutcome,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) enum RawOutcome {
    /// Process exited with a status code (None = killed by signal)
    Exited(Option<i32>),
    SpawnFailed(String),
    TimedOut,
    Cancelled,
}

impl HookRaw {
    pub(crate) fn stdout_preview(&self) -> &str {
        &self.stdout
    }

    pub(crate) fn stderr_preview(&self) -> &str {
        &self.stderr
    }

    /// Short tag for log lines
    pub(crate) fn outcome_tag(&self) -> &'static str {
        match self.outcome {
            RawOutcome::Exited(Some(0)) => "success",
            RawOutcome::Exited(Some(2)) => "blocked",
            RawOutcome::Exited(_) => "failed",
            RawOutcome::SpawnFailed(_) => "spawn_failed",
            RawOutcome::TimedOut => "timed_out",
            RawOutcome::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Finished,
    Blocked,
    Failed,
    Cancelled,
}

/// Interpret one finished hook run: (effects for the chain, outcome label).
pub(crate) fn interpret(event: HookEvent, raw: &HookRaw) -> (HookEffects, Outcome) {
    match &raw.outcome {
        RawOutcome::Cancelled => (HookEffects::default(), Outcome::Cancelled),
        RawOutcome::TimedOut => (HookEffects::default(), Outcome::Failed),
        RawOutcome::SpawnFailed(detail) => {
            tracing::warn!(detail = %detail, "hook failed to spawn");
            (HookEffects::default(), Outcome::Failed)
        }
        RawOutcome::Exited(code) => match code {
            Some(0) => {
                let trimmed = raw.stdout.trim();
                if trimmed.starts_with('{') {
                    match serde_json::from_str::<Value>(trimmed) {
                        Ok(json) => match parse_json_output(event, &json) {
                            Ok(effects) => (effects, Outcome::Finished),
                            Err(detail) => {
                                tracing::warn!(detail = %detail, "hook stdout JSON invalid");
                                (HookEffects::default(), Outcome::Failed)
                            }
                        },
                        Err(error) => {
                            tracing::warn!(detail = %error, "hook stdout is not valid JSON");
                            (HookEffects::default(), Outcome::Failed)
                        }
                    }
                } else {
                    // Plain diagnostic text on stdout: success with no effect
                    (HookEffects::default(), Outcome::Finished)
                }
            }
            Some(2) => {
                let reason = block_reason(raw);
                let mut effects = HookEffects::default();
                if event == HookEvent::Stop {
                    // Stop semantics are inverted: "block" means "don't stop",
                    // and the reason itself becomes the continuation context
                    effects.stop_signal = true;
                    effects.additional_contexts.push(reason);
                } else {
                    effects.blocked = Some(reason);
                }
                (effects, Outcome::Blocked)
            }
            other => {
                tracing::warn!(
                    exit_code = ?other,
                    stderr = %raw.stderr,
                    "hook exited non-zero"
                );
                (HookEffects::default(), Outcome::Failed)
            }
        },
    }
}

/// Redact credential-looking text from hook-supplied block reasons before
/// they reach the model or the UI (ZCode's sanitizeHookDisplayText): key-value
/// pairs whose key names a secret (`token: …`, `password=…`, quoted or not)
/// and `Bearer <token>` strings
pub(crate) fn sanitize_reason(text: &str) -> String {
    static SECRET_PAIR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)\b(api[-_]?key|api[-_]?secret|token|access[-_]?token|refresh[-_]?token|id[-_]?token|password|passwd|secret|authorization|auth[-_]?token|access[-_]?key|secret[-_]?key|private[-_]?key|client[-_]?secret)(\s*[:=]\s*["']?)[^\s"']+"#,
        )
        .expect("secret pair regex")
    });
    static BEARER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\bbearer\s+[a-z0-9._~+/=-]+").expect("bearer regex")
    });
    // Bearer first: otherwise the pair rule consumes "Bearer" as the value of
    // an "Authorization:" key and leaves the actual token behind
    let text = BEARER.replace_all(text, "Bearer ***");
    SECRET_PAIR.replace_all(&text, "${1}${2}***").into_owned()
}

/// Exit-2 block reason: stderr preview, else stdout preview, else a default
fn block_reason(raw: &HookRaw) -> String {
    let reason = if !raw.stderr.trim().is_empty() {
        raw.stderr.trim()
    } else if !raw.stdout.trim().is_empty() {
        raw.stdout.trim()
    } else {
        "Hook blocked execution"
    };
    const REASON_CAP: usize = 500;
    let capped = if reason.len() <= REASON_CAP {
        reason.to_string()
    } else {
        // Cut on a char boundary
        let mut end = REASON_CAP;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &reason[..end])
    };
    sanitize_reason(&capped)
}

/// Parse the stdout JSON object against the per-event contract. Returns Err
/// (hook fails, isolated) when a known field has a wrong type or
/// hookSpecificOutput names a different event.
pub(crate) fn parse_json_output(event: HookEvent, json: &Value) -> Result<HookEffects, String> {
    let object = json
        .as_object()
        .ok_or_else(|| "output must be a JSON object".to_string())?;
    let mut effects = HookEffects::default();
    let str_field =
        |object: &serde_json::Map<String, Value>, key: &str| -> Result<Option<String>, String> {
            match object.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) => Ok(Some(text.clone())),
                Some(_) => Err(format!("field \"{key}\" must be a string")),
            }
        };
    let reason = || -> Result<Option<String>, String> {
        Ok(str_field(object, "reason")?
            .or(str_field(object, "stopReason")?)
            .filter(|text| !text.is_empty())
            .map(|text| sanitize_reason(&text)))
    };
    if let Some(value) = object.get("continue") {
        let flag = value
            .as_bool()
            .ok_or_else(|| "field \"continue\" must be a boolean".to_string())?;
        if event == HookEvent::Stop {
            // Stop: continue:true asks the agent to keep going;
            // continue:false is ignored at the stop boundary (ZCode semantics)
            if flag {
                effects.stop_signal = true;
                if let Some(text) = reason()? {
                    effects.additional_contexts.push(text);
                }
            }
        } else if !flag {
            effects.blocked =
                Some(reason()?.unwrap_or_else(|| "Hook blocked execution".to_string()));
        }
    }
    if let Some(decision) = str_field(object, "decision")? {
        match decision.as_str() {
            "block" => {
                if event == HookEvent::Stop {
                    // "block" at the stop boundary means "don't stop": the
                    // reason itself becomes the continuation context
                    effects.stop_signal = true;
                    if let Some(text) = reason()? {
                        effects.additional_contexts.push(text);
                    }
                } else {
                    effects.blocked =
                        Some(reason()?.unwrap_or_else(|| "Hook blocked execution".to_string()));
                }
            }
            // Top-level approve only answers permission prompts
            "approve" if event == HookEvent::PermissionRequest => {
                effects.permission = Some(HookPermission::Allow);
            }
            _ => {}
        }
    }
    if let Some(suppress) = object.get("suppressOutput") {
        effects.suppress_output = suppress
            .as_bool()
            .ok_or_else(|| "field \"suppressOutput\" must be a boolean".to_string())?;
    }
    // systemMessage on a continuing Stop rides along as context; elsewhere it
    // has no carrier in pig's v1 and is dropped
    if event == HookEvent::Stop
        && effects.stop_signal
        && let Some(message) = str_field(object, "systemMessage")?
    {
        effects.additional_contexts.push(message);
    }
    for key in ["additionalContext", "additional_context"] {
        if let Some(value) = object.get(key) {
            let text = value
                .as_str()
                .ok_or_else(|| format!("field \"{key}\" must be a string"))?;
            effects.additional_contexts.push(text.to_string());
        }
    }
    if let Some(specific) = object.get("hookSpecificOutput") {
        parse_hook_specific(event, specific, &mut effects)?;
    }
    Ok(effects)
}

fn parse_hook_specific(
    event: HookEvent,
    specific: &Value,
    effects: &mut HookEffects,
) -> Result<(), String> {
    let object = specific
        .as_object()
        .ok_or_else(|| "hookSpecificOutput must be an object".to_string())?;
    if let Some(name) = object.get("hookEventName").and_then(Value::as_str)
        && name != event.name()
    {
        return Err(format!(
            "hookSpecificOutput.hookEventName \"{name}\" does not match the fired event \"{}\"",
            event.name()
        ));
    }
    // permissionDecision only carries meaning on the two tool-gate events
    if matches!(event, HookEvent::PreToolUse | HookEvent::PermissionRequest)
        && let Some(value) = object.get("permissionDecision")
    {
        let decision = value
            .as_str()
            .ok_or_else(|| "permissionDecision must be a string".to_string())?;
        effects.permission = match decision {
            "allow" => Some(HookPermission::Allow),
            "ask" => Some(HookPermission::Ask),
            "deny" => Some(HookPermission::Deny),
            other => return Err(format!("unknown permissionDecision \"{other}\"")),
        };
        let reason = match object.get("permissionDecisionReason") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(_) => {
                return Err("permissionDecisionReason must be a string".to_string());
            }
        };
        // deny rejects the tool call (same end state as a block); the
        // reason, when given, is the model-facing note
        if effects.permission == Some(HookPermission::Deny) && effects.blocked.is_none() {
            effects.blocked = Some(
                reason
                    .map(|text| sanitize_reason(&text))
                    .unwrap_or_else(|| "Denied by a hook".to_string()),
            );
        }
    }
    if let Some(updated) = object.get("updatedInput").filter(|v| !v.is_null()) {
        effects.updated_input = Some(updated.clone());
    }
    if let Some(context) = object.get("additionalContext") {
        let text = context
            .as_str()
            .ok_or_else(|| "hookSpecificOutput.additionalContext must be a string".to_string())?;
        effects.additional_contexts.push(text.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(event: HookEvent, json: &str) -> Result<HookEffects, String> {
        parse_json_output(event, &serde_json::from_str(json).unwrap())
    }

    #[test]
    fn continue_false_blocks_with_reason() {
        let effects = parse(
            HookEvent::PreToolUse,
            r#"{"continue": false, "reason": "not allowed here"}"#,
        )
        .unwrap();
        assert_eq!(effects.blocked.as_deref(), Some("not allowed here"));
        assert!(!effects.stop_signal);
    }

    #[test]
    fn stop_event_inverts_block_semantics() {
        // continue:true at Stop asks for another round
        let effects = parse(HookEvent::Stop, r#"{"continue": true}"#).unwrap();
        assert!(effects.stop_signal);
        assert!(effects.blocked.is_none());
        // decision:block at Stop also means "don't stop", reason becomes context
        let effects = parse(
            HookEvent::Stop,
            r#"{"decision": "block", "reason": "run the tests first"}"#,
        )
        .unwrap();
        assert!(effects.stop_signal);
        assert_eq!(effects.additional_contexts, ["run the tests first"]);
        // continue:false at Stop is ignored
        let effects = parse(HookEvent::Stop, r#"{"continue": false}"#).unwrap();
        assert!(!effects.stop_signal);
    }

    #[test]
    fn permission_decision_and_updated_input() {
        let effects = parse(
            HookEvent::PreToolUse,
            r#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny",
                "updatedInput": {"command": "ls -la"}}}"#,
        )
        .unwrap();
        assert_eq!(effects.permission, Some(HookPermission::Deny));
        assert_eq!(
            effects.updated_input.as_ref().unwrap()["command"],
            serde_json::json!("ls -la")
        );
    }

    #[test]
    fn wrong_event_name_fails_the_hook() {
        let error = parse(
            HookEvent::PostToolUse,
            r#"{"hookSpecificOutput": {"hookEventName": "PreToolUse"}}"#,
        )
        .unwrap_err();
        assert!(error.contains("does not match"));
    }

    #[test]
    fn additional_context_both_spellings() {
        let effects = parse(
            HookEvent::UserPromptSubmit,
            r#"{"additionalContext": "one", "additional_context": "two"}"#,
        )
        .unwrap();
        assert_eq!(effects.additional_contexts, ["one", "two"]);
    }

    #[test]
    fn approve_answers_permission_request_only() {
        let effects = parse(HookEvent::PermissionRequest, r#"{"decision": "approve"}"#).unwrap();
        assert_eq!(effects.permission, Some(HookPermission::Allow));
        let effects = parse(HookEvent::PreToolUse, r#"{"decision": "approve"}"#).unwrap();
        assert_eq!(effects.permission, None);
    }

    #[test]
    fn merge_keeps_strongest_permission_and_first_block() {
        let mut chain = parse(
            HookEvent::PreToolUse,
            r#"{"continue": false, "reason": "first"}"#,
        )
        .unwrap();
        chain.merge(
            parse(
                HookEvent::PreToolUse,
                r#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow"}}"#,
            )
            .unwrap(),
        );
        assert_eq!(chain.blocked.as_deref(), Some("first"));
        assert_eq!(chain.permission, Some(HookPermission::Allow));
        chain.merge(
            parse(
                HookEvent::PreToolUse,
                r#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "protected"}}"#,
            )
            .unwrap(),
        );
        assert_eq!(chain.permission, Some(HookPermission::Deny));
        // An explicit deny with no earlier block contributes its reason
        let solo = parse(
            HookEvent::PreToolUse,
            r#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "protected"}}"#,
        )
        .unwrap();
        assert_eq!(solo.blocked.as_deref(), Some("protected"));
    }

    #[test]
    fn exit_two_synthesizes_reason_from_stderr() {
        let raw = HookRaw {
            outcome: RawOutcome::Exited(Some(2)),
            stdout: String::new(),
            stderr: "protected branch".into(),
        };
        let (effects, outcome) = interpret(HookEvent::PreToolUse, &raw);
        assert_eq!(outcome, Outcome::Blocked);
        assert_eq!(effects.blocked.as_deref(), Some("protected branch"));
        // Stop turns the same exit into a continuation context
        let (effects, outcome) = interpret(HookEvent::Stop, &raw);
        assert_eq!(outcome, Outcome::Blocked);
        assert!(effects.stop_signal);
        assert_eq!(effects.additional_contexts, ["protected branch"]);
    }

    #[test]
    fn non_zero_other_than_two_is_failure_not_block() {
        let raw = HookRaw {
            outcome: RawOutcome::Exited(Some(1)),
            stdout: String::new(),
            stderr: "boom".into(),
        };
        let (effects, outcome) = interpret(HookEvent::PreToolUse, &raw);
        assert_eq!(outcome, Outcome::Failed);
        assert!(effects.blocked.is_none());
    }

    #[test]
    fn plain_stdout_text_is_success_without_effect() {
        let raw = HookRaw {
            outcome: RawOutcome::Exited(Some(0)),
            stdout: "hello from hook".into(),
            stderr: String::new(),
        };
        let (effects, outcome) = interpret(HookEvent::PostToolUse, &raw);
        assert_eq!(outcome, Outcome::Finished);
        assert!(effects.blocked.is_none());
    }

    #[test]
    fn block_reasons_redact_credentials() {
        // exit-2 stderr path
        let raw = HookRaw {
            outcome: RawOutcome::Exited(Some(2)),
            stdout: String::new(),
            stderr: "denied: token=abc123, Authorization: Bearer xyz.abc".into(),
        };
        let (effects, _) = interpret(HookEvent::PreToolUse, &raw);
        let reason = effects.blocked.unwrap();
        // "Bearer" itself is consumed as the Authorization value; what matters
        // is that no secret material survives
        assert!(reason.contains("token=***"), "{reason}");
        assert!(
            !reason.contains("abc123") && !reason.contains("xyz.abc"),
            "{reason}"
        );
        // JSON reason path
        let effects = parse(
            HookEvent::PreToolUse,
            r#"{"continue": false, "reason": "api_key: sk-1234"}"#,
        )
        .unwrap();
        assert_eq!(effects.blocked.as_deref(), Some("api_key: ***"));
        // permissionDecisionReason deny path
        let effects = parse(
            HookEvent::PreToolUse,
            r#"{"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "password=hunter2"}}"#,
        )
        .unwrap();
        assert_eq!(effects.blocked.as_deref(), Some("password=***"));
        // Ordinary text passes through untouched
        assert_eq!(sanitize_reason("protected branch"), "protected branch");
    }
}
