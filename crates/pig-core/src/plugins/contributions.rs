use std::path::Path;

use pig_utils::skills::{self, Skill, SkillSource};

use super::{PersistedPlugins, authorized_package};

pub fn enabled_packages(
    data_dir: &Path,
    trust: &crate::config::WorkspaceTrust,
) -> Result<Vec<pig_protocol::PluginPackageInfo>, pig_protocol::CoreError> {
    let path = data_dir.join(super::PLUGIN_STATE_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(pig_protocol::CoreError::PluginManifest {
                detail: error.to_string(),
            });
        }
    };
    let state: PersistedPlugins = serde_json::from_slice(&bytes).map_err(|error| {
        pig_protocol::CoreError::PluginManifest {
            detail: format!("{}: {error}", path.display()),
        }
    })?;
    let mut packages: Vec<_> = state
        .packages
        .into_iter()
        .filter(|package| authorized_package(data_dir, package, Some(trust)))
        .collect();
    packages.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(packages)
}

pub fn discover_skills_with_trust(cwd: &Path, data_dir: &Path, trusted: bool) -> Vec<Skill> {
    let trust = crate::config::WorkspaceTrust::new(cwd, trusted);
    let mut by_name: std::collections::BTreeMap<String, Skill> =
        skills::discover_with_trust(cwd, data_dir, trusted)
            .into_iter()
            .map(|skill| (skill.name.clone(), skill))
            .collect();
    if let Ok(packages) = enabled_packages(data_dir, &trust) {
        for package in packages {
            for contribution in package.contributions.skills {
                for skill in
                    skills::discover_root(&package.root.join(contribution.path), SkillSource::User)
                {
                    by_name.insert(skill.name.clone(), skill);
                }
            }
        }
    }
    let disabled = skills::load_disabled_map(data_dir);
    by_name
        .into_values()
        .filter(|skill| !disabled.contains(&skill.path.to_string_lossy().replace('\\', "/")))
        .collect()
}

pub fn skills_section_with_trust(cwd: &Path, data_dir: &Path, trusted: bool) -> String {
    skills::skills_section(discover_skills_with_trust(cwd, data_dir, trusted))
}

pub fn load_skill_output_with_trust(
    cwd: &Path,
    data_dir: &Path,
    name: &str,
    trusted: bool,
) -> Result<String, String> {
    let skills = discover_skills_with_trust(cwd, data_dir, trusted);
    let skill = skills::find_skill(&skills, name).ok_or_else(|| {
        let available: Vec<_> = skills.iter().map(|skill| skill.name.as_str()).collect();
        if available.is_empty() {
            format!("Skill \"{name}\" does not exist; no skills are currently available")
        } else {
            format!(
                "Skill \"{name}\" does not exist. Available skills: {}",
                available.join(", ")
            )
        }
    })?;
    skills::load_skill_output(skill)
}

pub fn mcp_servers(
    _cwd: &Path,
    data_dir: &Path,
    trust: &crate::config::WorkspaceTrust,
) -> Vec<crate::mcp::McpServerConfig> {
    let mut servers = Vec::new();
    if let Ok(packages) = enabled_packages(data_dir, trust) {
        for package in packages {
            for contribution in package.contributions.mcp {
                match crate::mcp::validate_server(&contribution.config) {
                    Ok(shape) if !shape.disabled => servers.push(crate::mcp::McpServerConfig {
                        name: contribution.name,
                        transport: shape.transport,
                        timeout: shape.timeout,
                        disabled: false,
                    }),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        "plugin {} contributed an invalid MCP server: {}",
                        package.id,
                        error.detail()
                    ),
                }
            }
        }
    }
    servers
}

pub fn skills_state(data_dir: &Path, trusted: bool, base: String) -> String {
    let packages =
        std::fs::read_to_string(data_dir.join(super::PLUGIN_STATE_FILE)).unwrap_or_default();
    format!(
        "{}{base}{packages}",
        if trusted { "trusted:" } else { "untrusted:" }
    )
}
