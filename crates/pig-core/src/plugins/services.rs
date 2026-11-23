use std::path::{Path, PathBuf};

use pig_protocol::{
    CoreError, McpServerStatus, PluginMcpServerInfo, PluginMcpSnapshot, PluginScope,
    PluginServiceSnapshot, PluginSkillInfo, PluginSkillsSnapshot, PluginWorkspace, WorkspaceMeta,
};
use pig_utils::skills::{self, SkillSource};

use super::{PluginRegistry, REDACTED, enabled_packages};

pub struct SessionServices {
    pub id: String,
    pub cwd: PathBuf,
    pub statuses: Option<Vec<McpServerStatus>>,
    pub stats: pig_protocol::PluginSessionStats,
    pub turns: Vec<pig_protocol::PluginTurnStats>,
}

pub fn snapshot(
    registry: &PluginRegistry,
    scope: PluginScope,
    workspaces: Vec<WorkspaceMeta>,
    session: Option<SessionServices>,
    trust: &crate::config::WorkspaceTrust,
) -> PluginServiceSnapshot {
    let workspace = match &scope {
        PluginScope::User => None,
        PluginScope::Project { workspace } => Some(workspace.clone()),
    };
    let current_cwd = session.as_ref().map(|session| session.cwd.clone());
    let workspaces: Vec<PluginWorkspace> = workspaces
        .into_iter()
        .filter(|workspace| !workspace.hidden)
        .map(|workspace| PluginWorkspace {
            current: current_cwd.as_ref() == Some(&workspace.path),
            name: workspace.alias.unwrap_or_else(|| {
                workspace
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| workspace.path.display().to_string())
            }),
            path: workspace.path,
        })
        .collect();
    let status_applicable = match (&scope, &current_cwd) {
        (PluginScope::User, Some(_)) => true,
        (PluginScope::Project { workspace }, Some(cwd)) => workspace == cwd,
        _ => false,
    };
    let session = session.filter(|_| status_applicable);
    let session_projection =
        session
            .as_ref()
            .map(|session| pig_protocol::PluginSessionProjection {
                id: session.id.clone(),
                cwd: session.cwd.clone(),
                stats: session.stats.clone(),
                turns: session.turns.clone(),
            });
    PluginServiceSnapshot {
        generation: registry.snapshot.generation,
        locale: String::new(),
        skills: skills_snapshot(registry, workspace.as_deref(), trust),
        mcp: mcp_snapshot(registry, workspace.as_deref(), session, trust),
        scope,
        workspaces,
        session: session_projection,
    }
}

fn skills_snapshot(
    registry: &PluginRegistry,
    workspace: Option<&Path>,
    trust: &crate::config::WorkspaceTrust,
) -> PluginSkillsSnapshot {
    let data_dir = &registry.data_dir;
    let user_path = skills::user_root(data_dir);
    let project_path = workspace.map(skills::project_root);
    let disabled = skills::load_disabled_map(data_dir);
    let mut rows = discover(&user_path, PluginScope::User, &disabled);
    if let (Some(root), Some(workspace)) = (&project_path, workspace) {
        rows.extend(discover(
            root,
            PluginScope::Project {
                workspace: workspace.to_path_buf(),
            },
            &disabled,
        ));
    }
    if let Ok(packages) = enabled_packages(registry, trust) {
        for package in packages {
            for contribution in package.contributions.skills {
                rows.extend(
                    discover(
                        &package.root.join(contribution.path),
                        PluginScope::User,
                        &disabled,
                    )
                    .into_iter()
                    .map(|mut skill| {
                        skill.package = Some(package.id.clone());
                        skill.markdown = None;
                        skill
                    }),
                );
            }
        }
    }
    let mut merged = Vec::new();
    for mut skill in rows {
        if let Some(existing) = merged
            .iter_mut()
            .find(|row: &&mut PluginSkillInfo| row.name == skill.name)
        {
            skill.overrides_user = true;
            *existing = skill;
        } else {
            merged.push(skill);
        }
    }
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    for skill in &mut merged {
        if skill.package.is_none() {
            skill.markdown = std::fs::read_to_string(&skill.path).ok();
        }
    }
    PluginSkillsSnapshot {
        user_path,
        project_path,
        skills: merged,
    }
}

