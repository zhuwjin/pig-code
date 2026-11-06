//! Lifecycle hooks (Claude Code-compatible contract, scoped per docs/PLAN.md):
//! user/project-configured shell commands run at engine events with a
//! one-line JSON payload on stdin; they observe, block, or adjust agent
//! behavior via exit code 2 or a stdout JSON object. Sequential execution in
//! config order (user entries before project entries per event); a failed or
//! timed-out hook never blocks the turn — only an explicit decision does.

mod output;
mod payload;
mod runner;
mod transcript;

use std::path::{Path, PathBuf};

use pig_protocol::{
    AppConfig, Event, HookEntry, HookKind, HookRunState, HookShell, HookSource, HooksConfig,
};
use tokio_util::sync::CancellationToken;

pub(crate) use output::{HookEffects, HookPermission};
pub use payload::HookPayload;
pub(crate) use runner::run_entry;

/// Hard cap for a single hook's timeout, aligned with the Bash tool's max
/// (10 minutes)
const MAX_HOOK_TIMEOUT_MS: u64 = 600_000;

/// The closed set of hook events (order fixes the HookSet slot index)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
    Stop,
}

pub const HOOK_EVENTS: [HookEvent; 7] = [
    HookEvent::SessionStart,
    HookEvent::UserPromptSubmit,
    HookEvent::PreToolUse,
    HookEvent::PermissionRequest,
    HookEvent::PostToolUse,
    HookEvent::PostToolUseFailure,
    HookEvent::Stop,
];

impl HookEvent {
    /// The contract event a package contributes to (the two closed sets are
    /// the same seven names).
    pub fn from_plugin(event: pig_protocol::PluginHookEvent) -> Self {
        match event {
            pig_protocol::PluginHookEvent::SessionStart => Self::SessionStart,
            pig_protocol::PluginHookEvent::UserPromptSubmit => Self::UserPromptSubmit,
            pig_protocol::PluginHookEvent::PreToolUse => Self::PreToolUse,
            pig_protocol::PluginHookEvent::PermissionRequest => Self::PermissionRequest,
            pig_protocol::PluginHookEvent::PostToolUse => Self::PostToolUse,
            pig_protocol::PluginHookEvent::PostToolUseFailure => Self::PostToolUseFailure,
            pig_protocol::PluginHookEvent::Stop => Self::Stop,
        }
    }

    /// Claude-form event name (payload's hook_event_name + UI display)
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::Stop => "Stop",
        }
    }

    /// Snake-case config key (the TOML array name under `[hooks]`)
    pub fn config_key(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session_start",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PermissionRequest => "permission_request",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::PostToolUseFailure => "post_tool_use_failure",
            HookEvent::Stop => "stop",
        }
    }

    /// Inverse of config_key (settings page round-trips)
    pub fn from_config_key(key: &str) -> Option<HookEvent> {
        HOOK_EVENTS
            .iter()
            .copied()
            .find(|event| event.config_key() == key)
    }

    /// HookSet slot index (position in HOOK_EVENTS)
    fn index(self) -> usize {
        self as usize
    }
}

/// Per-session merged hook snapshot: user-level entries from the AppConfig
/// snapshot plus project-level entries from `.pigcode/hooks.toml`, taken once
/// at Session create/load (same lifecycle as PermissionRules). Project entries
/// run only while the workspace is trusted (Op::TrustProjectHooks flips the
/// flag mid-session; new sessions pick the persisted list up at load).
#[derive(Clone)]
pub struct HookSet {
    /// Master switch from the user config: off = nothing runs (project files
    /// cannot force-enable)
    enabled: bool,
    timeout_ms: u64,
    max_output_bytes: usize,
    /// Per-event entries in execution order (user entries first, project
    /// entries after), indexed by HookEvent::index
    entries: Vec<Vec<HookEntry>>,
    /// Workspace root whose .pigcode/hooks.toml was loaded (trust key + banner)
    pub project_root: Option<PathBuf>,
    /// Total project-source entries across events (banner count)
    pub project_count: usize,
    /// Whether project entries may run (trusted via the first-use dialog)
    project_trusted: bool,
    /// The registry plugin entries were resolved from. Every run re-checks the
    /// declaring package, so disabling, updating or uninstalling it revokes
    /// the entry without rebuilding the session's hook set.
    plugins: Option<crate::plugins::SharedPluginRegistry>,
}

