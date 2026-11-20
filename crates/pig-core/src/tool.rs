use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use pig_protocol::{ApprovalPolicy, CoreError, PermissionSettings, SandboxMode};

use crate::task::SessionToolState;
use pig_provider::ToolCall;
use pig_utils::text::{FileEncoding, LineEnding};

const MAX_READ_LINES: usize = 2000;

/// Model-facing name of the shell tool. Built-in tool names are PascalCase
/// (`Read`, `TodoList`, `AgentSwarm`, …); one tool serves every platform and
/// dialect, with the shell as a per-call parameter.
pub const EXEC_COMMAND_TOOL: &str = "ExecCommand";
/// Model-facing name of the interactive PTY stdin/polling companion tool.
pub const WRITE_STDIN_TOOL: &str = "WriteStdin";

pub fn is_exec_command(name: &str) -> bool {
    name == EXEC_COMMAND_TOOL
}

const MAX_READ_CHARS: usize = 100_000;
/// Read per-line character cap; longer lines are truncated
const MAX_LINE_CHARS: usize = 2000;
const MAX_MATCH_RESULTS: usize = 200;
/// Shell foreground output character cap; beyond it, a head/tail preview is returned and the full output goes to a spill file
const MAX_EXEC_OUTPUT: usize = 30 * 1024;
const MAX_GREP_FILE_SIZE: u64 = 2 * 1024 * 1024;
/// Grep matched-line character cap (mirrors rg --max-columns): a single hit on
/// minified JS / a huge single-line JSON can blow up the context; overlong lines are truncated for display
const MAX_GREP_LINE_CHARS: usize = 500;
/// Grep/Glob total output character budget: context lines amplify output (200 hits × an 11-line window);
/// first come first served within the budget — past it, scanning stops with a pagination hint
const MAX_GREP_OUTPUT_CHARS: usize = 30 * 1024;

/// Truncate a Grep output line (annotate when over 500 characters)
fn truncate_grep_line(line: &str) -> String {
    if line.chars().count() > MAX_GREP_LINE_CHARS {
        let taken: String = line.chars().take(MAX_GREP_LINE_CHARS).collect();
        format!("{taken} [...line too long; truncated]")
    } else {
        line.to_string()
    }
}
/// File size cap before Read hits the disk: prevents "load the whole file into memory, then apply the output budget" from exhausting memory
const MAX_READ_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// Edit cap (whole read + whole write, more conservative than Read)
const MAX_EDIT_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Size-guard error construction: returns Some(message) when over the cap; each caller handles stat and the NotFound message itself.
fn file_size_error(len: u64, cap: u64, hint: &str) -> Option<String> {
    (len > cap).then(|| {
        format!(
            "File too large ({} MB, over the {} MB limit); {hint}",
            len / 1024 / 1024,
            cap / 1024 / 1024
        )
    })
}

pub struct FileChange {
    pub path: String,
    pub unified_diff: String,
    pub additions: u32,
    pub deletions: u32,
}

/// Image output from a tool (ReadMediaFile): enters model context with history
/// (Anthropic blocks / OpenAI split into user messages); rollout stores a media reference, not base64
pub struct ToolImage {
    pub media_type: String,
    pub data_base64: String,
    pub width: u32,
    pub height: u32,
}

/// PDF output from a tool (ReadMediaFile): the bytes enter model context as a
/// provider document; rollout and subagent JSONL store a reference, never base64.
pub struct ToolDocument {
    pub media_type: String,
    pub bytes: Vec<u8>,
    /// Stable wire file name (the source file's name)
    pub filename: String,
    pub label: Option<String>,
}

/// Media carried by a tool result. Keeping images and documents together means
/// every execution path passes both through instead of dropping one kind.
#[derive(Default)]
pub struct ToolOutputMedia {
    pub images: Vec<ToolImage>,
    pub documents: Vec<ToolDocument>,
    pub delivered_notification_ids: Vec<String>,
}

/// Input media capabilities of the model handling this call. Media is stripped
/// (with a visible text degradation) rather than sent to an unsupported model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaCapabilities {
    pub input_image: bool,
    pub input_pdf: bool,
}

