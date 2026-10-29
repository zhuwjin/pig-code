use super::*;

use crate::config::WorkspaceTrust;

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
    if let Err(error) = crate::config::ensure_private_data_dir(data_dir) {
        return Err(format!(
            "Failed to secure data directory {}: {error:?}",
            data_dir.display()
        ));
    }
    let raw = serde_json::to_string_pretty(&state)
        .map_err(|e| format!("Failed to serialize agents-state: {e}"))?;
    crate::config::write_private_file(&state_path(data_dir), raw.as_bytes())
        .map_err(|e| format!("Failed to write agents-state.json: {e:?}"))
}

/// Load profiles under one workspace-trust decision. A trusted workspace keeps
/// the project level (project > user > built-in); an untrusted one uses only
/// the user and built-in levels. The trust token must name the same canonical
/// workspace, so it cannot be reused for another checkout.
pub fn load_profiles_with_trust(
    cwd: &Path,
    data_dir: &Path,
    trust: &WorkspaceTrust,
) -> Vec<AgentProfile> {
    load_profiles_inner(cwd, data_dir, trust.admits(cwd))
}

fn load_profiles_inner(cwd: &Path, data_dir: &Path, include_project: bool) -> Vec<AgentProfile> {
    let mut by_name: HashMap<String, AgentProfile> = HashMap::new();
    for profile in builtin_profiles() {
        by_name.insert(profile.name.clone(), profile);
    }
    // Directory precedence: project > user > built-in; same name replaces wholesale
    let mut levels = vec![(data_dir.join("agents"), AgentSource::User)];
    if include_project {
        levels.push((cwd.join(".pigcode").join("agents"), AgentSource::Project));
    }
    for (dir, source) in levels {
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

#[cfg(test)]
mod trust_tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pig-agent-store-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_profile(directory: &Path, file: &str, name: &str, description: &str) {
        std::fs::create_dir_all(directory).expect("agents dir");
        std::fs::write(
            directory.join(file),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
        )
        .expect("profile");
    }

    /// Project profiles are repository content: they only activate through a
    /// trust token for this workspace; an untrusted workspace sees user-level
    /// and built-in profiles only.
    #[test]
    fn project_profiles_require_trust() {
        let tmp = TempDir::new("trust");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("workspace");
        let other = tmp.0.join("other");
        write_profile(
            &data.join("agents"),
            "user.md",
            "user-agent",
            "user-level profile",
        );
        write_profile(
            &ws.join(".pigcode/agents"),
            "explore.md",
            "explore",
            "project explore",
        );
        write_profile(
            &ws.join(".pigcode/agents"),
            "new.md",
            "project-agent",
            "project-only profile",
        );

        let names = |profiles: Vec<AgentProfile>| {
            profiles
                .into_iter()
                .map(|profile| profile.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(load_profiles_with_trust(
                &ws,
                &data,
                &WorkspaceTrust::new(&ws, /*trusted*/ true)
            )),
            vec!["explore", "general-purpose", "project-agent", "user-agent"],
            "a trusted workspace sees every level"
        );

        let untrusted = load_profiles_with_trust(&ws, &data, &WorkspaceTrust::untrusted(&ws));
        assert!(
            !names(untrusted.clone()).contains(&"project-agent".to_string()),
            "an untrusted project profile does not load"
        );
        let explore = untrusted
            .iter()
            .find(|profile| profile.name == "explore")
            .expect("builtin explore");
        assert_eq!(explore.source, AgentSource::BuiltIn);

        let trusted = load_profiles_with_trust(&ws, &data, &WorkspaceTrust::new(&ws, true));
        let trusted_names = names(trusted.clone());
        assert!(trusted_names.contains(&"project-agent".to_string()));
        let explore = trusted
            .iter()
            .find(|profile| profile.name == "explore")
            .expect("project explore");
        assert_eq!(explore.source, AgentSource::Project);
        assert_eq!(explore.description, "project explore");

        let foreign = load_profiles_with_trust(&ws, &data, &WorkspaceTrust::new(&other, true));
        assert!(!names(foreign).contains(&"project-agent".to_string()));
    }
}