impl HookSet {
    pub fn load(cwd: &Path, app_config: Option<&AppConfig>) -> Self {
        let user = app_config.map(|c| c.hooks.clone()).unwrap_or_default();
        let mut set = Self {
            enabled: user.enabled,
            timeout_ms: user.timeout_ms.clamp(1, MAX_HOOK_TIMEOUT_MS),
            max_output_bytes: user.max_output_bytes.max(1024),
            entries: HOOK_EVENTS
                .iter()
                .map(|event| user_entries(&user, *event))
                .collect(),
            project_root: None,
            project_count: 0,
            project_trusted: false,
            plugins: None,
        };
        for entries in &mut set.entries {
            for entry in entries.iter_mut() {
                entry.source = Some(HookSource::User);
            }
        }
        if !set.enabled {
            // Master switch off: the project file is not even read (nothing
            // can run, so there is nothing to banner about either)
            return set;
        }
        let project_path = cwd.join(".pigcode").join("hooks.toml");
        let Ok(text) = std::fs::read_to_string(&project_path) else {
            return set;
        };
        // Same `[hooks]` root shape as the user config; only the event arrays
        // are honored. Parse failure = warn + skip (fail-closed for execution)
        match toml::from_str::<ProjectHooksFile>(&text) {
            Ok(file) => {
                let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
                set.project_trusted = user
                    .trusted_projects
                    .iter()
                    .any(|trusted| Path::new(trusted) == canonical);
                set.project_root = Some(canonical);
                for event in HOOK_EVENTS {
                    for mut entry in file.hooks.entries(event).iter().cloned() {
                        if !valid_hook_entry(&entry) {
                            continue;
                        }
                        entry.source = Some(HookSource::Project);
                        set.project_count += 1;
                        set.entries[event.index()].push(entry);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    path = %project_path.display(),
                    detail = %error,
                    "project hooks file failed to parse; skipping it"
                );
            }
        }
        set
    }

    /// [`HookSet::load`] plus the enabled packages' hook contributions. They
    /// run through the same runner as user and project entries; the manifest
    /// must declare `provide_hooks`, and a project package is admitted only in
    /// a trusted workspace (the same rule as its other contributions).
    pub fn load_with_plugins(
        cwd: &Path,
        app_config: Option<&AppConfig>,
        plugins: &crate::plugins::SharedPluginRegistry,
        trust: &crate::config::WorkspaceTrust,
    ) -> Self {
        let mut set = Self::load(cwd, app_config);
        set.plugins = Some(plugins.clone());
        // The master switch gates package hooks too: nothing can force them on.
        if !set.enabled {
            return set;
        }
        let packages = plugins
            .lock()
            .map(|registry| crate::plugins::enabled_packages(&registry, trust))
            .unwrap_or_else(|error| {
                tracing::warn!(detail = %error, "plugin registry lock failed; plugin hooks are off");
                Ok(Vec::new())
            });
        let packages = match packages {
            Ok(packages) => packages,
            Err(error) => {
                tracing::warn!(detail = ?error, "plugin hooks could not be resolved");
                return set;
            }
        };
        for package in packages {
            for hook in package.contributions.hooks {
                if !valid_hook_entry(&hook.entry) {
                    continue;
                }
                let mut entry = hook.entry;
                entry.source = Some(HookSource::Plugin {
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                });
                set.entries[HookEvent::from_plugin(hook.event).index()].push(entry);
            }
        }
        set
    }

    /// Fast path for the seven integration sites: false = zero hook work
    pub fn is_active(&self) -> bool {
        self.enabled && self.entries.iter().any(|entries| !entries.is_empty())
    }

    /// Entries of one event that may actually run right now: entry-level
    /// enabled flag, project trust admission, and — for a package entry — the
    /// declaring package still being enabled, current and declaring this hook.
    fn runnable(&self, event: HookEvent) -> Vec<&HookEntry> {
        self.entries[event.index()]
            .iter()
            .filter(|entry| {
                entry.enabled
                    && match &entry.source {
                        Some(HookSource::Project) => self.project_trusted,
                        Some(HookSource::Plugin { package, digest }) => {
                            self.plugin_entry_active(package, digest, entry)
                        }
                        _ => true,
                    }
            })
            .collect()
    }

    fn plugin_entry_active(&self, package: &str, digest: &str, entry: &HookEntry) -> bool {
        let Some(plugins) = &self.plugins else {
            return true;
        };
        let Ok(registry) = plugins.lock() else {
            return false;
        };
        let mut declared = entry.clone();
        declared.source = None;
        registry.plugin_hook_active(package, digest, &declared)
    }

    /// Untrusted project hooks exist (and would run once trusted): the runner
    /// emits ProjectHooksPending once per session for this
    pub fn pending_project(&self) -> Option<(String, usize)> {
        if !self.enabled || self.project_trusted || self.project_count == 0 {
            return None;
        }
        self.project_root
            .as_ref()
            .map(|root| (root.display().to_string(), self.project_count))
    }

    /// Activate project hooks for this session (Op::TrustProjectHooks); the
    /// persisted trusted_projects list is the runner's business
    pub fn trust_project(&mut self) {
        self.project_trusted = true;
    }

    /// Global timeout/max-output knobs for the runner (entry overrides apply
    /// on top)
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }
}

/// `.pigcode/hooks.toml` wrapper: identical `[hooks]` root as the user config,
/// but only the seven event arrays are honored/round-tripped — global knobs in
/// the project file are ignored by design and never written back
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ProjectHooksFile {
    #[serde(default)]
    hooks: ProjectHooksBody,
}