impl MediaCapabilities {
    pub fn new(input_image: bool, input_pdf: bool) -> Self {
        Self {
            input_image,
            input_pdf,
        }
    }

    /// `ReadMediaFile` is reachable when either media kind is supported; the
    /// per-file capability is projected by the session layer after execution.
    pub fn any(&self) -> bool {
        self.input_image || self.input_pdf
    }
}

pub struct ToolEffect {
    pub output: String,
    pub file_change: Option<FileChange>,
    /// The diff of this edit itself ("before edit → after edit"), rendered inline by the UI tool card;
    /// `file_change` is the session-cumulative view (for the review panel) — the two differ in granularity
    pub edit_diff: Option<FileChange>,
    /// Image/PDF output (ReadMediaFile); always empty for other tools
    pub images: Vec<ToolImage>,
    pub documents: Vec<ToolDocument>,
    /// Terminal notification IDs delivered by this tool result (TaskWait only).
    pub delivered_notification_ids: Vec<String>,
}

impl ToolEffect {
    pub(crate) fn plain(output: String) -> Self {
        Self {
            output,
            file_change: None,
            edit_diff: None,
            images: vec![],
            documents: vec![],
            delivered_notification_ids: vec![],
        }
    }
}

impl From<FileChange> for pig_protocol::EditDiff {
    fn from(change: FileChange) -> Self {
        Self {
            path: change.path,
            unified_diff: change.unified_diff,
            additions: change.additions,
            deletions: change.deletions,
        }
    }
}

/// Compute the unified diff of a single edit (before edit → after edit), with relative paths normalized to `/`.
pub(crate) fn per_edit_diff(cwd: &Path, full: &Path, before: &str, after: &str) -> FileChange {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut additions = 0;
    let mut deletions = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => additions += 1,
            similar::ChangeTag::Delete => deletions += 1,
            _ => {}
        }
    }
    let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let relative = full
        .strip_prefix(&cwd_canonical)
        .or_else(|_| full.strip_prefix(cwd))
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| full.to_path_buf());
    let relative = relative.to_string_lossy().replace('\\', "/");
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{relative}"), &format!("b/{relative}"))
        .to_string();
    FileChange {
        path: relative,
        unified_diff: unified,
        additions,
        deletions,
    }
}

mod danger;
mod edit;
/// Session-level change tracking: snapshots the original bytes before the first modification; the diff is always "original → current" (LF view).
/// Snapshots are persisted on the session side via the dirty flag (file_originals table); restore rebuilds the baseline after restart.
/// Snapshots store raw bytes (not String), so GBK/UTF-16/binary files can all be reverted at the byte level;
/// the persistence boundary is still String, converted via snapshot_to_store/from_store (non-UTF-8 goes through hex).
///
/// There is also a second, **per-turn** tracking layer (same shape as ZCode turn-file-changes): before the first write within a turn
mod exec_command;
mod fetch;
mod media;
mod misc;
mod paths;
mod read;
mod search;
mod skill;
mod tracker;
mod write;
pub(crate) use write::atomic_write;

// Submodules are lifted to crate visibility wholesale (the root module's registry / cross-tool references);
// external API visibility stays as before, pinned by the explicit pub use below (zero call-site changes).
pub(crate) use fetch::*;
pub(crate) use media::*;
pub(crate) use misc::*;
pub(crate) use paths::*;
pub(crate) use read::*;
pub(crate) use tracker::*;

pub use danger::{DangerReason, is_dangerous_command};
pub use edit::{EditMatchError, EditOutcome, compute_edit};
pub use fetch::{check_fetch_url, extract_text, is_private_host, is_private_ip};
pub use misc::{AgentSwarmTool, AgentTool, SendMessageTool, parse_questions, parse_swarm_args};
pub use paths::{FsAccess, is_sensitive_file, resolve_checked, resolve_with_access};
pub use pig_utils::image::{
    CompressedImage, base64_encode, compress_image_for_model, convert_tiff_to_png,
    decode_image_check, encode_image_for_model, image_dimensions, sniff_image,
};
pub use search::search_files;
pub(crate) use skill::SkillTool;
pub use tracker::{ChangeTracker, snapshot_from_store, snapshot_to_store};

