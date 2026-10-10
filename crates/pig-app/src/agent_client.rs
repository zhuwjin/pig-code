use std::path::PathBuf;

use pig_protocol::{ApprovalDecision, ExecMode, Op};

/// Thin UI-side wrapper around the agent thread (multi-session; all operations
/// carry a session_id).
#[derive(Clone)]
pub struct AgentClient {
    ops: async_channel::Sender<Op>,
}

impl AgentClient {
    pub fn new(ops: async_channel::Sender<Op>) -> Self {
        Self { ops }
    }

    fn send(&self, op: Op) {
        let _ = self.ops.send_blocking(op);
    }

    pub fn new_session(
        &self,
        cwd: PathBuf,
        provider_id: Option<String>,
        model_id: Option<String>,
        reasoning_level: Option<String>,
        exec_mode: Option<ExecMode>,
        plan_enabled: Option<bool>,
    ) {
        self.send(Op::NewSession {
            cwd,
            provider_id,
            model_id,
            reasoning_level,
            exec_mode,
            plan_enabled,
        });
    }

    pub fn open_session(&self, session_id: String) {
        self.send(Op::OpenSession { session_id });
    }

    pub fn list_sessions(&self) {
        self.send(Op::ListSessions);
    }

    pub fn set_pinned(&self, session_id: &str, pinned: bool) {
        self.send(Op::UpdateSessionMeta {
            session_id: session_id.to_string(),
            pinned: Some(pinned),
            archived: None,
            title: None,
        });
    }

    pub fn set_archived(&self, session_id: &str, archived: bool) {
        self.send(Op::UpdateSessionMeta {
            session_id: session_id.to_string(),
            pinned: None,
            archived: Some(archived),
            title: None,
        });
    }

    /// Manual rename (core sets title_custom; auto-naming no longer overrides it)
    pub fn rename_session(&self, session_id: &str, title: &str) {
        self.send(Op::UpdateSessionMeta {
            session_id: session_id.to_string(),
            pinned: None,
            archived: None,
            title: Some(title.to_string()),
        });
    }

    pub fn delete_session(&self, session_id: &str) {
        self.send(Op::DeleteSession {
            session_id: session_id.to_string(),
        });
    }

    /// Session fork: derive a new session from the source session's first
    /// `turns` turns of history and switch to it
    pub fn fork_session(&self, session_id: &str, turns: usize) {
        self.send(Op::ForkSession {
            session_id: session_id.to_string(),
            turns,
        });
    }

    pub fn send_message(
        &self,
        session_id: String,
        content: String,
        files: Vec<String>,
        images: Vec<pig_protocol::PendingImage>,
        mode: ExecMode,
    ) {
        self.send(Op::SendMessage {
            session_id,
            content,
            files,
            images,
            mode,
        });
    }

    pub fn interrupt(&self, session_id: String) {
        self.send(Op::Interrupt { session_id });
    }

    /// Skip the session's active model-call retry wait (no-op when none is in flight)
    pub fn retry_now(&self, session_id: String) {
        self.send(Op::RetryNow { session_id });
    }

    /// Load a background subagent's full conversation (read-only display in the
    /// right "Subagent" tab)
    pub fn load_subagent(&self, session_id: String, agent_id: String) {
        self.send(Op::LoadSubagent {
            session_id,
            agent_id,
        });
    }

    pub fn set_model(
        &self,
        session_id: String,
        provider_id: String,
        model_id: String,
        reasoning_level: Option<String>,
    ) {
        self.send(Op::SetModel {
            session_id,
            provider_id,
            model_id,
            reasoning_level,
        });
    }

    pub fn set_reasoning(&self, session_id: String, reasoning_level: Option<String>) {
        self.send(Op::SetReasoning {
            session_id,
            reasoning_level,
        });
    }

    pub fn get_config(&self) {
        self.send(Op::GetConfig);
    }

    pub fn save_config(&self, config: pig_protocol::AppConfig) {
        self.send(Op::SaveConfig { config });
    }

    pub fn test_provider(&self, provider_id: String) {
        self.send(Op::TestProvider { provider_id });
    }

    pub fn model_lookup(&self, id: String) {
        self.send(Op::ModelLookup { id });
    }

    pub fn set_exec_mode(&self, session_id: String, mode: ExecMode) {
        self.send(Op::SetExecMode { session_id, mode });
    }

    /// Plan mode toggle (orthogonal to exec mode; core writes through to the
    /// store, no event back)
    pub fn set_plan_mode(&self, session_id: String, enabled: bool) {
        self.send(Op::SetPlanMode {
            session_id,
            enabled,
        });
    }

    pub fn set_fs_access(&self, session_id: String, read_outside: bool, write_outside: bool) {
        self.send(Op::SetFsAccess {
            session_id,
            read_outside,
            write_outside,
        });
    }

    /// Reply with an approval decision (feedback is non-None only on the plan
    /// "Revise" path; kimi Revise carries it to the model)
    pub fn approval_reply(
        &self,
        request_id: String,
        decision: ApprovalDecision,
        feedback: Option<String>,
    ) {
        self.send(Op::ApprovalReply {
            request_id,
            decision,
            feedback,
        });
    }

    pub fn question_reply(&self, request_id: String, answers: Option<Vec<Vec<String>>>) {
        self.send(Op::QuestionReply {
            request_id,
            answers,
        });
    }

    pub fn search_files(&self, session_id: String, query: String, cwd: Option<String>) {
        self.send(Op::SearchFiles {
            session_id,
            query,
            cwd,
        });
    }

    pub fn compact(&self, session_id: String, instruction: Option<String>) {
        self.send(Op::Compact {
            session_id,
            instruction,
        });
    }

    /// Query the list of MCP server names connected for a session (shown in the
    /// settings page)
    pub fn list_mcp_servers(&self, session_id: String) {
        self.send(Op::ListMcpServers { session_id });
    }

    pub fn cancel_queued(&self, session_id: String, text: String) {
        self.send(Op::CancelQueued { session_id, text });
    }

    pub fn git_info(&self, cwd: PathBuf) {
        self.send(Op::GitInfo { cwd });
    }

    pub fn git_status(&self, cwd: PathBuf) {
        self.send(Op::GitStatus { cwd });
    }

    pub fn git_diff(&self, cwd: PathBuf, path: String, staged: bool) {
        self.send(Op::GitDiff { cwd, path, staged });
    }

    pub fn checkout_branch(&self, cwd: PathBuf, branch: String) {
        self.send(Op::CheckoutBranch { cwd, branch });
    }

    pub fn list_workspaces(&self) {
        self.send(Op::ListWorkspaces);
    }

    pub fn add_workspace(&self, path: PathBuf) {
        self.send(Op::AddWorkspace { path });
    }

    pub fn remove_workspace(&self, path: PathBuf) {
        self.send(Op::RemoveWorkspace { path });
    }

    pub fn rename_workspace(&self, path: PathBuf, alias: Option<String>) {
        self.send(Op::RenameWorkspace { path, alias });
    }
}