fn discover(
    root: &Path,
    scope: PluginScope,
    disabled: &std::collections::HashSet<String>,
) -> Vec<PluginSkillInfo> {
    skills::discover_root(root, SkillSource::User)
        .into_iter()
        .map(|skill| PluginSkillInfo {
            name: skill.name,
            dir_name: skill
                .directory
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            enabled: !disabled.contains(&skill.path.to_string_lossy().replace('\\', "/")),
            path: skill.path,
            description: skill.description,
            when_to_use: skill.when_to_use,
            scope: scope.clone(),
            package: None,
            overrides_user: false,
            has_frontmatter: skill.has_frontmatter,
            markdown: None,
        })
        .collect()
}

fn mcp_snapshot(
    registry: &PluginRegistry,
    workspace: Option<&Path>,
    session: Option<SessionServices>,
    trust: &crate::config::WorkspaceTrust,
) -> PluginMcpSnapshot {
    let data_dir = &registry.data_dir;
    let user_path = data_dir.join("mcp.json");
    let project_path = workspace.map(|workspace| workspace.join(".pigcode/mcp.json"));
    let mut rows = read_mcp(&user_path, PluginScope::User);
    if let (Some(path), Some(workspace)) = (&project_path, workspace) {
        rows.extend(read_mcp(
            path,
            PluginScope::Project {
                workspace: workspace.to_path_buf(),
            },
        ));
    }
    if let Ok(packages) = enabled_packages(registry, trust) {
        for package in packages {
            for contribution in package.contributions.mcp {
                rows.push(mcp_info(
                    contribution.name,
                    contribution.config,
                    PluginScope::User,
                    Some(package.id.clone()),
                ));
            }
        }
    }
    let mut merged = Vec::new();
    for mut row in rows {
        if let Some(existing) = merged
            .iter_mut()
            .find(|server: &&mut PluginMcpServerInfo| server.name == row.name)
        {
            row.overrides_user = true;
            *existing = row;
        } else {
            merged.push(row);
        }
    }
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(session) = &session {
        for row in &mut merged {
            row.status = session.statuses.as_ref().and_then(|statuses| {
                statuses
                    .iter()
                    .find(|status| status.name == row.name)
                    .cloned()
            });
        }
    }
    PluginMcpSnapshot {
        user_path,
        project_path,
        servers: merged,
        status_applicable: session.is_some(),
        session_id: session.map(|session| session.id),
    }
}

fn read_mcp(path: &Path, scope: PluginScope) -> Vec<PluginMcpServerInfo> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(file) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    file.get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .map(|servers| {
            servers
                .iter()
                .map(|(name, config)| mcp_info(name.clone(), config.clone(), scope.clone(), None))
                .collect()
        })
        .unwrap_or_default()
}