pub use pig_protocol::{TodoItem, TodoStatus};

/// Session-shared todo list (Arc handle, shared by Session and ToolContext).
pub type TodoHandle = std::sync::Arc<std::sync::Mutex<Vec<TodoItem>>>;

pub struct ToolContext<'a> {
    pub cwd: &'a Path,
    pub tracker: &'a mut ChangeTracker,
    pub state: &'a crate::task::SessionToolState,
    /// One-call out-of-workspace grant: Some(access) when the gate just got an
    /// approval for exactly this call's outside access — resolve_with_access
    /// lets this single call through. Approvals are per request, so the grant
    /// is never shared with another call.
    pub fs_grant: Option<paths::FsAccess>,
    pub sandbox_override: crate::task::SandboxOverride,
    /// Base filesystem policy for spawned model commands. Plan mode and
    /// ReadOnly pass `ReadOnly` so the kernel enforces writes the command
    /// classifier cannot; Auto uses `WorkspaceWrite`; Full passes
    /// `Unrestricted` and spawns without an OS sandbox.
    pub sandbox_profile: crate::task::SandboxProfile,
    /// Extra permissions granted to this one command by an approved
    /// `sandbox_permissions: with_additional_permissions` request.
    ///
    /// This widens the *sandbox* rather than leaving it: the overlay's `Write`
    /// entries join the command's writable set for this launch only, and every
    /// other restriction (sensitive-file denies, read-only carve-outs) still
    /// applies.
    pub additional_permissions: crate::task::AdditionalPermissions,
}

/// A model-requested widening of the sandbox for one command.
///
/// Codex's `sandbox_permissions` model: the model declares what it needs
/// instead of the host guessing from the command text, and the user decides.
/// The request is honored only as "these extra paths become writable inside the
/// sandbox" — never as "run without a sandbox" (ReadOnly and Auto keep it;
/// Full already runs without one and rejects the request as unnecessary).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxEscalation {
    /// Absolute paths to add to the writable set for this launch. A directory
    /// grants the subtree; a file grants exactly that file (the only shape
    /// accepted inside a protected credential directory). Roots the session
    /// already writes (workspace/tmp/extra roots in Auto) are dropped as
    /// redundant before any dialog.
    pub write_roots: Vec<std::path::PathBuf>,
    /// Absolute **files** to make readable for this launch. Only meaningful
    /// inside a protected credential directory, where reading is otherwise
    /// denied; this never grants write access.
    pub read_roots: Vec<std::path::PathBuf>,
    /// The model's one-line reason, shown in the approval dialog.
    pub justification: Option<String>,
    /// `sandbox_permissions: "require_escalated"`: run outside the sandbox for
    /// this one command (Codex's field). Approval is always required; when the
    /// policy carries read denials the run stays sandboxed with the denials
    /// preserved instead of dropping the filesystem boundary entirely.
    pub require_escalated: bool,
}

impl SandboxEscalation {
    /// True when the request only asks for read access (the dialog then says
    /// "read", matching what is actually granted).
    pub fn is_read_only(&self) -> bool {
        self.write_roots.is_empty() && !self.read_roots.is_empty()
    }
}