/// The seven event arrays of the project hooks file (empty arrays are skipped
/// on write so the file stays hand-editable)
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ProjectHooksBody {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    session_start: Vec<HookEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    user_prompt_submit: Vec<HookEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pre_tool_use: Vec<HookEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    permission_request: Vec<HookEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    post_tool_use: Vec<HookEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    post_tool_use_failure: Vec<HookEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stop: Vec<HookEntry>,
}

impl ProjectHooksBody {
    fn entries(&self, event: HookEvent) -> &Vec<HookEntry> {
        match event {
            HookEvent::SessionStart => &self.session_start,
            HookEvent::UserPromptSubmit => &self.user_prompt_submit,
            HookEvent::PreToolUse => &self.pre_tool_use,
            HookEvent::PermissionRequest => &self.permission_request,
            HookEvent::PostToolUse => &self.post_tool_use,
            HookEvent::PostToolUseFailure => &self.post_tool_use_failure,
            HookEvent::Stop => &self.stop,
        }
    }

    fn entries_mut(&mut self, event: HookEvent) -> &mut Vec<HookEntry> {
        match event {
            HookEvent::SessionStart => &mut self.session_start,
            HookEvent::UserPromptSubmit => &mut self.user_prompt_submit,
            HookEvent::PreToolUse => &mut self.pre_tool_use,
            HookEvent::PermissionRequest => &mut self.permission_request,
            HookEvent::PostToolUse => &mut self.post_tool_use,
            HookEvent::PostToolUseFailure => &mut self.post_tool_use_failure,
            HookEvent::Stop => &mut self.stop,
        }
    }
}

/// Settings-page helper: read one workspace's project hook declarations from
/// `.pigcode/hooks.toml`. Ok(vec) = parsed (possibly empty); Err = the file
/// exists but failed to parse (displayed as a parse error on the page)
pub fn read_project_entries(cwd: &Path) -> Result<Vec<(HookEvent, Vec<HookEntry>)>, String> {
    let text = std::fs::read_to_string(cwd.join(".pigcode").join("hooks.toml"))
        .map_err(|e| e.to_string())?;
    let file: ProjectHooksFile = toml::from_str(&text).map_err(|e| e.to_string())?;
    Ok(HOOK_EVENTS
        .iter()
        .map(|event| (*event, file.hooks.entries(*event).clone()))
        .collect())
}

