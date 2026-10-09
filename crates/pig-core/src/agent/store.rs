use super::*;

/// agents-state.json: runtime-writable subagent overrides (for changing subagent models in the settings page)
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct AgentsState {
    #[serde(default)]
    model_overrides: HashMap<String, ModelOverride>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thought_level: Option<String>,
}

pub(crate) fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agents-state.json")
}

/// Missing file / parse failure = empty state (non-fatal)
pub(crate) fn load_agents_state(data_dir: &Path) -> AgentsState {
    let Ok(raw) = std::fs::read_to_string(state_path(data_dir)) else {
        return AgentsState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Read-modify-write agents-state.json: passing None for both keys deletes the entry for
/// that name. create_dir_all before writing; serialized pretty. Used by the settings page UI.
pub fn set_model_override(
    data_dir: &Path,
    name: &str,
    model: Option<&str>,
    thought_level: Option<&str>,
) -> Result<(), String> {
    let mut state = load_agents_state(data_dir);
    // Blank strings are treated as None
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    let thought_level = thought_level.map(str::trim).filter(|l| !l.is_empty());
    match (model, thought_level) {
        (None, None) => {
            state.model_overrides.remove(name);
        }
        (model, thought_level) => {
            state.model_overrides.insert(
                name.to_string(),
                ModelOverride {
                    model: model.map(str::to_string),
                    thought_level: thought_level.map(str::to_string),
                },
            );
        }
    }
    std::fs::create_dir_all(data_dir).map_err(|e| {
        format!(
            "Failed to create data directory {}: {e}",
            data_dir.display()
        )
    })?;
    let raw = serde_json::to_string_pretty(&state)
        .map_err(|e| format!("Failed to serialize agents-state: {e}"))?;
    std::fs::write(state_path(data_dir), raw)
        .map_err(|e| format!("Failed to write agents-state.json: {e}"))
}

/// Load all subagent profiles: built-in -> user -> project, later ones overriding earlier
/// by name; then apply the agents-state.json model overrides; finally sort the output by name.
pub fn load_profiles(cwd: &Path, data_dir: &Path) -> Vec<AgentProfile> {
    let mut by_name: HashMap<String, AgentProfile> = HashMap::new();
    for profile in builtin_profiles() {
        by_name.insert(profile.name.clone(), profile);
    }
    // Directory precedence: project > user > built-in; same name replaces wholesale
    for (dir, source) in [
        (data_dir.join("agents"), AgentSource::User),
        (cwd.join(".pigcode").join("agents"), AgentSource::Project),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // missing directory = empty
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort(); // stable order by file name within a directory for predictable behavior
        for path in files {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue; // skip read failures, non-fatal
            };
            match parse_agent_markdown(&content) {
                Ok(mut profile) => {
                    profile.source = source;
                    by_name.insert(profile.name.clone(), profile);
                }
                Err(e) => {
                    tracing::warn!(
                        "skipping unparsable subagent profile {}: {e}",
                        path.display()
                    );
                }
            }
        }
    }
    // State overrides: they apply only to profiles "whose frontmatter does not explicitly
    // specify a model" (built-ins always qualify), so they are applied uniformly after the
    // override merge — user/project profiles replacing built-ins by name still keep their
    // overrides.
    let state = load_agents_state(data_dir);
    for (name, profile) in by_name.iter_mut() {
        if profile.model.is_some() {
            continue;
        }
        if let Some(override_) = state.model_overrides.get(name) {
            if let Some(model) = override_.model.as_ref().filter(|m| !m.trim().is_empty()) {
                profile.model = Some(model.clone());
            }
            if let Some(level) = override_
                .thought_level
                .as_ref()
                .filter(|l| !l.trim().is_empty())
            {
                profile.thought_level = Some(level.clone());
            }
        }
    }
    let mut profiles: Vec<AgentProfile> = by_name.into_values().collect();
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

/// Normalization: lowercase + drop whitespace/hyphens/underscores ("My Agent" ≈ "my-agent" ≈ "my_agent")
pub(crate) fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}
