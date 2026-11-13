use super::*;

/// Context for gated execution (the argument bundle of exec_tool_gated_ctx):
/// a root or child Session assembles it from its own fields. Policy state that
/// must be shared across agents (pending approvals, permission rules, the
/// always-allowed set) is Arc-backed in the Session.
pub(crate) struct GateCtx<'a> {
    pub cwd: &'a Path,
    /// Canonical permission state for this turn (sandbox + approval + reviewer)
    pub permissions: PermissionSettings,
    /// Input media capabilities of the model handling these calls; a tool
    /// result carrying an unsupported media kind is degraded to text.
    pub media: tool::MediaCapabilities,
    pub tracker: &'a mut ChangeTracker,
    pub state: &'a crate::task::SessionToolState,
    /// Only the root session owns the persisted todo snapshot and emits root
    /// TodoListChanged events. Child gates point `state` at a child-local
    /// clone (its own TodoHandle) and set this false.
    pub owns_todo_state: bool,
    pub pending: &'a PendingApprovals,
    /// User/project allow+deny rules (Codex's execpolicy analogue), separate
    /// from the permission state above. Shared through the session so a rule
    /// learned by one agent governs the root and every other child.
    pub permission_rules: &'a Mutex<crate::permissions::PermissionRules>,
    pub always_allowed: &'a Mutex<HashSet<(String, String)>>,
    pub session_id: &'a str,
    /// Plan mode toggle: when Write/Edit writes the plan file (kimi
    /// writesOnlyPlanFile), the approval gate passes it through approval-free
    /// (path check in tool::is_plan_file_write)
    pub plan_enabled: bool,
    pub store: &'a Arc<Mutex<Store>>,
    /// Runtime tools beyond the built-ins (MCP and plugins): lookup fallback
    /// by name in the execution segment. The root passes all connected tools;
    /// subagents pass the subset narrowed by profile inheritance rules.
    pub extra_tools: &'a [Box<dyn tool::Tool>],
    /// Lifecycle hook snapshot (PreToolUse/PermissionRequest/PostToolUse fire
    /// inside the gate; shared by the parent session and subagent drives)
    pub hooks: &'a crate::hooks::HookSet,
    /// Conversation history for the hooks' transcript_path snapshot (parent
    /// session's history; background drives carry a detach-time snapshot)
    pub history: &'a [ChatMsg],
}