/// Parse an ExecCommand call's `sandbox_permissions` request.
///
/// Returns `Ok(None)` for the default (`use_default` or absent), `Ok(Some)`
/// for a well-formed request, and `Err` for a request that cannot be honored —
/// the error text goes back to the model verbatim so it can correct the call
/// instead of retrying blindly.
///
/// The request shape follows Codex: `additional_permissions.file_system.write`
/// lists the absolute paths this one command needs to write. A path may be a
/// directory (the subtree becomes writable) or a single file (that file only);
/// files are the only shape that can be approved inside a protected credential
/// directory. Reads are already available full-disk, so only write paths are
/// considered; a request without write paths is rejected. `require_escalated`
/// is the separate Codex request for one unsandboxed run.
pub fn sandbox_escalation(arguments: &str) -> Result<Option<SandboxEscalation>, String> {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let requested = args["sandbox_permissions"]
        .as_str()
        .unwrap_or("use_default");
    match requested {
        "use_default" | "" => Ok(None),
        "with_additional_permissions" => {
            let roots: Vec<std::path::PathBuf> =
                args["additional_permissions"]["file_system"]["write"]
                    .as_array()
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(|entry| entry.as_str())
                            .map(std::path::PathBuf::from)
                            .collect()
                    })
                    .unwrap_or_default();
            let read_roots: Vec<std::path::PathBuf> = args["additional_permissions"]["file_system"]
                ["read"]
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| entry.as_str())
                        .map(std::path::PathBuf::from)
                        .collect()
                })
                .unwrap_or_default();
            if roots.is_empty() && read_roots.is_empty() {
                return Err(
                    "sandbox_permissions `with_additional_permissions` requires at least one \
                     absolute path in `additional_permissions.file_system.write` (or `.read` for \
                     a single file inside a credential directory); omit the field to run normally"
                        .to_string(),
                );
            }
            for (field, paths) in [("write", &roots), ("read", &read_roots)] {
                for root in paths {
                    if !root.is_absolute() {
                        return Err(format!(
                            "additional_permissions.file_system.{field} must contain absolute \
                             paths; `{}` is relative",
                            root.display()
                        ));
                    }
                    if !root.is_dir() && !root.is_file() {
                        return Err(format!(
                            "additional_permissions.file_system.{field} must name existing \
                             paths; `{}` is missing",
                            root.display()
                        ));
                    }
                }
            }
            let justification = args["justification"]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            Ok(Some(SandboxEscalation {
                write_roots: roots,
                read_roots,
                justification,
                require_escalated: false,
            }))
        }
        // Codex's `RequireEscalated`: ask to run this one command without the
        // sandbox. The approval dialog is mandatory; when read denials are
        // active the attempt stays sandboxed with a full-disk write grant and
        // the denials preserved (Codex's EscalatedSandboxWithRestrictions).
        // `additional_permissions` is contradictory here and is rejected so
        // the approval names the authority that will actually be granted.
        "require_escalated" => {
            let additional = &args["additional_permissions"]["file_system"];
            if additional["write"]
                .as_array()
                .is_some_and(|entries| !entries.is_empty())
                || additional["read"]
                    .as_array()
                    .is_some_and(|entries| !entries.is_empty())
            {
                return Err(
                    "`require_escalated` runs without the sandbox; drop `additional_permissions` (they are only for `with_additional_permissions`)"
                        .to_string(),
                );
            }
            let justification = args["justification"]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            Ok(Some(SandboxEscalation {
                write_roots: Vec::new(),
                read_roots: Vec::new(),
                justification,
                require_escalated: true,
            }))
        }
        other => Err(format!(
            "unsupported sandbox_permissions `{other}`: use `with_additional_permissions` with `additional_permissions.file_system.write` to widen the sandbox for one command, `require_escalated` to ask for one unsandboxed run, or omit the field"
        )),
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    /// Read-only tools are exempt from approval in every mode
    fn read_only(&self) -> bool {
        false
    }
    fn is_shell(&self) -> bool {
        false
    }
    fn is_plugin(&self) -> bool {
        false
    }
    /// The declaration a plugin tool was registered with (approval, timeout,
    /// concurrency, output schema); `None` for host tools.
    fn plugin_tool(&self) -> Option<&pig_protocol::PluginTool> {
        None
    }
    /// Selective strict-schema preference. Only tools whose schemas are fully
    /// expressible in the strict subset opt in (`Read`/`Write`/`Edit`/
    /// `ExecCommand`); the provider still downgrades to the ordinary schema
    /// when the endpoint/model capability or the schema shape does not allow it.
    fn prefer_strict(&self) -> bool {
        false
    }
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>>;
}

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadFile),
        Box::new(WriteFile),
        Box::new(EditFile),
        Box::new(Glob),
        Box::new(Grep),
        Box::new(ExecCommand),
        Box::new(exec_command::WriteStdin),
        Box::new(TodoListTool),
        Box::new(FetchUrl),
        Box::new(TaskList),
        Box::new(TaskOutput),
        Box::new(TaskWait),
        Box::new(TaskStop),
        Box::new(AskUserQuestionTool),
        Box::new(ExitPlanModeTool),
        Box::new(EnterPlanModeTool),
        Box::new(ReadMediaFile),
    ]
}