fn mcp_info(
    name: String,
    config: serde_json::Value,
    scope: PluginScope,
    package: Option<String>,
) -> PluginMcpServerInfo {
    let validated = crate::mcp::validate_server(&config);
    let (summary, timeout_ms, invalid_reason) = match validated {
        Ok(shape) => {
            let summary = match shape.transport {
                crate::mcp::McpTransport::Stdio(stdio) => {
                    format!("{} {}", stdio.command, stdio.args.join(" "))
                        .trim()
                        .to_owned()
                }
                crate::mcp::McpTransport::Http(http) => http.url,
            };
            (summary, shape.timeout.as_millis() as u64, None)
        }
        Err(error) => (String::new(), 30_000, Some(error.detail())),
    };
    PluginMcpServerInfo {
        name,
        scope,
        package,
        config: redact_config(&config),
        summary,
        disabled: config
            .get("disabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        timeout_ms,
        invalid_reason,
        overrides_user: false,
        read_only: false,
        status: None,
    }
}

fn redact_config(config: &serde_json::Value) -> serde_json::Value {
    let mut config = config.clone();
    if let Some(map) = config.as_object_mut() {
        for field in ["env", "headers"] {
            if let Some(values) = map
                .get_mut(field)
                .and_then(serde_json::Value::as_object_mut)
            {
                for value in values.values_mut() {
                    *value = serde_json::Value::String(REDACTED.to_owned());
                }
            }
        }
        if let Some(oauth) = map
            .get_mut("oauth")
            .and_then(serde_json::Value::as_object_mut)
        {
            for secret_key in ["clientSecret", "accessToken", "refreshToken"] {
                if let Some(value) = oauth.get_mut(secret_key)
                    && !value.is_null()
                {
                    *value = serde_json::Value::String(REDACTED.to_owned());
                }
            }
        }
    }
    config
}

pub fn upsert_mcp(
    data_dir: &Path,
    scope: &PluginScope,
    name: &str,
    config: serde_json::Value,
) -> Result<(), CoreError> {
    if name.trim().is_empty() {
        return Err(write_error("the MCP server name must not be empty"));
    }
    let path = mcp_path(data_dir, scope)?;
    let mut servers = read_mcp_object(&path)?;
    let config = restore_redacted(config, servers.get(name));
    crate::mcp::validate_server(&config).map_err(|error| write_error(error.detail()))?;
    servers.insert(name.to_owned(), config);
    write_mcp_object(&path, servers, scope)
}

pub fn delete_mcp(data_dir: &Path, scope: &PluginScope, name: &str) -> Result<(), CoreError> {
    let path = mcp_path(data_dir, scope)?;
    let mut servers = read_mcp_object(&path)?;
    servers.remove(name);
    write_mcp_object(&path, servers, scope)
}

pub fn set_mcp_enabled(
    data_dir: &Path,
    scope: &PluginScope,
    name: &str,
    disabled: bool,
) -> Result<(), CoreError> {
    let path = mcp_path(data_dir, scope)?;
    let mut servers = read_mcp_object(&path)?;
    if let Some(config) = servers
        .get_mut(name)
        .and_then(serde_json::Value::as_object_mut)
    {
        if disabled {
            config.insert("disabled".into(), true.into());
        } else {
            config.remove("disabled");
        }
    }
    write_mcp_object(&path, servers, scope)
}

fn restore_redacted(
    mut config: serde_json::Value,
    previous: Option<&serde_json::Value>,
) -> serde_json::Value {
    for field in ["env", "headers", "oauth"] {
        let Some(new_values) = config
            .get_mut(field)
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        for (name, value) in new_values {
            if value.as_str() == Some(REDACTED)
                && let Some(previous) = previous
                    .and_then(|previous| previous.get(field))
                    .and_then(|previous| previous.get(name))
            {
                *value = previous.clone();
            }
        }
    }
    config
}

fn read_mcp_object(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>, CoreError> {
    let file = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| write_error(format!("{} is invalid JSON: {error}", path.display())))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(write_error(error.to_string())),
    };
    if !file.is_object() {
        return Err(write_error(format!(
            "{} must contain an object",
            path.display()
        )));
    }
    Ok(file
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default())
}

fn write_mcp_object(
    path: &Path,
    servers: serde_json::Map<String, serde_json::Value>,
    scope: &PluginScope,
) -> Result<(), CoreError> {
    if matches!(scope, PluginScope::Project { .. }) {
        let parent = path
            .parent()
            .ok_or_else(|| write_error("project MCP path has no parent"))?;
        crate::config::ensure_real_directory(parent)
            .map_err(|error| write_error(error.to_string()))?;
    }
    let mut file = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| write_error(error.to_string()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(write_error(error.to_string())),
    };
    file["mcpServers"] = serde_json::Value::Object(servers);
    let mut bytes =
        serde_json::to_vec_pretty(&file).map_err(|error| write_error(error.to_string()))?;
    bytes.push(b'\n');
    crate::config::write_settings_file(path, &bytes).map_err(|error| write_error(error.to_string()))
}

fn mcp_path(data_dir: &Path, scope: &PluginScope) -> Result<PathBuf, CoreError> {
    match scope {
        PluginScope::User => Ok(data_dir.join("mcp.json")),
        PluginScope::Project { workspace } if workspace.is_absolute() => {
            Ok(workspace.join(".pigcode/mcp.json"))
        }
        PluginScope::Project { .. } => {
            Err(permission("project scope requires an absolute workspace"))
        }
    }
}

pub fn save_skill(
    data_dir: &Path,
    scope: &PluginScope,
    dir_name: &str,
    markdown: &str,
    create: bool,
) -> Result<(), CoreError> {
    if dir_name.is_empty()
        || dir_name.len() > 64
        || !dir_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(write_error(
            "skill directory names must contain 1-64 lowercase letters, digits or dashes",
        ));
    }
    if markdown.is_empty() || markdown.len() as u64 > skills::MAX_SKILL_BYTES {
        return Err(write_error(
            "SKILL.md must be non-empty and at most 100 KiB",
        ));
    }
    let root = skill_root(data_dir, scope)?;
    if matches!(scope, PluginScope::User) {
        crate::config::ensure_private_data_dir(data_dir)?;
    }
    if matches!(scope, PluginScope::Project { .. }) {
        let parent = root
            .parent()
            .ok_or_else(|| write_error("project skill root has no parent"))?;
        crate::config::ensure_real_directory(parent)
            .map_err(|error| write_error(error.to_string()))?;
    }
    crate::config::ensure_real_directory(&root).map_err(|error| write_error(error.to_string()))?;
    let directory = root.join(dir_name);
    if create {
        if std::fs::symlink_metadata(&directory).is_ok() {
            return Err(write_error(format!(
                "skill directory already exists: {}",
                directory.display()
            )));
        }
        std::fs::create_dir(&directory).map_err(|error| write_error(error.to_string()))?;
    }
    crate::config::ensure_real_directory(&directory)
        .map_err(|error| write_error(error.to_string()))?;
    crate::config::write_settings_file(&directory.join(skills::SKILL_FILE), markdown.as_bytes())
        .map_err(|error| write_error(error.to_string()))
}

