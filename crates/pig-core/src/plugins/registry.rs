use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use pig_protocol::{
    CoreError, InstalledPluginMetadata, PLUGIN_API_VERSION, PluginAction, PluginCapability,
    PluginContributions, PluginIssue, PluginManifest, PluginPackageInfo, PluginPackageSource,
    PluginSnapshot, PluginSource,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{BUILTIN_PACKAGES, GENERATED_TYPES, PLUGIN_STATE_FILE, PluginRegistry};

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct PersistedPlugins {
    #[serde(default)]
    pub packages: Vec<PluginPackageInfo>,
    #[serde(default)]
    pub enabled: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled_builtin: BTreeMap<String, String>,
    #[serde(default)]
    pub pending_updates: BTreeMap<String, String>,
    #[serde(default)]
    pub credential_grants: BTreeSet<pig_protocol::PluginCredentialGrant>,
}

impl PluginRegistry {
    pub fn new(data_dir: PathBuf) -> Self {
        Self::with_host_version(data_dir, pig_protocol::PLUGIN_HOST_VERSION)
    }

    /// A registry that checks manifests against `host_version` (tests and
    /// hosts that version their plugin API separately from the crate).
    pub fn with_host_version(data_dir: PathBuf, host_version: impl Into<String>) -> Self {
        let state = std::fs::read(data_dir.join(PLUGIN_STATE_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PersistedPlugins>(&bytes).ok())
            .unwrap_or_default();
        let mut registry = Self {
            data_dir,
            host_version: host_version.into(),
            generation: 0,
            snapshot: PluginSnapshot {
                packages: state.packages,
                credential_grants: state.credential_grants.iter().cloned().collect(),
                ..PluginSnapshot::default()
            },
            enabled: state.enabled,
            disabled_builtin: state.disabled_builtin,
            pending_updates: state.pending_updates,
            runtime_epochs: BTreeMap::new(),
            credential_grants: state.credential_grants,
        };
        registry.apply_enabled();
        registry
    }

    pub fn shared(data_dir: PathBuf) -> super::SharedPluginRegistry {
        Self::new(data_dir).into_shared()
    }

    pub fn into_shared(self) -> super::SharedPluginRegistry {
        std::sync::Arc::new(std::sync::Mutex::new(self))
    }

    pub fn sync(&mut self, sources: Vec<PluginPackageSource>) -> PluginSnapshot {
        self.generation += 1;
        self.snapshot.generation = self.generation;
        self.snapshot.packages.clear();
        self.snapshot.errors.clear();
        let mut ids = BTreeSet::new();
        for source in sources {
            match self.inspect(&source) {
                Ok(package)
                    if !ids.insert(format!(
                        "{}:{}:{:?}",
                        package.id, package.digest, package.source
                    )) =>
                {
                    self.snapshot.errors.push(PluginIssue {
                        root: source.root,
                        error: manifest(format!("duplicate plugin id `{}`", package.id)),
                    });
                }
                Ok(package) => self.snapshot.packages.push(package),
                Err(error) => self.snapshot.errors.push(PluginIssue {
                    root: source.root,
                    error,
                }),
            }
        }
        self.snapshot.packages.sort_by(|a, b| {
            a.id.cmp(&b.id)
                .then_with(|| a.digest.cmp(&b.digest))
                .then_with(|| a.root.cmp(&b.root))
        });
        self.pending_updates.retain(|id, digest| {
            self.snapshot
                .packages
                .iter()
                .any(|package| &package.id == id && &package.digest == digest)
        });
        let current: BTreeSet<String> = self
            .snapshot
            .packages
            .iter()
            .map(|package| format!("{}:{}", package.id, package.digest))
            .collect();
        self.snapshot
            .runtime_contributions
            .retain(|key, _| current.contains(key));
        self.runtime_epochs.retain(|key, _| current.contains(key));
        self.apply_enabled();
        self.persist();
        self.snapshot.clone()
    }

    pub fn sync_runtime_contributions(
        &mut self,
        id: &str,
        digest: &str,
        epoch: u64,
        generation: u64,
        contributions: pig_protocol::PluginContributions,
    ) -> Result<PluginSnapshot, CoreError> {
        if generation > self.snapshot.generation {
            return Err(permission(format!(
                "the runtime contribution generation is from the future: received {generation}, current {}",
                self.snapshot.generation
            )));
        }
        let key = format!("{id}:{digest}");
        if self
            .runtime_epochs
            .get(&key)
            .is_some_and(|current| *current > epoch)
        {
            return Ok(self.snapshot.clone());
        }
        let empty = contributions.themes.is_empty()
            && contributions.skills.is_empty()
            && contributions.mcp.is_empty()
            && contributions.tools.is_empty()
            && contributions.commands.is_empty();
        if empty {
            if self.runtime_epochs.get(&key) == Some(&epoch) {
                self.snapshot.runtime_contributions.remove(&key);
                self.runtime_epochs.remove(&key);
            } else {
                let package = self.package(id, digest)?;
                if !package.enabled {
                    return Err(permission(
                        "a disabled plugin cannot register contributions",
                    ));
                }
                self.snapshot
                    .runtime_contributions
                    .insert(key.clone(), PluginContributions::default());
                self.runtime_epochs.insert(key, epoch);
            }
            self.generation += 1;
            self.snapshot.generation = self.generation;
            self.persist();
            return Ok(self.snapshot.clone());
        }
        let package = self.package(id, digest)?.clone();
        if !package.enabled {
            return Err(permission(
                "a disabled plugin cannot register contributions",
            ));
        }
        if !contributions.credentials.is_empty()
            || !contributions.http.is_empty()
            || !contributions.actions.is_empty()
            || !contributions.config.items.is_empty()
            || !contributions.config.read.is_empty()
            || !contributions.config.write.is_empty()
        {
            return Err(permission(
                "runtime contributions cannot change permissions, credentials, HTTP or configuration schemas",
            ));
        }
        let mut validated = contributions.clone();
        validated.actions = package.contributions.actions.clone();
        validate_contributions(&package.root, &validated)?;
        self.snapshot
            .runtime_contributions
            .insert(key.clone(), contributions);
        self.runtime_epochs.insert(key, epoch);
        self.generation += 1;
        self.snapshot.generation = self.generation;
        self.persist();
        Ok(self.snapshot.clone())
    }

    pub(crate) fn inspect(
        &self,
        source: &PluginPackageSource,
    ) -> Result<PluginPackageInfo, CoreError> {
        let root = canonical(&source.root)?;
        let manifest_path = root.join(pig_protocol::PLUGIN_MANIFEST_FILE);
        let package_manifest: PluginManifest = read_json(&manifest_path)?;
        if package_manifest.api_version != pig_protocol::PLUGIN_API_VERSION
            || package_manifest.protocols.ui != 1
            || package_manifest.protocols.services != 1
            || package_manifest.build == 0
        {
            return Err(manifest("unsupported plugin manifest or protocol version"));
        }
        validate_id(&package_manifest.id)?;
        let identity = package_manifest.id.clone();
        let name = package_manifest.name.fallback();
        let version = package_manifest.version;
        let incompatible =
            super::host_version_mismatch(&self.host_version, &package_manifest.min_host_version)?;
        let backend = package_manifest.backend;
        let contributions = package_manifest.contributions;
        let capabilities = package_manifest.capabilities;
        if name.trim().is_empty() || version.trim().is_empty() {
            return Err(manifest("plugin name and version must not be empty"));
        }
        if let Some(entry) = &backend {
            relative_file(entry)?;
        }
        if backend.is_none()
            && (!contributions.tools.is_empty() || !contributions.commands.is_empty())
        {
            return Err(manifest("tools and commands require a backend"));
        }
        self.validate_source(&root, &identity, &source.source)?;
        validate_contributions(&root, &contributions)?;
        let installed = root.join(".pig-install.json");
        let content_digest = package_digest(&root)?;
        let digest = if installed.is_file() {
            read_json::<InstalledPluginMetadata>(&installed)?.digest
        } else {
            content_digest.clone()
        };
        Ok(PluginPackageInfo {
            id: identity,
            name,
            version,
            digest,
            content_digest,
            root,
            source: source.source.clone(),
            enabled: false,
            incompatible,
            backend,
            contributions,
            capabilities,
        })
    }

    fn validate_source(
        &self,
        root: &Path,
        id: &str,
        source: &PluginSource,
    ) -> Result<(), CoreError> {
        if let PluginSource::BuiltIn = source {
            if !BUILTIN_PACKAGES.contains(&id) {
                return Err(permission(format!("`{id}` is not a built-in plugin")));
            }
            let expected = canonical(&self.data_dir.join("plugins/builtin").join(id))?;
            if root != expected {
                return Err(permission(
                    "a built-in plugin must run from its materialized package",
                ));
            }
        }
        if let PluginSource::Project { workspace } = source {
            let workspace = canonical(workspace)?;
            if !root.starts_with(workspace.join(".pigcode/plugins")) {
                return Err(manifest("a project plugin must be inside .pigcode/plugins"));
            }
        }
        Ok(())
    }

    fn apply_enabled(&mut self) {
        for package in &mut self.snapshot.packages {
            let pending = self.pending_updates.get(&package.id) == Some(&package.digest);
            package.enabled = pending
                || match package.source {
                    PluginSource::BuiltIn => {
                        self.disabled_builtin.get(&package.id) != Some(&package.digest)
                    }
                    _ => self.enabled.get(&package.id) == Some(&package.digest),
                };
        }
    }

    fn persist(&self) {
        // `incompatible` is derived from the manifest and this host's version:
        // persisting it would keep a stale diagnostic after a host upgrade.
        let mut packages = self.snapshot.packages.clone();
        for package in &mut packages {
            package.incompatible = None;
        }
        let state = PersistedPlugins {
            packages,
            enabled: self.enabled.clone(),
            disabled_builtin: self.disabled_builtin.clone(),
            pending_updates: self.pending_updates.clone(),
            credential_grants: self.credential_grants.clone(),
        };
        match serde_json::to_vec_pretty(&state) {
            Ok(bytes) => {
                if let Err(error) = crate::config::write_private_file(
                    &self.data_dir.join(PLUGIN_STATE_FILE),
                    &bytes,
                ) {
                    tracing::warn!("could not save the plugin registry: {error:?}");
                }
            }
            Err(error) => tracing::warn!("could not serialize the plugin registry: {error}"),
        }
    }

    pub fn set_enabled(
        &mut self,
        id: &str,
        digest: &str,
        enabled: bool,
    ) -> Result<PluginSnapshot, CoreError> {
        let builtin = matches!(self.package(id, digest)?.source, PluginSource::BuiltIn);
        if enabled {
            let previous = self
                .snapshot
                .packages
                .iter()
                .find(|package| package.id == id && package.enabled && package.digest != digest)
                .map(|package| package.digest.clone());
            if previous.is_some() {
                self.pending_updates
                    .insert(id.to_owned(), digest.to_owned());
            } else if builtin {
                self.disabled_builtin.remove(id);
            } else {
                self.enabled.insert(id.to_owned(), digest.to_owned());
            }
        } else if self.pending_updates.get(id) == Some(&digest.to_owned()) {
            self.pending_updates.remove(id);
        } else {
            self.pending_updates.remove(id);
            if builtin {
                self.disabled_builtin
                    .insert(id.to_owned(), digest.to_owned());
            } else {
                self.enabled.remove(id);
            }
        }
        self.generation += 1;
        self.snapshot.generation = self.generation;
        self.apply_enabled();
        self.persist();
        Ok(self.snapshot.clone())
    }

    pub fn commit_update(&mut self, id: &str, digest: &str) -> Result<PluginSnapshot, CoreError> {
        let builtin = matches!(self.package(id, digest)?.source, PluginSource::BuiltIn);
        self.pending_updates.remove(id);
        if builtin {
            self.disabled_builtin.remove(id);
        } else {
            self.enabled.insert(id.to_owned(), digest.to_owned());
        }
        self.snapshot.runtime_contributions.retain(|key, _| {
            key == &format!("{id}:{digest}") || !key.starts_with(&format!("{id}:"))
        });
        self.runtime_epochs.retain(|key, _| {
            key == &format!("{id}:{digest}") || !key.starts_with(&format!("{id}:"))
        });
        self.generation += 1;
        self.snapshot.generation = self.generation;
        self.apply_enabled();
        self.persist();
        Ok(self.snapshot.clone())
    }

    pub fn cancel_update(&mut self, id: &str, digest: &str) -> Result<PluginSnapshot, CoreError> {
        self.package(id, digest)?;
        if self.pending_updates.get(id) == Some(&digest.to_owned()) {
            self.pending_updates.remove(id);
        }
        self.snapshot
            .runtime_contributions
            .remove(&format!("{id}:{digest}"));
        self.runtime_epochs.remove(&format!("{id}:{digest}"));
        self.generation += 1;
        self.snapshot.generation = self.generation;
        self.apply_enabled();
        self.persist();
        Ok(self.snapshot.clone())
    }

    pub fn set_credential_grant(
        &mut self,
        id: &str,
        digest: &str,
        credential: &str,
        origin: &str,
        allowed: bool,
        config: Option<&pig_protocol::AppConfig>,
    ) -> Result<PluginSnapshot, CoreError> {
        let method = self
            .package(id, digest)?
            .contributions
            .http
            .iter()
            .find(|candidate| candidate.id == origin)
            .and_then(|candidate| candidate.methods.first())
            .map(String::as_str)
            .ok_or_else(|| permission(format!("unknown HTTP origin `{origin}`")))?;
        let declaration =
            self.declared_credential_for_origin(id, digest, origin, method, credential, config)?;
        let pig_protocol::PluginCredentialDeclaration::Provider { provider_id, .. } = declaration
        else {
            return Err(permission(
                "an owned plugin credential needs no separate shared grant",
            ));
        };
        let grant = pig_protocol::PluginCredentialGrant {
            package: id.to_owned(),
            digest: digest.to_owned(),
            credential: credential.to_owned(),
            provider_id,
            origin: origin.to_owned(),
        };
        if allowed {
            self.credential_grants.insert(grant);
        } else {
            self.credential_grants.remove(&grant);
        }
        self.snapshot.credential_grants = self.credential_grants.iter().cloned().collect();
        self.generation += 1;
        self.snapshot.generation = self.generation;
        self.persist();
        Ok(self.snapshot.clone())
    }

    /// Whether a package still declares `entry` and may run it now. Used on
    /// every hook run so disabling, updating or uninstalling the package (or
    /// removing the hook through runtime contributions) revokes it at once.
    pub fn plugin_hook_active(
        &self,
        id: &str,
        digest: &str,
        entry: &pig_protocol::HookEntry,
    ) -> bool {
        let Ok(package) = self.package(id, digest) else {
            return false;
        };
        if !package.enabled || package.incompatible.is_some() {
            return false;
        }
        if package_digest(&package.root).ok().as_deref() != Some(&package.content_digest) {
            return false;
        }
        let runtime = self
            .snapshot
            .runtime_contributions
            .get(&format!("{id}:{digest}"));
        package
            .contributions
            .hooks
            .iter()
            .chain(runtime.into_iter().flat_map(|runtime| runtime.hooks.iter()))
            .any(|hook| hook.entry == *entry)
    }

    pub fn package(&self, id: &str, digest: &str) -> Result<&PluginPackageInfo, CoreError> {
        self.snapshot
            .packages
            .iter()
            .find(|package| package.id == id && package.digest == digest)
            .ok_or_else(|| permission("the plugin is unknown or its content digest changed"))
    }

    pub fn authorize_named(
        &self,
        id: &str,
        digest: &str,
        capability: &str,
        target: &str,
        config: Option<&pig_protocol::AppConfig>,
    ) -> Result<(), CoreError> {
        let package = self.package(id, digest)?;
        if package_digest(&package.root)? != package.content_digest {
            return Err(permission("the plugin files changed after discovery"));
        }
        if !package.enabled {
            return Err(permission("the plugin is disabled"));
        }
        ensure_compatible(package)?;
        if let PluginSource::Project { workspace } = &package.source {
            let trust = crate::config::WorkspaceTrust::for_config(config, workspace);
            if !trust.admits(workspace) {
                return Err(permission("the project workspace is not trusted"));
            }
        }
        let granted = package.capabilities.iter().any(|item| {
            item.name == capability
                && (item.targets.iter().any(|allowed| allowed == "*")
                    || item.targets.iter().any(|allowed| allowed == target))
        });
        if granted {
            Ok(())
        } else {
            Err(permission(format!(
                "{capability} is not granted for `{target}`"
            )))
        }
    }

    pub fn authorize(
        &self,
        id: &str,
        digest: &str,
        action: &PluginAction,
        config: Option<&pig_protocol::AppConfig>,
    ) -> Result<(), CoreError> {
        let capability = match action {
            PluginAction::OpenPage { page_id } => {
                if !pig_protocol::PLUGIN_HOST_NAVIGATION_TARGETS.contains(&page_id.as_str()) {
                    return Err(permission("unknown host navigation target"));
                }
                PluginCapability::Navigate
            }
            PluginAction::Refresh => PluginCapability::ReadSettings,
            PluginAction::CreateSkill { .. }
            | PluginAction::UpdateSkill { .. }
            | PluginAction::DeleteSkill { .. }
            | PluginAction::SetSkillEnabled { .. } => PluginCapability::ManageSkills,
            PluginAction::UpsertMcp { .. }
            | PluginAction::DeleteMcp { .. }
            | PluginAction::SetMcpEnabled { .. }
            | PluginAction::RefreshMcp => PluginCapability::ManageMcp,
            PluginAction::ReadConfig { .. } => PluginCapability::ReadConfig,
            PluginAction::WriteConfig { .. } => PluginCapability::WriteConfig,
        };
        self.authority(id, digest, config, capability).map(drop)
    }

    pub fn tool<'a>(
        &'a self,
        id: &str,
        digest: &str,
        name: &str,
        config: Option<&pig_protocol::AppConfig>,
    ) -> Result<&'a pig_protocol::PluginTool, CoreError> {
        self.authority(id, digest, config, PluginCapability::ProvideTools)?
            .contributions
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| permission(format!("plugin `{id}` does not contribute tool `{name}`")))
    }

    pub fn http_origin<'a>(
        &'a self,
        id: &str,
        digest: &str,
        origin_id: &str,
        method: &str,
        config: Option<&pig_protocol::AppConfig>,
    ) -> Result<&'a pig_protocol::PluginHttpOrigin, CoreError> {
        self.authority(id, digest, config, PluginCapability::Http)?
            .contributions
            .http
            .iter()
            .find(|origin| {
                origin.id == origin_id && origin.methods.iter().any(|allowed| allowed == method)
            })
            .ok_or_else(|| {
                permission(format!(
                    "plugin `{id}` does not allow {method} on origin `{origin_id}`"
                ))
            })
    }

    pub fn credential_allowed(
        &self,
        id: &str,
        digest: &str,
        credential: &str,
        config: Option<&pig_protocol::AppConfig>,
    ) -> Result<pig_protocol::PluginCredentialDeclaration, CoreError> {
        self.authority(id, digest, config, PluginCapability::Http)?
            .contributions
            .credentials
            .iter()
            .find(|declaration| declaration.id() == credential)
            .cloned()
            .ok_or_else(|| {
                permission(format!(
                    "plugin `{id}` does not declare credential `{credential}`"
                ))
            })
    }

    pub fn owned_credential(
        &self,
        id: &str,
        digest: &str,
        credential: &str,
        config: Option<&pig_protocol::AppConfig>,
    ) -> Result<(), CoreError> {
        let package = self.authority(id, digest, config, PluginCapability::ManageOwnSecrets)?;
        if package
            .contributions
            .credentials
            .iter()
            .any(|declaration| {
                matches!(declaration, pig_protocol::PluginCredentialDeclaration::Owned { id, .. } if id == credential)
            })
        {
            Ok(())
        } else {
            Err(permission(format!(
                "plugin `{id}` does not own credential `{credential}`"
            )))
        }
    }

    fn declared_credential_for_origin(
        &self,
        id: &str,
        digest: &str,
        origin: &str,
        method: &str,
        credential: &str,
        config: Option<&pig_protocol::AppConfig>,
    ) -> Result<pig_protocol::PluginCredentialDeclaration, CoreError> {
        let origin = self.http_origin(id, digest, origin, method, config)?;
        if !origin
            .credentials
            .iter()
            .any(|allowed| allowed == credential)
        {
            return Err(permission(format!(
                "credential `{credential}` is not declared for HTTP origin `{}`",
                origin.id
            )));
        }
        self.package(id, digest)?
            .contributions
            .credentials
            .iter()
            .find(|declaration| declaration.id() == credential)
            .cloned()
            .ok_or_else(|| {
                permission(format!(
                    "plugin `{id}` does not declare credential `{credential}`"
                ))
            })
    }

    pub fn credential_for_origin(
        &self,
        id: &str,
        digest: &str,
        origin: &str,
        method: &str,
        credential: &str,
        config: Option<&pig_protocol::AppConfig>,
    ) -> Result<pig_protocol::PluginCredentialDeclaration, CoreError> {
        let declaration =
            self.declared_credential_for_origin(id, digest, origin, method, credential, config)?;
        if let pig_protocol::PluginCredentialDeclaration::Provider { provider_id, .. } =
            &declaration
        {
            let grant = pig_protocol::PluginCredentialGrant {
                package: id.to_owned(),
                digest: digest.to_owned(),
                credential: credential.to_owned(),
                provider_id: provider_id.clone(),
                origin: origin.to_owned(),
            };
            if !self.credential_grants.contains(&grant) {
                return Err(permission(format!(
                    "plugin `{id}` is not authorized to use provider credential `{provider_id}` on origin `{origin}`"
                )));
            }
        }
        Ok(declaration)
    }

    fn authority<'a>(
        &'a self,
        id: &str,
        digest: &str,
        config: Option<&pig_protocol::AppConfig>,
        capability: PluginCapability,
    ) -> Result<&'a PluginPackageInfo, CoreError> {
        let package = self.package(id, digest)?;
        if package_digest(&package.root)? != package.content_digest {
            return Err(permission(
                "the plugin files changed after discovery; reload it first",
            ));
        }
        if !package.enabled {
            return Err(permission("the plugin is disabled"));
        }
        ensure_compatible(package)?;
        if let PluginSource::Project { workspace } = &package.source {
            let trust = crate::config::WorkspaceTrust::for_config(config, workspace);
            if !trust.admits(workspace) {
                return Err(permission(
                    "the workspace containing this plugin is not trusted",
                ));
            }
        }
        if !package.contributions.actions.contains(&capability) {
            return Err(permission(format!(
                "plugin `{id}` did not declare the required {capability:?} capability"
            )));
        }
        Ok(package)
    }
}

