use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::Ordering;

use pig_protocol::{CoreError, ExecMode};

use crate::provider::ToolCall;
use crate::task::SessionToolState;
use crate::text::{FileEncoding, LineEnding};

const MAX_READ_LINES: usize = 2000;
const MAX_READ_CHARS: usize = 100_000;
/// Read per-line character cap; longer lines are truncated
const MAX_LINE_CHARS: usize = 2000;
const MAX_MATCH_RESULTS: usize = 200;
/// Bash foreground output character cap; beyond it, a head/tail preview is returned and the full output goes to a spill file
const MAX_BASH_OUTPUT: usize = 30 * 1024;
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
/// (Anthropic blocks / OpenAI split into user messages); never enters the protocol and is never persisted to the rollout
pub struct ToolImage {
    pub media_type: String,
    pub data_base64: String,
    pub width: u32,
    pub height: u32,
}

pub struct ToolEffect {
    pub output: String,
    pub file_change: Option<FileChange>,
    /// The diff of this edit itself ("before edit → after edit"), rendered inline by the UI tool card;
    /// `file_change` is the session-cumulative view (for the review panel) — the two differ in granularity
    pub edit_diff: Option<FileChange>,
    /// Image output (ReadMediaFile); always empty for other tools
    pub images: Vec<ToolImage>,
}

impl ToolEffect {
    fn plain(output: String) -> Self {
        Self {
            output,
            file_change: None,
            edit_diff: None,
            images: vec![],
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

/// Session-level change tracking: snapshots the original bytes before the first modification; the diff is always "original → current" (LF view).
/// Snapshots are persisted on the session side via the dirty flag (file_originals table); restore rebuilds the baseline after restart.
/// Snapshots store raw bytes (not String), so GBK/UTF-16/binary files can all be reverted at the byte level;
/// the persistence boundary is still String, converted via snapshot_to_store/from_store (non-UTF-8 goes through hex).
///
/// There is also a second, **per-turn** tracking layer (same shape as ZCode turn-file-changes): before the first write within a turn
mod bash;
mod bash_policy;
mod edit;
mod fetch;
mod media;
mod misc;
mod paths;
mod read;
mod search;
mod skill;
mod tracker;
mod websearch;
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
pub(crate) use websearch::*;

pub use bash::DangerReason;
pub use bash::is_dangerous_command;
pub use bash_policy::is_readonly_command;
pub use edit::{EditMatchError, EditOutcome, compute_edit};
pub use fetch::{check_fetch_url, extract_text, is_private_host, is_private_ip};
pub use media::{
    base64_encode, compress_image_for_model, convert_tiff_to_png, decode_image_check,
    encode_image_for_model, image_dimensions, sniff_image,
};
pub use misc::{AgentSwarmTool, AgentTool, parse_questions, parse_swarm_args};
pub use paths::{is_sensitive_file, resolve_checked, resolve_with_access};
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
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    /// Read-only tools are exempt from approval in every mode
    fn read_only(&self) -> bool {
        false
    }
    fn is_shell(&self) -> bool {
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
        Box::new(Bash),
        Box::new(TodoListTool),
        Box::new(FetchUrl),
        Box::new(WebSearch),
        Box::new(TaskList),
        Box::new(TaskOutput),
        Box::new(TaskStop),
        Box::new(AskUserQuestionTool),
        Box::new(ExitPlanModeTool),
        Box::new(EnterPlanModeTool),
        Box::new(ReadMediaFile),
    ]
}

pub fn schemas() -> Vec<serde_json::Value> {
    all().iter().map(|tool| tool.schema()).collect()
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
    data_dir: &Path,
    profiles: &[crate::agent::AgentProfile],
) -> Vec<Box<dyn Tool>> {
    let mut tools = all();
    tools.push(Box::new(AgentTool::new(profiles)));
    tools.push(Box::new(AgentSwarmTool::new(profiles)));
    tools.push(Box::new(SkillTool::new(cwd, data_dir)));
    tools
}

/// Root session tool schema set (including Agent); parameters match all_root
pub fn schemas_root(
    cwd: &Path,
    data_dir: &Path,
    profiles: &[crate::agent::AgentProfile],
) -> Vec<serde_json::Value> {
    all_root(cwd, data_dir, profiles)
        .iter()
        .map(|tool| tool.schema())
        .collect()
}

/// Approval decision: plan mode hard-rejects whole categories earlier (orthogonal to modes); only the permission tier matters here.
/// Neither Yolo nor FullAccess requires approval (the dangerous-command forced dialog lives in the session layer, which Yolo skips as well).
pub fn requires_approval(tool: &dyn Tool, mode: ExecMode) -> bool {
    // Agent/AgentSwarm are always approval-free: each write operation inside the subagent goes through its own approval gate,
    // so the delegated call itself is not approved twice (dialog copy could not describe the whole subtask anyway)
    if tool.name() == "Agent" || tool.name() == "AgentSwarm" {
        return false;
    }
    match mode {
        ExecMode::FullAccess | ExecMode::Yolo => false,
        ExecMode::ConfirmBeforeEdit => !tool.read_only(),
        // MCP tools without annotations are treated conservatively as non-read-only: approval is shown even under AutoEdit
        // (is_shell only covers Bash and cannot intercept MCP write tools passing through)
        ExecMode::AutoEdit => {
            tool.is_shell() || (tool.name().starts_with("mcp__") && !tool.read_only())
        }
    }
}

pub fn summarize(call: &ToolCall) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let raw = match call.name.as_str() {
        "Read" | "Write" | "Edit" | "ReadMediaFile" => {
            args["path"].as_str().unwrap_or("?").to_string()
        }
        "Bash" => args["command"].as_str().unwrap_or("?").to_string(),
        "Glob" => args["pattern"].as_str().unwrap_or("?").to_string(),
        "Grep" => args["pattern"].as_str().unwrap_or("?").to_string(),
        "TodoList" => args["todos"]
            .as_array()
            .map(|items| format!("update todos ({} items)", items.len()))
            .unwrap_or_else(|| "view todos".to_string()),
        "FetchURL" => args["url"].as_str().unwrap_or("?").to_string(),
        "WebSearch" => args["query"].as_str().unwrap_or("?").to_string(),
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
    Vec<ToolImage>,
) {
    execute_with_extra(call, ctx, &[]).await
}

pub async fn execute_with_extra(
    call: &ToolCall,
    ctx: ToolContext<'_>,
    // Runtime tools beyond the built-ins (MCP): fallback list for lookup by name
    extra: &[Box<dyn Tool>],
) -> (
    String,
    bool,
    Option<FileChange>,
    Option<pig_protocol::EditDiff>,
    Vec<ToolImage>,
) {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let tools = all();
    let Some(tool) = tools
        .iter()
        .chain(extra.iter())
        .find(|tool| tool.name() == call.name)
    else {
        return (
            format!("Unknown tool: {}", call.name),
            true,
            None,
            None,
            vec![],
        );
    };
    match tool.execute(args, ctx).await {
        Ok(effect) => (
            effect.output,
            false,
            effect.file_change,
            effect.edit_diff.map(Into::into),
            effect.images,
        ),
        Err(error) => (error, true, None, None, vec![]),
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
struct Bash;

pub fn approval_subject(call: &ToolCall) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    match call.name.as_str() {
        "Bash" => args["command"]
            .as_str()
            .and_then(|command| command.split_whitespace().next())
            .unwrap_or("")
            .to_string(),
        "Write" | "Edit" => args["path"].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    }
}