pub fn delete_skill(data_dir: &Path, scope: &PluginScope, dir_name: &str) -> Result<(), CoreError> {
    if dir_name.is_empty() || dir_name.contains(['/', '\\']) || matches!(dir_name, "." | "..") {
        return Err(write_error("invalid skill directory name"));
    }
    let root = skill_root(data_dir, scope)?;
    if matches!(scope, PluginScope::Project { .. }) {
        crate::config::require_real_directory(root.parent().unwrap_or(&root))
            .map_err(|error| write_error(error.to_string()))?;
    }
    crate::config::require_real_directory(&root).map_err(|error| write_error(error.to_string()))?;
    let directory = root.join(dir_name);
    crate::config::require_real_directory(&directory)
        .map_err(|error| write_error(error.to_string()))?;
    std::fs::remove_dir_all(directory).map_err(|error| write_error(error.to_string()))
}

pub fn set_skill_enabled(
    data_dir: &Path,
    plugins: &super::SharedPluginRegistry,
    path: &Path,
    enabled: bool,
    workspaces: &[PathBuf],
    config: Option<&pig_protocol::AppConfig>,
) -> Result<(), CoreError> {
    let mut roots = vec![skills::user_root(data_dir)];
    roots.extend(
        workspaces
            .iter()
            .map(|workspace| skills::project_root(workspace)),
    );
    {
        let registry = plugins.lock().expect("plugin registry lock");
        if let Ok(packages) = enabled_packages(
            &registry,
            &crate::config::WorkspaceTrust::untrusted(Path::new(".")),
        ) {
            for package in packages.into_iter().filter(|package| {
                !matches!(package.source, pig_protocol::PluginSource::Project { .. })
            }) {
                roots.extend(
                    package
                        .contributions
                        .skills
                        .into_iter()
                        .map(|skill| package.root.join(skill.path)),
                );
            }
        }
    }
    if let Some(config) = config {
        for workspace in workspaces {
            let trust = crate::config::WorkspaceTrust::for_config(Some(config), workspace);
            let registry = plugins.lock().expect("plugin registry lock");
            if let Ok(packages) = enabled_packages(&registry, &trust) {
                for package in packages {
                    roots.extend(
                        package
                            .contributions
                            .skills
                            .into_iter()
                            .map(|skill| package.root.join(skill.path)),
                    );
                }
            }
        }
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| write_error(error.to_string()))?;
    #[cfg(windows)]
    let canonical = dunce::simplified(&canonical).to_path_buf();
    let inside_managed_root = roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| {
            #[cfg(windows)]
            let root = dunce::simplified(&root).to_path_buf();
            canonical.starts_with(root)
        });
    if path.file_name().and_then(|name| name.to_str()) != Some(skills::SKILL_FILE)
        || !inside_managed_root
    {
        return Err(permission(
            "the skill path is not in an enabled plugin or a managed skill root",
        ));
    }
    crate::config::ensure_private_data_dir(data_dir)?;
    let (state, text) = skills::skills_state_after_toggle(data_dir, path, enabled)?;
    crate::config::write_settings_file(&state, text.as_bytes()).map_err(|error| {
        CoreError::SkillsStateWrite {
            detail: error.to_string(),
        }
    })
}

fn skill_root(data_dir: &Path, scope: &PluginScope) -> Result<PathBuf, CoreError> {
    match scope {
        PluginScope::User => Ok(skills::user_root(data_dir)),
        PluginScope::Project { workspace } if workspace.is_absolute() => {
            Ok(skills::project_root(workspace))
        }
        PluginScope::Project { .. } => {
            Err(permission("project scope requires an absolute workspace"))
        }
    }
}

fn write_error(detail: impl Into<String>) -> CoreError {
    CoreError::PluginWrite {
        detail: detail.into(),
    }
}

fn permission(detail: impl Into<String>) -> CoreError {
    CoreError::PluginPermission {
        detail: detail.into(),
    }
}