pub fn schemas() -> Vec<serde_json::Value> {
    all()
        .iter()
        .map(|tool| schema_with_strict(tool.as_ref()))
        .collect()
}

/// One tool's request schema: its wire shape plus an optional per-tool
/// `function.strict` declaration, consumed by the provider's strict encoder.
/// Only an opted-in tool is declared; everything else keeps the ordinary schema.
pub fn schema_with_strict(tool: &dyn Tool) -> serde_json::Value {
    let mut schema = tool.schema();
    if tool.prefer_strict() {
        schema["function"]["strict"] = serde_json::Value::Bool(true);
    }
    schema
}

#[cfg(test)]
struct StrictTool;

#[cfg(test)]
impl Tool for StrictTool {
    fn name(&self) -> &str {
        "StrictTool"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {"name": "StrictTool", "parameters": {"type": "object"}},
        })
    }

    fn prefer_strict(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        unimplemented!()
    }
}

#[test]
fn schema_with_strict_reads_the_real_tool_declaration() {
    let schema = schema_with_strict(&StrictTool);
    assert_eq!(schema["function"]["strict"], serde_json::json!(true));
}

#[test]
fn only_the_declared_tools_opt_into_strict() {
    let declared: Vec<String> = all()
        .iter()
        .filter(|tool| tool.prefer_strict())
        .map(|tool| tool.name().to_owned())
        .collect();
    assert_eq!(declared, ["Read", "Write", "Edit", "ExecCommand"]);
    for tool in all() {
        let schema = schema_with_strict(tool.as_ref());
        if tool.prefer_strict() {
            assert_eq!(schema["function"]["strict"], serde_json::json!(true));
        } else {
            assert!(
                schema["function"].get("strict").is_none(),
                "undeclared tool {} must not be strictified",
                tool.name()
            );
        }
    }
}

/// Root session tool set = all built-in tools + Agent/AgentSwarm + Skill (subagent loops use a
/// narrowed all() with Skill added separately at the spawn point, naturally excluding Agent to prevent nesting).
/// profiles is a **session-frozen snapshot** passed by the caller: the profile list is embedded in the
/// Agent/AgentSwarm descriptions, and re-scanning disk every step would let editing a subagent profile break the tools prefix cache
/// (tools sit at the very front of the cache prefix; any change invalidates everything from byte 0; the same
/// trade-off as kimi frozenCatalogProfiles / ZCode startup assembly). At spawn time execution re-reads
/// via load_profiles, so a stale list self-heals through the "profile does not exist" error
pub fn all_root(
    cwd: &Path,
    profiles: &[crate::agent::AgentProfile],
    plugins: &crate::plugins::SharedPluginRegistry,
) -> Vec<Box<dyn Tool>> {
    all_root_with_summary(
        cwd,
        &crate::agent::agent_description_list(profiles),
        plugins,
    )
}

pub fn all_root_with_summary(
    cwd: &Path,
    profiles_summary: &str,
    plugins: &crate::plugins::SharedPluginRegistry,
) -> Vec<Box<dyn Tool>> {
    let mut tools = all();
    tools.push(Box::new(AgentTool::from_summary(profiles_summary)));
    tools.push(Box::new(AgentSwarmTool::from_summary(profiles_summary)));
    tools.push(Box::new(SendMessageTool));
    tools.push(Box::new(SkillTool::new(cwd, plugins)));
    tools
}

/// Root session tool schema set (including Agent); parameters match all_root
pub fn schemas_root(
    cwd: &Path,
    profiles: &[crate::agent::AgentProfile],
    plugins: &crate::plugins::SharedPluginRegistry,
) -> Vec<serde_json::Value> {
    all_root(cwd, profiles, plugins)
        .iter()
        .map(|tool| schema_with_strict(tool.as_ref()))
        .collect()
}