fn ensure_compatible(package: &PluginPackageInfo) -> Result<(), CoreError> {
    match &package.incompatible {
        Some(detail) => Err(permission(format!(
            "plugin `{}` is not active: {detail}",
            package.id
        ))),
        None => Ok(()),
    }
}

pub(crate) fn authorized_package(
    data_dir: &Path,
    package: &PluginPackageInfo,
    trusted_workspace: Option<&crate::config::WorkspaceTrust>,
) -> bool {
    if !package.enabled
        || package.incompatible.is_some()
        || package_digest(&package.root).ok().as_deref() != Some(&package.content_digest)
    {
        return false;
    }
    match &package.source {
        PluginSource::BuiltIn => {
            let builtin_root = data_dir.join("plugins/builtin");
            let builtin_root = canonical(&builtin_root).unwrap_or(builtin_root);
            BUILTIN_PACKAGES.contains(&package.id.as_str())
                && package.root.starts_with(builtin_root)
        }
        PluginSource::User => true,
        PluginSource::Project { workspace } => {
            trusted_workspace.is_some_and(|trust| trust.admits(workspace))
        }
    }
}

fn validate_contributions(
    root: &Path,
    contributions: &PluginContributions,
) -> Result<(), CoreError> {
    if contributions.api_version != PLUGIN_API_VERSION {
        return Err(manifest(format!(
            "unsupported plugin API {}; expected {PLUGIN_API_VERSION}",
            contributions.api_version
        )));
    }
    if !contributions.themes.is_empty()
        && !contributions
            .actions
            .contains(&PluginCapability::ProvideThemes)
    {
        return Err(permission(
            "theme contributions require the provide_themes capability",
        ));
    }
    let mut theme_ids = BTreeSet::new();
    for theme in &contributions.themes {
        if !theme_ids.insert(theme.id.clone()) {
            return Err(manifest(format!("duplicate theme id `{}`", theme.id)));
        }
        super::themes::validate_theme(root, theme)?;
    }
    if !contributions.mcp.is_empty()
        && !contributions
            .actions
            .contains(&PluginCapability::ProvideMcp)
    {
        return Err(permission(
            "MCP contributions require the provide_mcp capability",
        ));
    }
    let mut server_names = BTreeSet::new();
    for server in &contributions.mcp {
        if server.name.trim().is_empty() || !server_names.insert(&server.name) {
            return Err(manifest(
                "MCP names must be non-empty and unique per package",
            ));
        }
        crate::mcp::validate_server(&server.config).map_err(|error| manifest(error.detail()))?;
    }
    if !contributions.tools.is_empty()
        && !contributions
            .actions
            .contains(&PluginCapability::ProvideTools)
    {
        return Err(permission(
            "tool contributions require the provide_tools capability",
        ));
    }
    if !contributions.hooks.is_empty()
        && !contributions
            .actions
            .contains(&PluginCapability::ProvideHooks)
    {
        return Err(permission(
            "hook contributions require the provide_hooks capability",
        ));
    }
    let mut hooks = BTreeSet::new();
    for hook in &contributions.hooks {
        if !crate::hooks::valid_hook_entry(&hook.entry) {
            return Err(manifest(format!(
                "plugin hook for {} must not select a shell for a process entry",
                hook.event.name()
            )));
        }
        if hook.entry.command.trim().is_empty() {
            return Err(manifest("plugin hooks need a command"));
        }
        if hook.entry.timeout_ms.is_some_and(|timeout| timeout == 0) {
            return Err(manifest("plugin hook timeouts must be positive"));
        }
        if let Some(matcher) = hook
            .entry
            .matcher
            .as_deref()
            .map(str::trim)
            .filter(|matcher| !matcher.is_empty() && *matcher != "*")
        {
            let plain = matcher
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|');
            if !plain && regex::Regex::new(matcher).is_err() {
                return Err(manifest(format!(
                    "plugin hook matcher `{matcher}` is not a valid regex"
                )));
            }
        }
        let key = (
            hook.event,
            hook.entry.command.clone(),
            hook.entry.args.join("\u{0}"),
            hook.entry.matcher.clone().unwrap_or_default(),
        );
        if !hooks.insert(key) {
            return Err(manifest(format!(
                "duplicate plugin hook for {}",
                hook.event.name()
            )));
        }
    }
    if !contributions.http.is_empty() && !contributions.actions.contains(&PluginCapability::Http) {
        return Err(permission("HTTP contributions require the http capability"));
    }
    let mut tool_names = BTreeSet::new();
    for tool in &contributions.tools {
        if tool.name.len() > 64
            || !tool
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            || !tool
                .name
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic())
        {
            return Err(manifest(format!(
                "invalid plugin tool name `{}`",
                tool.name
            )));
        }
        if tool.name.starts_with("mcp__") {
            return Err(manifest(
                "plugin tool names may not use the reserved mcp__ prefix",
            ));
        }
        if !tool_names.insert(&tool.name) {
            return Err(manifest(format!("duplicate tool name `{}`", tool.name)));
        }
        if tool.description.trim().is_empty() || !tool.parameters.is_object() {
            return Err(manifest(format!(
                "tool `{}` needs a description and an object JSON Schema",
                tool.name
            )));
        }
        if tool
            .output_schema
            .as_ref()
            .is_some_and(|schema| !schema.is_object())
        {
            return Err(manifest(format!(
                "tool `{}` output schema must be an object JSON Schema",
                tool.name
            )));
        }
        super::schema::check_supported(&tool.parameters)
            .map_err(|error| manifest(format!("tool `{}` parameters: {error}", tool.name)))?;
        if let Some(output) = &tool.output_schema {
            super::schema::check_supported(output).map_err(|error| {
                manifest(format!("tool `{}` output schema: {error}", tool.name))
            })?;
        }
        if tool.approval == pig_protocol::PluginToolApproval::Never && !tool.read_only {
            return Err(manifest(format!(
                "tool `{}` may waive approval only when it is read-only",
                tool.name
            )));
        }
        if tool.timeout_ms.is_some_and(|timeout| {
            timeout == 0 || timeout > pig_protocol::PLUGIN_TOOL_MAX_TIMEOUT_MS
        }) {
            return Err(manifest(format!(
                "tool `{}` timeout must be between 1 and {} ms",
                tool.name,
                pig_protocol::PLUGIN_TOOL_MAX_TIMEOUT_MS
            )));
        }
        if tool.max_concurrency.is_some_and(|limit| {
            limit == 0 || limit as usize > pig_protocol::PLUGIN_TOOL_MAX_CONCURRENCY
        }) {
            return Err(manifest(format!(
                "tool `{}` concurrency must be between 1 and {}",
                tool.name,
                pig_protocol::PLUGIN_TOOL_MAX_CONCURRENCY
            )));
        }
        if tool.max_output_bytes.is_some_and(|bytes| {
            bytes < pig_protocol::PLUGIN_TOOL_MIN_OUTPUT_BYTES
                || bytes > pig_protocol::PLUGIN_TOOL_MAX_OUTPUT_BYTES
        }) {
            return Err(manifest(format!(
                "tool `{}` output budget must be between {} and {} bytes",
                tool.name,
                pig_protocol::PLUGIN_TOOL_MIN_OUTPUT_BYTES,
                pig_protocol::PLUGIN_TOOL_MAX_OUTPUT_BYTES
            )));
        }
    }
    let mut credential_ids = BTreeSet::new();
    for declaration in &contributions.credentials {
        validate_id(declaration.id())?;
        if !credential_ids.insert(declaration.id()) {
            return Err(manifest(format!(
                "duplicate credential id `{}`",
                declaration.id()
            )));
        }
        match declaration {
            pig_protocol::PluginCredentialDeclaration::Owned { label, .. } => {
                if label.trim().is_empty() {
                    return Err(manifest(format!(
                        "owned credential `{}` needs a label",
                        declaration.id()
                    )));
                }
            }
            pig_protocol::PluginCredentialDeclaration::Provider { provider_id, .. } => {
                if provider_id.trim().is_empty() {
                    return Err(manifest(format!(
                        "provider credential `{}` needs a provider id",
                        declaration.id()
                    )));
                }
            }
        }
    }
    let mut origins = BTreeSet::new();
    for origin in &contributions.http {
        validate_id(&origin.id)?;
        if !origins.insert(&origin.id) {
            return Err(manifest(format!("duplicate HTTP origin `{}`", origin.id)));
        }
        let url = reqwest::Url::parse(&origin.url)
            .map_err(|error| manifest(format!("invalid HTTP origin URL: {error}")))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(manifest(
                "HTTP origins must be HTTP(S) URLs without credentials or fragments",
            ));
        }
        if origin.methods.is_empty()
            || origin
                .methods
                .iter()
                .any(|method| !matches!(method.as_str(), "GET" | "POST"))
        {
            return Err(manifest(
                "HTTP origins must allow only explicit GET or POST methods",
            ));
        }
        if origin
            .credentials
            .iter()
            .any(|credential| !credential_ids.contains(credential.as_str()))
        {
            return Err(manifest(format!(
                "HTTP origin `{}` references an undeclared credential",
                origin.id
            )));
        }
    }
    for skill in &contributions.skills {
        let path = safe_relative(&skill.path)?;
        if !root
            .join(path)
            .join(pig_utils::skills::SKILL_FILE)
            .is_file()
        {
            return Err(manifest(format!("skill `{}` has no SKILL.md", skill.path)));
        }
    }
    let mut command_ids = BTreeSet::new();
    for command in &contributions.commands {
        validate_id(&command.id)?;
        if !command_ids.insert(&command.id) || command.title.is_empty() {
            return Err(manifest(format!(
                "duplicate or untitled command `{}`",
                command.id
            )));
        }
    }
    super::config::validate_declaration(contributions)?;
    Ok(())
}

