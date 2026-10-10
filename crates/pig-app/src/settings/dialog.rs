use super::*;

pub struct ModelDialog {
    /// None = adding a new model
    pub(crate) editing: Option<usize>,
    pub(crate) id: Entity<InputState>,
    pub(crate) context_window: Entity<InputState>,
    pub(crate) max_tokens: Entity<InputState>,
    pub(crate) advanced_open: bool,
    pub(crate) input_image: bool,
    pub(crate) input_video: bool,
    pub(crate) input_pdf: bool,
    pub(crate) cap_structured: bool,
    pub(crate) cap_web_search: bool,
    pub(crate) cap_system_msg: bool,
    pub(crate) enabled: bool,
    pub(crate) reasoning_levels: Vec<String>,
    /// Level id → display name (display only; kept in sync as chips are
    /// added/removed)
    pub(crate) reasoning_labels: std::collections::HashMap<String, String>,
    /// Default reasoning level: the initial level for new sessions and model
    /// switches; None = unset
    pub(crate) default_level: Option<String>,
    pub(crate) new_level: Entity<InputState>,
    pub(crate) new_label: Entity<InputState>,
    pub(crate) params_json: Entity<TextareaState>,
    pub(crate) params_error: Option<String>,
    /// web_search_tool as raw JSON (empty = not set); validated on save
    pub(crate) web_search_json: Entity<TextareaState>,
    pub(crate) web_search_error: Option<String>,
    pub(crate) snapshot: Option<ModelConfig>,
    /// Model ID already looked up on models.dev (the same ID is not re-queried;
    /// only a changed ID triggers a new query)
    pub(crate) looked_up_id: Option<String>,
    /// models.dev lookup state (loading / not-found hint)
    pub(crate) lookup_state: LookupState,
    /// Whether this lookup fills with "reset form" semantics: full overwrite plus
    /// missing fields falling back to defaults; lookups triggered by Enter/blur
    /// use gentle filling (overwrite only what the data source has, keep the
    /// hand-tuned params JSON)
    pub(crate) lookup_overwrite: bool,
}

/// models.dev lookup progress: three states of the hint row beside the input
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LookupState {
    #[default]
    Idle,
    Pending,
    /// Lookup finished but not listed (network failures land here too; Enter retries)
    NotFound,
}

/// MCP create/edit dialog (dual form/JSON modes, aligned with ZCode's editor shape)
pub(crate) struct McpDialog {
    /// Name of the server being edited (None = creating; name and scope are
    /// locked while editing)
    pub(crate) editing: Option<String>,
    /// Target file level to write to
    pub(crate) scope: McpSource,
    /// Whether project level is selectable (the project path is unknown without
    /// an open session, so unavailable)
    pub(crate) project_available: bool,
    /// Transport kind (stdio local command / HTTP remote endpoint)
    pub(crate) kind: McpTransportKind,
    pub(crate) name: Entity<InputState>,
    pub(crate) command: Entity<InputState>,
    /// stdio arguments (space-separated; for arguments containing spaces use
    /// JSON mode)
    pub(crate) args: Entity<InputState>,
    pub(crate) url: Entity<InputState>,
    /// Timeout in milliseconds (blank = default 30s)
    pub(crate) timeout: Entity<InputState>,
    /// Workspace display name for a project-level target (the scope button shows
    /// "project (xxx)"; None = unknown
    pub(crate) project_workspace: Option<String>,
    pub(crate) advanced_open: bool,
    /// JSON textarea for environment variables (stdio) / headers (HTTP)
    pub(crate) env_headers: Entity<TextareaState>,
    /// One draft each for env/headers: the textarea content is swapped when
    /// switching transport kind
    pub(crate) env_draft: String,
    pub(crate) headers_draft: String,
    pub(crate) json_mode: bool,
    pub(crate) json_text: Entity<TextareaState>,
    /// Two-step delete confirmation
    pub(crate) delete_armed: bool,
    /// Edit base: the original entry JSON; saving overlays form fields on it
    /// (unknown fields like oauth are written back faithfully)
    pub(crate) base: serde_json::Value,
    /// Validation error (filled on save/mode-switch failure, shown above the
    /// footer)
    pub(crate) error: Option<String>,
}

/// Skill create/edit dialog (the form edits SKILL.md's frontmatter plus body)
pub(crate) struct SkillDialog {
    /// Directory name of the skill being edited (None = creating; name and scope
    /// are locked while editing)
    pub(crate) editing: Option<String>,
    /// Target directory of the edit (saving rewrites in place; None when creating)
    pub(crate) target_dir: Option<PathBuf>,
    /// Target level to write to when creating
    pub(crate) scope: pig_utils::skills::SkillSource,
    /// Whether project level is selectable (the project path is unknown without
    /// a selected workspace, so unavailable)
    pub(crate) project_available: bool,
    /// Workspace display name for a project-level target (the scope button shows
    /// "project (xxx)"; None = unknown)
    pub(crate) project_workspace: Option<String>,
    pub(crate) name: Entity<InputState>,
    pub(crate) description: Entity<TextareaState>,
    /// frontmatter when_to_use (optional)
    pub(crate) when_to_use: Entity<InputState>,
    /// SKILL.md body
    pub(crate) body: Entity<TextareaState>,
    /// Extra frontmatter keys written back faithfully (license etc., from the
    /// edit base)
    pub(crate) extra_frontmatter: Vec<(String, String)>,
    /// Two-step delete confirmation
    pub(crate) delete_armed: bool,
    /// Validation error (filled on save failure, shown above the footer)
    pub(crate) error: Option<String>,
}