/// Approval decision per tool class, from the canonical permission state.
/// Plan mode hard-rejects whole categories earlier (orthogonal to permissions);
/// only the permission tier matters here.
/// The read-only sandbox runs model Bash without routine approvals (the model
/// must request writes explicitly) and asks for the other non-read-only tools.
/// Workspace-write and full access do not ask routinely; full access still
/// rejects dangerous commands at the session layer.
pub fn requires_approval(tool: &dyn Tool, permissions: PermissionSettings) -> bool {
    // Agent/AgentSwarm are always approval-free: each write operation inside the subagent goes through its own approval gate,
    // so the delegated call itself is not approved twice (dialog copy could not describe the whole subtask anyway)
    if tool.name() == "Agent" || tool.name() == "AgentSwarm" {
        return false;
    }
    if permissions.approval == ApprovalPolicy::Never {
        return false;
    }
    // A plugin tool declares its own approval stance. `never` is accepted only
    // for read-only tools (checked at install); `always` asks in every mode
    // that asks at all.
    if let Some(declaration) = tool.plugin_tool() {
        match declaration.approval {
            pig_protocol::PluginToolApproval::Never => return false,
            pig_protocol::PluginToolApproval::Always => {
                return permissions.approval == ApprovalPolicy::OnRequest;
            }
            pig_protocol::PluginToolApproval::OnRequest => {}
        }
    }
    // Shell commands carry the model's own escalation decision: no routine
    // prompt, the read-only sandbox blocks unrequested writes. File tools
    // and MCP writes are not sandboxed, so they still ask.
    permissions.sandbox.mode == SandboxMode::ReadOnly && !tool.read_only() && !tool.is_shell()
}

pub fn summarize(call: &ToolCall) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let raw = match call.name.as_str() {
        "Read" | "Write" | "Edit" | "ReadMediaFile" => {
            args["path"].as_str().unwrap_or("?").to_string()
        }
        name if is_exec_command(name) => args["command"].as_str().unwrap_or("?").to_string(),
        WRITE_STDIN_TOOL => {
            let sid = args["session_id"].as_str().unwrap_or("?");
            match args["chars"].as_str().filter(|s| !s.is_empty()) {
                Some(chars) => {
                    let preview = chars.replace('\r', "\\r").replace('\n', "\\n");
                    format!("{sid} ← {preview}")
                }
                None => format!("{sid} (poll)"),
            }
        }
        "Glob" => args["pattern"].as_str().unwrap_or("?").to_string(),
        "Grep" => args["pattern"].as_str().unwrap_or("?").to_string(),
        "TodoList" => args["todos"]
            .as_array()
            .map(|items| format!("update todos ({} items)", items.len()))
            .unwrap_or_else(|| "view todos".to_string()),
        "FetchURL" => args["url"].as_str().unwrap_or("?").to_string(),
        "Skill" => args["skill"].as_str().unwrap_or("?").to_string(),
        "TaskList" => "list background tasks".to_string(),
        "TaskOutput" | "TaskStop" => args["task_id"].as_str().unwrap_or("?").to_string(),
        "AskUserQuestion" => args["questions"][0]["question"]
            .as_str()
            .unwrap_or("?")
            .to_string(),
        "ExitPlanMode" => "requesting to exit plan mode".to_string(),
        "EnterPlanMode" => "requesting to enter plan mode".to_string(),
        "Agent" => format!(
            "subagent {}: {}",
            args["subagent_type"]
                .as_str()
                .or_else(|| args["resume"].as_str())
                .unwrap_or("general-purpose"),
            args["description"].as_str().unwrap_or("?")
        ),
        "AgentSwarm" => format!(
            "subagent swarm ({} items)",
            args["items"].as_array().map(|a| a.len()).unwrap_or(0)
                + args["resume_agent_ids"]
                    .as_object()
                    .map(|m| m.len())
                    .unwrap_or(0)
        ),
        _ => args.to_string(),
    };
    // No truncation at the source: the UI ellipsizes the collapsed line to one row while the expanded card shows the full text;
    // the full text is persisted in arguments with the rollout, so the summary is not shortened further
    raw
}

