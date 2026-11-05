use std::path::Path;

use pig_utils::skills::{self, Skill, SkillSource};

use super::{PluginRegistry, authorized_package};

pub fn enabled_packages(
    registry: &PluginRegistry,
    trust: &crate::config::WorkspaceTrust,
) -> Result<Vec<pig_protocol::PluginPackageInfo>, pig_protocol::CoreError> {
    let mut packages: Vec<_> = registry
        .snapshot
        .packages
        .iter()
        .filter(|package| authorized_package(&registry.data_dir, package, Some(trust)))
        .cloned()
        .map(|mut package| {
            if let Some(contributions) = registry
                .snapshot
                .runtime_contributions
                .get(&format!("{}:{}", package.id, package.digest))
            {
                package.contributions = package.contributions.merged(contributions);
            }
            package
        })
        .collect();
    packages.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(packages)
}

pub fn discover_skills_with_trust(
    cwd: &Path,
    plugins: &super::SharedPluginRegistry,
    trusted: bool,
) -> Vec<Skill> {
    let registry = plugins.lock().expect("plugin registry lock");
    let trust = crate::config::WorkspaceTrust::new(cwd, trusted);
    let mut by_name: std::collections::BTreeMap<String, Skill> =
        skills::discover_with_trust(cwd, &registry.data_dir, trusted)
            .into_iter()
            .map(|skill| (skill.name.clone(), skill))
            .collect();
    if let Ok(packages) = enabled_packages(&registry, &trust) {
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
    let disabled = skills::load_disabled_map(&registry.data_dir);
    by_name
        .into_values()
        .filter(|skill| !disabled.contains(&skill.path.to_string_lossy().replace('\\', "/")))
        .collect()
}

pub fn skills_section_with_trust(
    cwd: &Path,
    plugins: &super::SharedPluginRegistry,
    trusted: bool,
) -> String {
    skills::skills_section(discover_skills_with_trust(cwd, plugins, trusted))
}

pub fn load_skill_output_with_trust(
    cwd: &Path,
    plugins: &super::SharedPluginRegistry,
    name: &str,
    trusted: bool,
) -> Result<String, String> {
    let skills = discover_skills_with_trust(cwd, plugins, trusted);
    let skill = skills::find_skill(&skills, name).ok_or_else(|| {
        let available: Vec<String> = skills.iter().map(|skill| skill.name.clone()).collect();
        if available.is_empty() {
            "No skills are currently available".to_owned()
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
    plugins: &super::SharedPluginRegistry,
    trust: &crate::config::WorkspaceTrust,
) -> Vec<crate::mcp::McpServerConfig> {
    let registry = plugins.lock().expect("plugin registry lock");
    let mut servers = Vec::new();
    if let Ok(packages) = enabled_packages(&registry, trust) {
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