/// Settings-page helper: write one workspace's project hooks back to
/// `.pigcode/hooks.toml` as a full-file rewrite (only the seven event arrays;
/// empty arrays are omitted). Refuses to clobber an existing unparsable file
/// (the user must fix it by hand first); creates `.pigcode/` when missing.
/// Runtime annotations (source) are stripped before writing.
pub fn write_project_entries(
    cwd: &Path,
    entries: &[(HookEvent, Vec<HookEntry>)],
) -> Result<(), String> {
    let path = cwd.join(".pigcode").join("hooks.toml");
    if path.is_file() {
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        toml::from_str::<ProjectHooksFile>(&text).map_err(|e| e.to_string())?;
    }
    let mut body = ProjectHooksBody::default();
    for (event, list) in entries {
        *body.entries_mut(*event) = list
            .iter()
            .cloned()
            .map(|mut entry| {
                entry.source = None;
                entry
            })
            .collect();
    }
    let text =
        toml::to_string_pretty(&ProjectHooksFile { hooks: body }).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

fn user_entries(config: &HooksConfig, event: HookEvent) -> Vec<HookEntry> {
    let mut entries = match event {
        HookEvent::SessionStart => config.session_start.clone(),
        HookEvent::UserPromptSubmit => config.user_prompt_submit.clone(),
        HookEvent::PreToolUse => config.pre_tool_use.clone(),
        HookEvent::PermissionRequest => config.permission_request.clone(),
        HookEvent::PostToolUse => config.post_tool_use.clone(),
        HookEvent::PostToolUseFailure => config.post_tool_use_failure.clone(),
        HookEvent::Stop => config.stop.clone(),
    };
    entries.retain(valid_hook_entry);
    entries
}

/// A process hook has no shell, so a `shell` value there is a configuration
/// error rather than a silently ignored field.
pub(crate) fn valid_hook_entry(entry: &HookEntry) -> bool {
    if entry.kind == HookKind::Process && entry.shell != HookShell::Auto {
        tracing::warn!(
            command = %entry.command,
            shell = entry.shell.name(),
            "hook entry selects a shell for a process hook; skipping the entry"
        );
        return false;
    }
    true
}

/// Matcher semantics (Claude/ZCode): missing/empty/"*" matches everything;
/// events without a match subject (no tool name) always match; a matcher of
/// plain names `^[A-Za-z0-9_|]+$` is an exact pipe-separated list; anything
/// else is a regex, and an invalid regex never matches.
pub(crate) fn matcher_matches(matcher: Option<&str>, value: Option<&str>) -> bool {
    let Some(matcher) = matcher
        .map(str::trim)
        .filter(|m| !m.is_empty() && *m != "*")
    else {
        return true;
    };
    let Some(value) = value else {
        return true;
    };
    let plain = matcher
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|');
    if plain {
        return matcher.split('|').any(|name| name == value);
    }
    regex::Regex::new(matcher)
        .map(|compiled| compiled.is_match(value))
        .unwrap_or(false)
}

/// Owned context for one hook-chain run: cloned cheaply into detached async
/// hook tasks (the channel sender is shared with the session)
pub(crate) struct HookRunCtx {
    pub session_id: String,
    pub cwd: PathBuf,
    pub tx: async_channel::Sender<Event>,
    pub cancel: CancellationToken,
}

/// Run every matching hook of one event in config order and merge the
/// effects. Empty match set = zero work (no events, no subprocess). Emits a
/// live-only HookStatus pair (Running/terminal) around the synchronous part;
/// async entries are detached and only logged.
pub(crate) async fn run_event(
    hooks: &HookSet,
    event: HookEvent,
    payload: HookPayload,
    run: &HookRunCtx,
) -> HookEffects {
    // Match subject: tool events match the tool name, SessionStart matches its
    // source ("startup"/"resume"), the rest have no subject and always match
    let subject = payload.tool_name.as_deref().or(payload.source);
    let matched: Vec<&HookEntry> = hooks
        .runnable(event)
        .into_iter()
        .filter(|entry| matcher_matches(entry.matcher.as_deref(), subject))
        .collect();
    if matched.is_empty() {
        return HookEffects::default();
    }
    let sync_matched: Vec<&HookEntry> = matched.iter().copied().filter(|e| !e.r#async).collect();
    // The indicator label comes from the first synchronous entry (an async
    // entry's label would mislabel the sync chain it is not part of)
    let label = sync_matched
        .first()
        .or(matched.first())
        .and_then(|entry| entry.status_message.clone())
        .unwrap_or_else(|| event.name().to_string());
    if !sync_matched.is_empty() {
        crate::session::emit_bg(&run.session_id, &run.tx, |session_id| Event::HookStatus {
            session_id,
            hook_event: event.name().to_string(),
            label: label.clone(),
            state: HookRunState::Running,
        });
    }
    let mut merged = HookEffects::default();
    let mut state = HookRunState::Finished;
    let mut cancelled = false;
    for entry in matched {
        if entry.r#async {
            // Fire-and-forget: output and decisions are ignored (terminal
            // state only logged + reported as a summary row); the cancel
            // token still reaps the process
            let ctx = HookRunCtx {
                session_id: run.session_id.clone(),
                cwd: run.cwd.clone(),
                tx: run.tx.clone(),
                cancel: run.cancel.clone(),
            };
            let entry = entry.clone();
            let payload = payload.clone();
            let (timeout_ms, max_output) = (hooks.timeout_ms(), hooks.max_output_bytes());
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let raw = run_entry(&entry, &payload, &ctx, timeout_ms, max_output).await;
                let row_state = match raw.outcome {
                    output::RawOutcome::Exited(Some(0)) => Some(HookRunState::Finished),
                    // Exit 2 asked to block but async decisions are ignored
                    output::RawOutcome::Exited(Some(2)) => Some(HookRunState::Blocked),
                    output::RawOutcome::Cancelled => None,
                    _ => Some(HookRunState::Failed),
                };
                if let Some(row_state) = row_state {
                    let hook_event = event.name().to_string();
                    let label = entry
                        .status_message
                        .clone()
                        .unwrap_or_else(|| event.name().to_string());
                    let source = entry.source.clone();
                    let duration_ms = started.elapsed().as_millis() as u64;
                    crate::session::emit_bg(&ctx.session_id, &ctx.tx, |session_id| {
                        Event::HookRunFinished {
                            session_id,
                            hook_event,
                            label,
                            source,
                            duration_ms,
                            state: row_state,
                            reason: None,
                        }
                    });
                }
                tracing::info!(
                    hook_event = event.name(),
                    outcome = ?raw.outcome_tag(),
                    "async hook finished"
                );
            });
            continue;
        }
        if run.cancel.is_cancelled() {
            cancelled = true;
            break;
        }
        let started = std::time::Instant::now();
        let raw = run_entry(
            entry,
            &payload,
            run,
            hooks.timeout_ms(),
            hooks.max_output_bytes(),
        )
        .await;
        let (effects, outcome) = output::interpret(event, &raw);
        tracing::info!(
            hook_event = event.name(),
            command = %entry.command,
            outcome = ?outcome,
            elapsed_ms = started.elapsed().as_millis() as u64,
            stdout_preview = %raw.stdout_preview(),
            stderr_preview = %raw.stderr_preview(),
            "hook finished"
        );
        // Per-entry completion row for the thread-view hook summary
        let row_state = match outcome {
            output::Outcome::Finished => Some(HookRunState::Finished),
            output::Outcome::Blocked => Some(HookRunState::Blocked),
            output::Outcome::Failed => Some(HookRunState::Failed),
            output::Outcome::Cancelled => None,
        };
        if let Some(row_state) = row_state {
            let hook_event = event.name().to_string();
            let label = entry
                .status_message
                .clone()
                .unwrap_or_else(|| event.name().to_string());
            let source = entry.source.clone();
            let duration_ms = started.elapsed().as_millis() as u64;
            let reason = effects.blocked.clone();
            crate::session::emit_bg(&run.session_id, &run.tx, |session_id| {
                Event::HookRunFinished {
                    session_id,
                    hook_event,
                    label,
                    source,
                    duration_ms,
                    state: row_state,
                    reason,
                }
            });
        }
        state = match (state, outcome) {
            (_, output::Outcome::Blocked) => HookRunState::Blocked,
            (HookRunState::Finished, output::Outcome::Failed) => HookRunState::Failed,
            (earlier, output::Outcome::Finished | output::Outcome::Failed) => earlier,
            (_, output::Outcome::Cancelled) => {
                cancelled = true;
                break;
            }
        };
        merged.merge(effects);
    }
    // On cancellation no terminal event: a Finished here would misreport, and
    // the Running indicator is superseded by the turn's following events
    if !sync_matched.is_empty() && !cancelled {
        crate::session::emit_bg(&run.session_id, &run.tx, |session_id| Event::HookStatus {
            session_id,
            hook_event: event.name().to_string(),
            label: label.clone(),
            state,
        });
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(command: &str) -> HookEntry {
        HookEntry {
            kind: pig_protocol::HookKind::Command,
            command: command.to_string(),
            args: Vec::new(),
            shell: pig_protocol::HookShell::Auto,
            matcher: None,
            timeout_ms: None,
            r#async: false,
            enabled: true,
            status_message: None,
            source: None,
            custom: None,
        }
    }

    #[test]
    fn a_process_hook_cannot_select_a_shell() {
        let mut process = entry("node");
        process.kind = HookKind::Process;
        process.shell = HookShell::GitBash;
        assert!(!valid_hook_entry(&process));

        let mut command = entry("echo hi");
        command.shell = HookShell::Bash;
        assert!(valid_hook_entry(&command));
    }

    /// Runner smoke test: stdin payload write + stdout capture + exit code
    /// round-trip (unix sh; the engine path is exercised by tests/hooks.rs)
    #[cfg(unix)]
    #[tokio::test]
    async fn runner_executes_script_and_captures_output() {
        let entry = entry("cat > /dev/null; echo '{\"continue\": false, \"reason\": \"nope\"}'");
        let (tx, _rx) = async_channel::unbounded::<Event>();
        let run = HookRunCtx {
            session_id: "s-test".into(),
            cwd: std::env::temp_dir(),
            tx,
            cancel: CancellationToken::new(),
        };
        let payload = HookPayload::user_prompt(
            "s-test",
            &run.cwd,
            pig_protocol::ExecMode::Auto.permissions(),
            "hi",
        );
        let raw = run_entry(&entry, &payload, &run, 5000, 32768).await;
        // Exit 0 + a block JSON: the raw process tag is "success"; the block
        // decision itself is interpret()'s business (output.rs tests)
        assert_eq!(raw.outcome_tag(), "success");
        assert!(raw.stdout.contains("nope"), "stdout: {}", raw.stdout);
    }

    /// An explicit shell that this platform cannot provide fails the hook
    /// instead of falling back to the auto chain.
    #[tokio::test]
    async fn an_unavailable_explicit_shell_is_a_spawn_failure() {
        use crate::hooks::output::RawOutcome;

        let (tx, _rx) = async_channel::unbounded::<Event>();
        let run = HookRunCtx {
            session_id: "s-test".into(),
            cwd: std::env::temp_dir(),
            tx,
            cancel: CancellationToken::new(),
        };
        let payload = HookPayload::user_prompt(
            "s-test",
            &run.cwd,
            pig_protocol::ExecMode::Auto.permissions(),
            "hi",
        );
        let mut entry = entry("echo hi");
        #[cfg(windows)]
        {
            entry.shell = HookShell::Zsh;
        }
        #[cfg(unix)]
        {
            entry.shell = HookShell::Cmd;
        }
        let raw = run_entry(&entry, &payload, &run, 5000, 32768).await;
        assert_eq!(raw.outcome_tag(), "spawn_failed");
        let RawOutcome::SpawnFailed(message) = raw.outcome else {
            panic!("expected a spawn failure");
        };
        assert!(
            message.contains("not available on this platform"),
            "{message}"
        );
    }

    #[test]
    fn matcher_shapes() {
        // Missing / empty / "*" match everything
        assert!(matcher_matches(None, Some("Write")));
        assert!(matcher_matches(Some(""), Some("Write")));
        assert!(matcher_matches(Some("*"), Some("Write")));
        // No match subject (non-tool events): always matches
        assert!(matcher_matches(Some("Write|Edit"), None));
        // Exact pipe list
        assert!(matcher_matches(Some("Write|Edit"), Some("Write")));
        assert!(matcher_matches(Some("Write|Edit"), Some("Edit")));
        assert!(!matcher_matches(Some("Write|Edit"), Some("ExecCommand")));
        // Regex branch (contains a non-plain character)
        assert!(matcher_matches(Some("^mcp__.*$"), Some("mcp__fs__write")));
        assert!(!matcher_matches(Some("^mcp__.*$"), Some("Write")));
        // Invalid regex never matches
        assert!(!matcher_matches(Some("([invalid"), Some("Write")));
    }

    #[test]
    fn user_config_parses_flat_entries() {
        let config: AppConfig = toml::from_str(
            r#"
providers = []
default_provider = "x"
default_model = "y"

[hooks]
enabled = true
timeout_ms = 15000

[[hooks.pre_tool_use]]
command = "guard.sh"
matcher = "Write|Edit"
timeout_ms = 5000

[[hooks.stop]]
command = "check.sh"
async = true

[[hooks.pre_tool_use]]
command = "disabled.sh"
enabled = false
"#,
        )
        .unwrap();
        assert!(config.hooks.enabled);
        assert_eq!(config.hooks.timeout_ms, 15000);
        assert_eq!(config.hooks.pre_tool_use.len(), 2);
        assert_eq!(
            config.hooks.pre_tool_use[0].matcher.as_deref(),
            Some("Write|Edit")
        );
        assert_eq!(config.hooks.pre_tool_use[0].timeout_ms, Some(5000));
        assert!(config.hooks.stop[0].r#async);
        assert!(!config.hooks.pre_tool_use[1].enabled);
        // Defaults when the section is absent entirely
        let bare: AppConfig =
            toml::from_str("providers = []\ndefault_provider = \"x\"\ndefault_model = \"y\"")
                .unwrap();
        assert!(!bare.hooks.enabled);
        assert_eq!(bare.hooks.timeout_ms, 60_000);
        assert_eq!(bare.hooks.max_output_bytes, 32 * 1024);
    }

    #[test]
    fn hook_set_merges_user_first_then_project_with_trust() {
        let dir = std::env::temp_dir().join(format!("pig-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
        std::fs::write(
            dir.join(".pigcode").join("hooks.toml"),
            "[[hooks.pre_tool_use]]\ncommand = \"project.sh\"\n[[hooks.stop]]\ncommand = \"ps.sh\"\n",
        )
        .unwrap();
        let mut config = AppConfig {
            providers: vec![],
            default_provider: "x".into(),
            default_model: "y".into(),
            ..Default::default()
        };
        config.hooks.enabled = true;
        config.hooks.pre_tool_use = vec![entry("user-a"), entry("user-b")];

        // Untrusted: project entries present but pending
        let set = HookSet::load(&dir, Some(&config));
        assert!(set.is_active());
        let pending = set.pending_project().expect("project hooks pending");
        assert_eq!(pending.1, 2);
        let runnable: Vec<&HookEntry> = set.runnable(HookEvent::PreToolUse);
        assert_eq!(
            runnable
                .iter()
                .map(|e| e.command.as_str())
                .collect::<Vec<_>>(),
            ["user-a", "user-b"],
            "untrusted project entries stay out of the runnable set"
        );

        // Trusted mid-session: project entries join after user entries
        let mut set = set;
        set.trust_project();
        assert!(set.pending_project().is_none());
        let runnable: Vec<&HookEntry> = set.runnable(HookEvent::PreToolUse);
        assert_eq!(
            runnable
                .iter()
                .map(|e| e.command.as_str())
                .collect::<Vec<_>>(),
            ["user-a", "user-b", "project.sh"]
        );
        assert_eq!(runnable[2].source, Some(HookSource::Project));
        assert_eq!(
            set.runnable(HookEvent::Stop)
                .iter()
                .map(|e| e.command.as_str())
                .collect::<Vec<_>>(),
            ["ps.sh"]
        );

        // Persisted trust list admits the project file at load time
        config.hooks.trusted_projects = vec![dir.canonicalize().unwrap().display().to_string()];
        let set = HookSet::load(&dir, Some(&config));
        assert!(set.pending_project().is_none());
        assert_eq!(set.runnable(HookEvent::Stop).len(), 1);

        // Master switch off: nothing is active and nothing pends
        config.hooks.enabled = false;
        let set = HookSet::load(&dir, Some(&config));
        assert!(!set.is_active());
        assert!(set.pending_project().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugin_hooks_join_after_user_entries_and_are_revoked_with_the_package() {
        let dir = std::env::temp_dir().join(format!(
            "pig-hooks-plugin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let package = dir.join("plugin");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("pig-plugin.json"),
            r#"{
              "apiVersion": 1,
              "id": "demo",
              "name": "Demo",
              "version": "1.0.0",
              "build": 1,
              "minHostVersion": "1.0.0",
              "protocols": {"ui": 1, "services": 1},
              "capabilities": [],
              "contributions": {
                "actions": ["provide_hooks"],
                "hooks": [{
                  "event": "pre_tool_use",
                  "command": "plugin-guard.sh",
                  "matcher": "Write|Edit"
                }]
              }
            }"#,
        )
        .unwrap();
        let data = dir.join("data");
        let plugins = crate::plugins::PluginRegistry::shared(data);
        let snapshot = plugins
            .lock()
            .unwrap()
            .sync(vec![pig_protocol::PluginPackageSource {
                root: package,
                source: pig_protocol::PluginSource::User,
            }]);
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        let digest = snapshot.packages[0].digest.clone();
        plugins
            .lock()
            .unwrap()
            .set_enabled("demo", &digest, true)
            .unwrap();

        let mut config = AppConfig {
            providers: vec![],
            default_provider: "x".into(),
            default_model: "y".into(),
            ..Default::default()
        };
        config.hooks.enabled = true;
        config.hooks.pre_tool_use = vec![entry("user-a")];
        let trust = crate::config::WorkspaceTrust::untrusted(&dir);
        let set = HookSet::load_with_plugins(&dir, Some(&config), &plugins, &trust);
        let runnable = set.runnable(HookEvent::PreToolUse);
        assert_eq!(
            runnable
                .iter()
                .map(|entry| entry.command.as_str())
                .collect::<Vec<_>>(),
            vec!["user-a", "plugin-guard.sh"],
            "package entries run after user entries"
        );
        assert!(matches!(
            runnable[1].source,
            Some(HookSource::Plugin { .. })
        ));

        // Disabling the package revokes its hook without rebuilding the set.
        plugins
            .lock()
            .unwrap()
            .set_enabled("demo", &digest, false)
            .unwrap();
        assert_eq!(set.runnable(HookEvent::PreToolUse).len(), 1);
        plugins
            .lock()
            .unwrap()
            .set_enabled("demo", &digest, true)
            .unwrap();
        assert_eq!(set.runnable(HookEvent::PreToolUse).len(), 2);

        // The global switch still gates package hooks.
        config.hooks.enabled = false;
        let off = HookSet::load_with_plugins(&dir, Some(&config), &plugins, &trust);
        assert!(!off.is_active());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_project_file_warns_and_skips() {
        let dir = std::env::temp_dir().join(format!("pig-hooks-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
        std::fs::write(
            dir.join(".pigcode").join("hooks.toml"),
            "not valid toml {{{",
        )
        .unwrap();
        let mut config = AppConfig {
            providers: vec![],
            default_provider: "x".into(),
            default_model: "y".into(),
            ..Default::default()
        };
        config.hooks.enabled = true;
        let set = HookSet::load(&dir, Some(&config));
        assert!(!set.is_active());
        assert!(set.pending_project().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_new_fields_round_trip_toml() {
        let entry: HookEntry = toml::from_str(
            r#"
type = "process"
command = "node"
args = ["scripts/check.mjs", "--strict"]
matcher = "Write"

[custom]
review = "strict"
level = 2
"#,
        )
        .unwrap();
        assert_eq!(entry.kind, pig_protocol::HookKind::Process);
        assert_eq!(entry.args, ["scripts/check.mjs", "--strict"]);
        let custom = entry.custom.as_ref().expect("custom map");
        assert_eq!(custom["review"], "strict");
        assert_eq!(custom["level"], 2);
        // The runtime annotation never survives a round-trip
        let mut stamped = entry.clone();
        stamped.source = Some(HookSource::Project);
        let text = toml::to_string_pretty(&stamped).unwrap();
        assert!(
            !text.contains("source"),
            "source must not serialize: {text}"
        );
        let back: HookEntry = toml::from_str(&text).unwrap();
        assert_eq!(back.kind, pig_protocol::HookKind::Process);
        assert_eq!(back.custom.as_ref().unwrap()["review"], "strict");
        // Defaults: bare command-only entries stay shell-kind with no args
        let bare: HookEntry = toml::from_str(r#"command = "echo hi""#).unwrap();
        assert_eq!(bare.kind, pig_protocol::HookKind::Command);
        assert!(bare.args.is_empty());
        assert!(bare.custom.is_none());
    }

    #[test]
    fn write_project_entries_round_trip_and_refuses_broken_file() {
        let dir = std::env::temp_dir().join(format!("pig-hooks-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Creating the file (and .pigcode/) from nothing works; runtime
        // annotations are stripped, empty event arrays are omitted
        let mut stamped = entry("project.sh");
        stamped.source = Some(HookSource::Project);
        write_project_entries(&dir, &[(HookEvent::PreToolUse, vec![stamped])]).unwrap();
        let text = std::fs::read_to_string(dir.join(".pigcode").join("hooks.toml")).unwrap();
        assert!(text.contains("project.sh"), "{text}");
        assert!(!text.contains("source"), "{text}");
        assert!(
            !text.contains("stop"),
            "empty events stay unwritten: {text}"
        );
        let read = read_project_entries(&dir).unwrap();
        let pre = read
            .iter()
            .find(|(event, _)| *event == HookEvent::PreToolUse)
            .map(|(_, list)| list)
            .unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].command, "project.sh");
        // Rewriting with an empty set empties the file's arrays
        write_project_entries(&dir, &[]).unwrap();
        let read = read_project_entries(&dir).unwrap();
        assert!(read.iter().all(|(_, list)| list.is_empty()));
        // A broken file is never clobbered
        std::fs::write(dir.join(".pigcode").join("hooks.toml"), "{{{").unwrap();
        assert!(write_project_entries(&dir, &[]).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(".pigcode").join("hooks.toml")).unwrap(),
            "{{{"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SessionStart matches its matcher against the source, not (vacuously)
    /// against a missing tool name
    #[cfg(unix)]
    #[tokio::test]
    async fn session_start_matcher_filters_on_source() {
        let mut config = AppConfig {
            providers: vec![],
            default_provider: "x".into(),
            default_model: "y".into(),
            ..Default::default()
        };
        config.hooks.enabled = true;
        let mut only_resume = entry("touch $PIG_PROJECT_DIR/resume-ran");
        only_resume.matcher = Some("resume".to_string());
        config.hooks.session_start = vec![only_resume];
        let dir = std::env::temp_dir().join(format!("pig-hooks-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let set = HookSet::load(&dir, Some(&config));
        let (tx, _rx) = async_channel::unbounded::<Event>();
        let run = HookRunCtx {
            session_id: "s-src".into(),
            cwd: dir.clone(),
            tx,
            cancel: CancellationToken::new(),
        };
        // "startup" does not match matcher "resume": nothing runs
        run_event(
            &set,
            HookEvent::SessionStart,
            HookPayload::session_start(
                "s-src",
                &dir,
                pig_protocol::ExecMode::Auto.permissions(),
                "startup",
            ),
            &run,
        )
        .await;
        assert!(!dir.join("resume-ran").exists());
        // "resume" matches: the hook runs
        run_event(
            &set,
            HookEvent::SessionStart,
            HookPayload::session_start(
                "s-src",
                &dir,
                pig_protocol::ExecMode::Auto.permissions(),
                "resume",
            ),
            &run,
        )
        .await;
        assert!(dir.join("resume-ran").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Claude-compatible env aliases ride on the child process, and ${VAR}
    /// expansion applies to command/args before spawn (unknown names stay
    /// literal for the process kind, where no shell re-expands them)
    #[cfg(unix)]
    #[tokio::test]
    async fn runner_env_aliases_and_var_expansion() {
        let dir = std::env::temp_dir().join(format!("pig-hooks-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, _rx) = async_channel::unbounded::<Event>();
        let run = HookRunCtx {
            session_id: "s-env".into(),
            cwd: dir.clone(),
            tx,
            cancel: CancellationToken::new(),
        };
        // Shell kind: the child sees the CLAUDE_* aliases
        let probe = entry("printf '%s|%s' \"$CLAUDE_SESSION_ID\" \"$CLAUDE_PROJECT_DIR\"");
        let payload = HookPayload::user_prompt(
            "s-env",
            &dir,
            pig_protocol::ExecMode::Auto.permissions(),
            "hi",
        );
        let raw = run_entry(&probe, &payload, &run, 5000, 32768).await;
        assert_eq!(raw.stdout, format!("s-env|{}", dir.display()));
        // Process kind: ${VAR} expands pre-spawn (no shell would do it)
        let mut process = entry("echo");
        process.kind = pig_protocol::HookKind::Process;
        process.args = vec![
            "${CLAUDE_SESSION_ID}".to_string(),
            "${PIG_PROJECT_DIR}".to_string(),
            "${DEFINITELY_UNSET_PIG_VAR}".to_string(),
        ];
        let raw = run_entry(&process, &payload, &run, 5000, 32768).await;
        assert_eq!(
            raw.stdout,
            format!("s-env {} ${{DEFINITELY_UNSET_PIG_VAR}}\n", dir.display())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// kind=process spawns argv directly (no shell): `cat` echoes the stdin
    /// payload verbatim; shell syntax in args would NOT be interpreted
    #[cfg(unix)]
    #[tokio::test]
    async fn runner_process_kind_runs_argv_without_shell() {
        let mut process = entry("cat");
        process.kind = pig_protocol::HookKind::Process;
        let (tx, _rx) = async_channel::unbounded::<Event>();
        let run = HookRunCtx {
            session_id: "s-proc".into(),
            cwd: std::env::temp_dir(),
            tx,
            cancel: CancellationToken::new(),
        };
        let payload = HookPayload::stop(
            "s-proc",
            &run.cwd,
            pig_protocol::ExecMode::Auto.permissions(),
            "",
            false,
        );
        let expected = payload.to_json_line(None);
        let raw = run_entry(&process, &payload, &run, 5000, 32768).await;
        assert_eq!(raw.outcome_tag(), "success");
        assert_eq!(raw.stdout, expected);
        // A missing program is a clean SpawnFailed, not a shell error message
        let mut missing = entry("definitely-not-a-real-program-pig");
        missing.kind = pig_protocol::HookKind::Process;
        let raw = run_entry(&missing, &payload, &run, 5000, 32768).await;
        assert_eq!(raw.outcome_tag(), "spawn_failed");
    }
}