/// Compatibility entry: built-in tools only (for integration tests); runtime paths (gating / concurrent read-only sections) go through execute_with_extra
pub async fn execute(
    call: &ToolCall,
    ctx: ToolContext<'_>,
) -> (
    String,
    bool,
    Option<FileChange>,
    Option<pig_protocol::EditDiff>,
    ToolOutputMedia,
) {
    execute_with_extra(call, ctx, &[]).await
}

pub async fn execute_with_extra(
    call: &ToolCall,
    ctx: ToolContext<'_>,
    // Runtime tools beyond the built-ins (MCP and plugins): fallback lookup by name
    extra: &[Box<dyn Tool>],
) -> (
    String,
    bool,
    Option<FileChange>,
    Option<pig_protocol::EditDiff>,
    ToolOutputMedia,
) {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let tools = all();
    let Some(tool) = tools
        .iter()
        .chain(extra.iter())
        .find(|tool| tool.name() == call.name.as_str())
    else {
        return (
            format!("Unknown tool: {}", call.name),
            true,
            None,
            None,
            ToolOutputMedia::default(),
        );
    };
    match tool.execute(args, ctx).await {
        Ok(effect) => (
            effect.output,
            false,
            effect.file_change,
            effect.edit_diff.map(Into::into),
            ToolOutputMedia {
                images: effect.images,
                documents: effect.documents,
                delivered_notification_ids: effect.delivered_notification_ids,
            },
        ),
        Err(error) => (error, true, None, None, ToolOutputMedia::default()),
    }
}

/// Out-of-bounds shapes from resolve_core (for the policy layer to distinguish; messages are decided by the caller)
enum BoundFailure {
    /// Parent directory, once canonicalized, lies outside the workspace
    ParentOutside {
        resolved: PathBuf,
    },
    /// Target already exists (including symlinks) and canonicalizes outside the workspace
    TargetOutside {
        resolved: PathBuf,
    },
    /// Dangling symlink: fail-closed; the policy layer does not let it through either
    DanglingSymlink,
    Other(String),
}

struct ReadFile;
struct WriteFile;
struct EditFile;
struct Glob;
struct Grep;
struct ExecCommand;

/// The single path a Write/Edit call targets, resolved the way the tool resolves
/// it (absolute, or relative to `cwd`), without touching the filesystem.
///
/// Used by the gate to classify a write against the credential directories:
/// one target path is a single file, never a subtree.
pub fn write_target_path(cwd: &std::path::Path, call: &ToolCall) -> Option<std::path::PathBuf> {
    file_tool_target(cwd, call)
}

/// The path a file tool will touch, resolved the way the tool resolves it
/// (absolute, or relative to `cwd`).
///
/// Returned for the tools that name exactly one path — `Write`, `Edit`, `Read`,
/// `ReadMediaFile`, `Grep`, `Glob` — so the gate can classify it before the tool
/// runs. `Grep`/`Glob` may omit `path`, in which case the workspace root is the
/// target and nothing needs classifying.
///
/// Read-tool targets are canonicalized to the final object (a symlink resolves
/// to its target), because the tools verify that object and a credential
/// approval has to name it too. `Write`/`Edit` keep the lexical spelling: their
/// write is a rename over the named entry, so resolving the link would change
/// which object is replaced.
pub fn file_tool_target(cwd: &std::path::Path, call: &ToolCall) -> Option<std::path::PathBuf> {
    if !matches!(
        call.name.as_str(),
        "Write" | "Edit" | "Read" | "ReadMediaFile" | "Grep" | "Glob"
    ) {
        return None;
    }
    let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    let raw = args["path"].as_str()?;
    let raw = std::path::Path::new(raw);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .join(raw)
    };
    let lexical = paths::lexical_normalize_public(&joined);
    if matches!(
        call.name.as_str(),
        "Read" | "ReadMediaFile" | "Grep" | "Glob"
    ) {
        Some(paths::canonical_read_target_for_gate(&lexical))
    } else {
        Some(lexical)
    }
}