pub fn package_digest(root: &Path) -> Result<String, CoreError> {
    let root = canonical(root)?;
    let mut files = Vec::new();
    collect_files(&root, &root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hash = Sha256::new();
    for (name, path) in files {
        hash.update(name.as_bytes());
        hash.update([0]);
        let bytes = std::fs::read(&path).map_err(io)?;
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

fn collect_files(
    root: &Path,
    directory: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), CoreError> {
    let mut entries = std::fs::read_dir(directory)
        .map_err(io)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | "node_modules" | GENERATED_TYPES) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(io)?;
        if metadata.file_type().is_symlink() {
            return Err(manifest(format!(
                "plugin packages may not contain symbolic links: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_files(root, &path, out)?;
        } else if metadata.is_file() {
            if metadata.len() > 16 * 1024 * 1024 || out.len() >= 10_000 {
                return Err(manifest(
                    "plugin package exceeds the file count or size limit",
                ));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|error| manifest(error.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, path));
        } else {
            return Err(manifest(
                "plugin packages may contain only regular files and directories",
            ));
        }
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, CoreError> {
    let bytes =
        std::fs::read(path).map_err(|error| manifest(format!("{}: {error}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|error| manifest(format!("{}: {error}", path.display())))
}

pub(crate) fn validate_id(id: &str) -> Result<(), CoreError> {
    if id.is_empty()
        || id.starts_with(['.', '-', '_'])
        || id.ends_with(['.', '-', '_'])
        || id.contains("..")
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(manifest(format!("invalid plugin contribution id `{id}`")));
    }
    Ok(())
}

pub(crate) fn relative_file(path: &str) -> Result<PathBuf, CoreError> {
    safe_relative(path)
}

fn safe_relative(path: &str) -> Result<PathBuf, CoreError> {
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(manifest(
            "plugin paths must be relative paths inside the package",
        ));
    }
    Ok(path)
}

pub(crate) fn canonical(path: &Path) -> Result<PathBuf, CoreError> {
    let path = path
        .canonicalize()
        .map_err(|error| manifest(format!("{}: {error}", path.display())))?;
    #[cfg(windows)]
    let path = dunce::simplified(&path).to_path_buf();
    Ok(path)
}

fn manifest(detail: impl Into<String>) -> CoreError {
    CoreError::PluginManifest {
        detail: detail.into(),
    }
}

fn permission(detail: impl Into<String>) -> CoreError {
    CoreError::PluginPermission {
        detail: detail.into(),
    }
}

fn io(error: std::io::Error) -> CoreError {
    manifest(error.to_string())
}
