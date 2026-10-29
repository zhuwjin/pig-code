//! Hook stdin payload: one-line JSON + `\n`, Claude-compatible snake_case
//! field names (existing hook scripts work unmodified). Environment variables
//! PIG_SESSION_ID / PIG_PROJECT_DIR (plus CLAUDE_* aliases) ride along on the
//! process itself; `transcript_path` points at a per-run JSONL snapshot of
//! the conversation (written by the runner, deleted when the run ends).

use std::path::PathBuf;

use pig_protocol::PermissionSettings;
use serde_json::{Value, json};

use super::HookEvent;

#[derive(Clone)]
pub struct HookPayload {
    pub event: HookEvent,
    pub session_id: String,
    pub cwd: PathBuf,
    /// Canonical permission state (sandbox / approval policy / reviewer).
    pub permissions: PermissionSettings,
    /// UserPromptSubmit
    pub prompt: Option<String>,
    /// SessionStart: "startup" | "resume"
    pub source: Option<&'static str>,
    /// Tool events
    pub tool_name: Option<String>,
    /// Parsed tool arguments (falls back to the raw string when unparseable)
    pub tool_input: Option<Value>,
    pub tool_input_raw: Option<String>,
    pub tool_use_id: Option<String>,
    /// PostToolUse / PostToolUseFailure
    pub tool_response: Option<String>,
    pub is_interrupt: bool,
    /// Stop
    pub last_assistant_message: Option<String>,
    pub stop_hook_active: bool,
    /// Conversation snapshot as pre-serialized JSONL lines (empty = the
    /// payload carries no transcript_path); the runner writes them to a
    /// per-run temp file
    pub transcript_lines: Vec<String>,
}

impl HookPayload {
    fn base(
        event: HookEvent,
        session_id: &str,
        cwd: &std::path::Path,
        permissions: PermissionSettings,
    ) -> Self {
        Self {
            event,
            session_id: session_id.to_string(),
            cwd: cwd.to_path_buf(),
            permissions,
            prompt: None,
            source: None,
            tool_name: None,
            tool_input: None,
            tool_input_raw: None,
            tool_use_id: None,
            tool_response: None,
            is_interrupt: false,
            last_assistant_message: None,
            stop_hook_active: false,
            transcript_lines: Vec::new(),
        }
    }

    /// Attach a conversation snapshot (the runner materializes it as the
    /// transcript file). UserPromptSubmit additionally appends the pending
    /// prompt as the final user line — the message is not in history yet at
    /// hook time
    pub fn with_transcript(mut self, history: &[pig_provider::ChatMsg]) -> Self {
        self.transcript_lines = super::transcript::transcript_lines(history);
        if let Some(prompt) = &self.prompt {
            self.transcript_lines.push(
                json!({
                    "type": "user",
                    "message": { "role": "user", "content": prompt },
                })
                .to_string(),
            );
        }
        self
    }

    pub fn session_start(
        session_id: &str,
        cwd: &std::path::Path,
        permissions: PermissionSettings,
        source: &'static str,
    ) -> Self {
        let mut payload = Self::base(HookEvent::SessionStart, session_id, cwd, permissions);
        payload.source = Some(source);
        payload
    }

    pub fn user_prompt(
        session_id: &str,
        cwd: &std::path::Path,
        permissions: PermissionSettings,
        prompt: &str,
    ) -> Self {
        let mut payload = Self::base(HookEvent::UserPromptSubmit, session_id, cwd, permissions);
        payload.prompt = Some(prompt.to_string());
        payload
    }

    pub fn tool(
        event: HookEvent,
        session_id: &str,
        cwd: &std::path::Path,
        permissions: PermissionSettings,
        tool_name: &str,
        arguments: &str,
        tool_use_id: &str,
    ) -> Self {
        let mut payload = Self::base(event, session_id, cwd, permissions);
        payload.tool_name = Some(tool_name.to_string());
        payload.tool_use_id = Some(tool_use_id.to_string());
        match serde_json::from_str::<Value>(arguments) {
            Ok(value) => payload.tool_input = Some(value),
            Err(_) => payload.tool_input_raw = Some(arguments.to_string()),
        }
        payload
    }

    pub fn with_response(mut self, output: &str) -> Self {
        self.tool_response = Some(output.to_string());
        self
    }

    pub fn with_interrupt(mut self, is_interrupt: bool) -> Self {
        self.is_interrupt = is_interrupt;
        self
    }

    pub fn stop(
        session_id: &str,
        cwd: &std::path::Path,
        permissions: PermissionSettings,
        last_assistant_message: &str,
        stop_hook_active: bool,
    ) -> Self {
        let mut payload = Self::base(HookEvent::Stop, session_id, cwd, permissions);
        payload.last_assistant_message = Some(last_assistant_message.to_string());
        payload.stop_hook_active = stop_hook_active;
        payload
    }

    pub fn to_json_line(&self, transcript_path: Option<&str>) -> String {
        let mut payload = json!({
            "hook_event_name": self.event.name(),
            "session_id": self.session_id,
            "cwd": self.cwd.display().to_string(),
            "permissions": self.permissions,
        });
        let object = payload.as_object_mut().expect("payload is an object");
        if let Some(path) = transcript_path {
            object.insert("transcript_path".into(), json!(path));
        }
        if let Some(prompt) = &self.prompt {
            object.insert("prompt".into(), json!(prompt));
        }
        if let Some(source) = self.source {
            object.insert("source".into(), json!(source));
        }
        if let Some(name) = &self.tool_name {
            object.insert("tool_name".into(), json!(name));
        }
        if let Some(input) = &self.tool_input {
            object.insert("tool_input".into(), input.clone());
        }
        if let Some(raw) = &self.tool_input_raw {
            object.insert("tool_input_raw".into(), json!(raw));
        }
        if let Some(id) = &self.tool_use_id {
            object.insert("tool_use_id".into(), json!(id));
        }
        if let Some(response) = &self.tool_response {
            object.insert("tool_response".into(), json!(response));
        }
        if self.tool_response.is_some() {
            object.insert("is_interrupt".into(), json!(self.is_interrupt));
        }
        if let Some(message) = &self.last_assistant_message {
            object.insert("last_assistant_message".into(), json!(message));
            object.insert("stop_hook_active".into(), json!(self.stop_hook_active));
        }
        let mut line = serde_json::to_string(&payload).expect("payload serializes");
        line.push('\n');
        line
    }
}