pub fn approval_subject(call: &ToolCall) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    match call.name.as_str() {
        name if is_exec_command(name) => args["command"]
            .as_str()
            .and_then(|command| command.split_whitespace().next())
            .unwrap_or("")
            .to_string(),
        WRITE_STDIN_TOOL => args["session_id"].as_str().unwrap_or("").to_string(),
        "Write" | "Edit" => args["path"].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod sandbox_escalation_tests {
    use super::sandbox_escalation;

    #[test]
    fn default_and_absent_requests_are_no_ops() {
        assert!(sandbox_escalation("{}").unwrap().is_none());
        assert!(
            sandbox_escalation(r#"{"command":"ls","sandbox_permissions":"use_default"}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_well_formed_request_carries_roots_and_reason() {
        let dir = std::env::temp_dir();
        let args = serde_json::json!({
            "command": "touch x",
            "sandbox_permissions": "with_additional_permissions",
            "additional_permissions": { "file_system": { "write": [dir.to_string_lossy()] } },
            "justification": "writes build output next to the compiler"
        })
        .to_string();
        let request = sandbox_escalation(&args).unwrap().expect("request");
        assert_eq!(request.write_roots, vec![dir]);
        assert_eq!(
            request.justification.as_deref(),
            Some("writes build output next to the compiler")
        );
    }

    #[test]
    fn missing_or_relative_roots_are_rejected_with_a_reason() {
        let bare = serde_json::json!({
            "command": "touch x",
            "sandbox_permissions": "with_additional_permissions"
        })
        .to_string();
        let error = sandbox_escalation(&bare).unwrap_err();
        assert!(
            error.contains("additional_permissions.file_system.write"),
            "{error}"
        );

        let relative = serde_json::json!({
            "command": "touch x",
            "sandbox_permissions": "with_additional_permissions",
            "additional_permissions": { "file_system": { "write": ["relative/path"] } }
        })
        .to_string();
        let error = sandbox_escalation(&relative).unwrap_err();
        assert!(error.contains("absolute"), "{error}");

        let missing = serde_json::json!({
            "command": "touch x",
            "sandbox_permissions": "with_additional_permissions",
            "additional_permissions": { "file_system": { "write": [std::env::temp_dir().join("pig-definitely-missing-root").to_string_lossy()] } }
        })
        .to_string();
        let error = sandbox_escalation(&missing).unwrap_err();
        assert!(error.contains("missing"), "{error}");
    }

    /// Codex's `RequireEscalated`: parsed as its own request, and
    /// `additional_permissions` is rejected because the approved authority is
    /// "no sandbox at all", not a per-command widening.
    #[test]
    fn require_escalated_is_parsed_without_additional_permissions() {
        let args = serde_json::json!({
            "command": "touch /tmp/x",
            "sandbox_permissions": "require_escalated",
            "justification": "needs the host toolchain"
        })
        .to_string();
        let request = sandbox_escalation(&args).unwrap().expect("request");
        assert!(request.require_escalated);
        assert!(request.write_roots.is_empty() && request.read_roots.is_empty());
        assert_eq!(
            request.justification.as_deref(),
            Some("needs the host toolchain")
        );

        let conflicting = serde_json::json!({
            "command": "touch /tmp/x",
            "sandbox_permissions": "require_escalated",
            "additional_permissions": { "file_system": { "write": ["/tmp"] } }
        })
        .to_string();
        let error = sandbox_escalation(&conflicting).unwrap_err();
        assert!(error.contains("drop `additional_permissions`"), "{error}");
    }
}

#[cfg(all(test, unix))]
mod outside_intent_tests {
    use super::*;

    /// A workspace-local symlinked parent resolves outside the workspace: the
    /// approval and the write both act on the canonical target, so the dialog
    /// must name that target rather than the link spelling.
    #[test]
    fn a_symlinked_parent_names_the_canonical_write_target() {
        let base = std::env::temp_dir().join(format!("pig-tool-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let workspace = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, workspace.join("linked")).expect("link");

        let state = crate::task::SessionToolState::for_test();
        let arguments = serde_json::json!({"path": "linked/report.md"}).to_string();
        let (access, named) = fs_outside_intent(&state, &workspace, "Write", &arguments)
            .expect("an outside write needs approval");
        assert!(matches!(access, FsAccess::Write));
        assert_eq!(
            std::path::PathBuf::from(&named),
            outside
                .canonicalize()
                .expect("canonical outside")
                .join("report.md"),
            "the dialog must name the object the tool writes"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
