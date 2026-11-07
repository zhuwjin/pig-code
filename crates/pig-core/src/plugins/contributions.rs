use std::path::Path;

use pig_protocol::{ModelConfig, ProviderConfig};
use pig_utils::skills::{self, Skill, SkillSource};

use super::{PluginRegistry, authorized_package, package_digest};

pub fn enabled_packages(
    registry: &PluginRegistry,
    trust: &crate::config::WorkspaceTrust,
) -> Result<Vec<pig_protocol::PluginPackageInfo>, pig_protocol::CoreError> {
    let layer = trust
        .is_trusted()
        .then(|| super::config::load_workspace_layer(trust.root()));
    let mut packages: Vec<_> = registry
        .snapshot
        .packages
        .iter()
        .filter(|package| {
            if layer
                .as_ref()
                .is_some_and(|layer| layer.enabled.get(&package.id) == Some(&false))
            {
                return false;
            }
            if authorized_package(&registry.data_dir, package, Some(trust)) {
                return true;
            }
            // A trusted workspace may enable a resource-only package it
            // disabled globally; a package with a backend stays a user
            // decision (its Engine is global).
            layer
                .as_ref()
                .is_some_and(|layer| layer.enabled.get(&package.id) == Some(&true))
                && !package.enabled
                && package.backend.is_none()
                && package.incompatible.is_none()
                && package_digest(&package.root).ok().as_deref() == Some(&package.content_digest)
        })
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

/// One provider a session can select: the host-facing [`ProviderConfig`] plus
/// the credential scope the plugin declared. Provider ids are namespaced with
/// `plugin:<package>:<id>` so a package can never shadow a user-configured
/// provider.
pub struct PluginProviderRuntime {
    pub provider: ProviderConfig,
    pub credential: Option<crate::credentials::CredentialId>,
}

impl PluginProviderRuntime {
    pub fn provider_id(package: &str, id: &str) -> String {
        format!("plugin:{package}:{id}")
    }
}

/// Effective provider contributions of enabled, compatible, authorized
/// packages, in package-id order. The first package contributing a given raw
/// provider id wins; the conflict is reported by the registry snapshot.
pub fn providers(
    registry: &PluginRegistry,
    trust: &crate::config::WorkspaceTrust,
) -> Vec<PluginProviderRuntime> {
    let Ok(packages) = enabled_packages(registry, trust) else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut providers = Vec::new();
    for package in packages {
        for contribution in package.contributions.providers {
            if !seen.insert(contribution.id.clone()) {
                continue;
            }
            let credential = contribution.credential.as_ref().and_then(|id| {
                package
                    .contributions
                    .credentials
                    .iter()
                    .find(|declaration| declaration.id() == id)
                    .and_then(|declaration| match declaration {
                        pig_protocol::PluginCredentialDeclaration::Owned { id, .. } => {
                            Some(crate::credentials::CredentialId::plugin(&package.id, id))
                        }
                        // Validation rejects this for provider contributions.
                        pig_protocol::PluginCredentialDeclaration::Provider { .. } => None,
                    })
            });
            let provider = ProviderConfig {
                id: PluginProviderRuntime::provider_id(&package.id, &contribution.id),
                name: contribution.name,
                base_url: contribution.base_url,
                requires_api_key: contribution.credential.is_some(),
                api_format: contribution.api_format,
                enabled: true,
                models: contribution
                    .models
                    .into_iter()
                    .map(|model| ModelConfig {
                        id: model.id,
                        name: model.name,
                        icon: None,
                        enabled: true,
                        context_window: model.context_window,
                        max_output_tokens: model.max_output_tokens,
                        input_image: model.input_image,
                        input_pdf: model.input_pdf,
                        cap_structured: model.cap_structured,
                        cap_strict_tools: model.cap_strict_tools,
                        cap_web_search: model.cap_web_search,
                        web_search_tool: None,
                        reasoning_levels: model.reasoning_levels,
                        default_reasoning_level: model.default_reasoning_level,
                        reasoning_labels: model.reasoning_labels.into_iter().collect(),
                        reasoning_params: model.reasoning_params.into_iter().collect(),
                        parallel_tool_calls: model.parallel_tool_calls,
                    })
                    .collect(),
                icon: contribution.icon,
                key_url: None,
            };
            providers.push(PluginProviderRuntime {
                provider,
                credential,
            });
        }
    }
    providers
}

pub fn discover_skills_with_trust(
    cwd: &Path,
    plugins: &super::SharedPluginRegistry,
    trusted: bool,
) -> Vec<Skill> {
    let registry = plugins.lock().expect("plugin registry lock");
    let trust = crate::config::WorkspaceTrust::new(cwd, trusted);
    // Precedence, deterministic and independent of discovery order: user and
    // project skills win, then the first enabled package by id within the
    // plugin level. A plugin can never shadow a user's skill.
    let mut by_name: std::collections::BTreeMap<String, Skill> = std::collections::BTreeMap::new();
    if let Ok(packages) = enabled_packages(&registry, &trust) {
        for package in packages {
            for contribution in package.contributions.skills {
                for skill in
                    skills::discover_root(&package.root.join(contribution.path), SkillSource::User)
                {
                    if let Some(existing) = by_name.get(&skill.name) {
                        tracing::warn!(
                            skill = skill.name,
                            winner = %existing.directory.display(),
                            loser = package.id,
                            "two plugins contribute the same skill name"
                        );
                        continue;
                    }
                    by_name.insert(skill.name.clone(), skill);
                }
            }
        }
    }
    for skill in skills::discover_with_trust(cwd, &registry.data_dir, trusted) {
        if by_name.contains_key(&skill.name) {
            tracing::warn!(
                skill = skill.name,
                "the user or project skill overrides a plugin skill of the same name"
            );
        }
        by_name.insert(skill.name.clone(), skill);
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
