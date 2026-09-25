use std::path::PathBuf;

use pig_protocol::{ApprovalDecision, ExecMode, Op};

/// UI 侧对 agent 线程的薄封装（多会话；所有操作带 session_id）。
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
    ) {
        self.send(Op::NewSession {
            cwd,
            provider_id,
            model_id,
            reasoning_level,
            exec_mode,
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

    /// 手动重命名（core 置 title_custom，自动命名不再覆盖）
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

    pub fn set_fs_access(&self, session_id: String, read_outside: bool, write_outside: bool) {
        self.send(Op::SetFsAccess {
            session_id,
            read_outside,
            write_outside,
        });
    }

    pub fn approval_reply(&self, request_id: String, decision: ApprovalDecision) {
        self.send(Op::ApprovalReply {
            request_id,
            decision,
        });
    }

    pub fn question_reply(&self, request_id: String, answers: Option<Vec<Vec<String>>>) {
        self.send(Op::QuestionReply {
            request_id,
            answers,
        });
    }

    pub fn search_files(&self, session_id: String, query: String) {
        self.send(Op::SearchFiles { session_id, query });
    }

    pub fn compact(&self, session_id: String) {
        self.send(Op::Compact { session_id });
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
