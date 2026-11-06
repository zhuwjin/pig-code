use pig_protocol::{CoreError, PluginAction, PluginActionResult, PluginPackageSource, PluginScope};

use super::{Manager, emit};
use crate::plugins::{self, SessionServices};

impl Manager {
    fn emit_plugin_snapshot(
        &self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        snapshot: pig_protocol::PluginSnapshot,
    ) {
        let credentials = self.credentials.clone();
        let ids: Vec<_> = snapshot
            .packages
            .iter()
            .flat_map(|package| {
                package
                    .contributions
                    .credentials
                    .iter()
                    .filter_map(|declaration| match declaration {
                        pig_protocol::PluginCredentialDeclaration::Owned { id, .. } => {
                            Some(crate::credentials::CredentialId::plugin(&package.id, id))
                        }
                        pig_protocol::PluginCredentialDeclaration::Provider { .. } => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        tokio::spawn(async move {
            for id in ids {
                let _ = credentials.get(id).await;
            }
            emit(&event_tx, pig_protocol::Event::PluginSnapshot { snapshot });
        });
    }

    pub(super) fn sync_plugins(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        sources: Vec<PluginPackageSource>,
    ) {
        let snapshot = self
            .plugins
            .lock()
            .expect("plugin registry lock")
            .sync(sources);
        self.refresh_session_plugin_surfaces();
        self.emit_plugin_snapshot(event_tx, snapshot);
    }

    /// Re-resolve every session's plugin-provided surfaces after the registry
    /// changed. A session owned by a turn is marked stale and refreshed at its
    /// wrap-up (dropping a live MCP manager mid-turn would break the running
    /// child's tool calls).
    fn refresh_session_plugin_surfaces(&mut self) {
        for entry in self.sessions.values_mut() {
            if entry.session.is_some() {
                if let Some(session) = entry.session.as_mut() {
                    session.refresh_plugin_surfaces();
                }
                entry.mcp_status = None;
            } else {
                entry.plugin_surfaces_stale = true;
            }
        }
    }

    pub(super) fn sync_plugin_runtime_contributions(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        request: pig_protocol::PluginRuntimeContributionRequest,
    ) {
        let pig_protocol::PluginRuntimeContributionRequest {
            request_id,
            package,
            digest,
            epoch,
            generation,
            contributions,
        } = request;
        let result = self
            .plugins
            .lock()
            .expect("plugin registry lock")
            .sync_runtime_contributions(&package, &digest, epoch, generation, contributions);
        match result {
            Ok(snapshot) => {
                self.refresh_session_plugin_surfaces();
                self.emit_plugin_snapshot(event_tx.clone(), snapshot.clone());
                emit(
                    &event_tx,
                    pig_protocol::Event::PluginRuntimeContributionCommitted {
                        request_id,
                        epoch,
                        snapshot,
                    },
                );
            }
            Err(error) => {
                self.plugin_action_result(&event_tx, request_id, package, digest, Err(error), false)
            }
        }
    }

    pub(super) fn refresh_plugin_marketplace(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
    ) {
        tokio::spawn(async move {
            let result = plugins::fetch_catalog(plugins::DEFAULT_MARKETPLACE_INDEX_URL).await;
            emit(
                &event_tx,
                pig_protocol::Event::PluginMarketplaceCatalog { result },
            );
        });
    }

    pub(super) fn install_marketplace_plugin(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        package: pig_protocol::PluginMarketplacePackage,
    ) {
        let data_dir = self.data_dir.clone();
        tokio::spawn(async move {
            let result = plugins::download_signed_package(
                &data_dir,
                plugins::DEFAULT_MARKETPLACE_INDEX_URL,
                &package,
            )
            .await
            .map(|download| pig_protocol::PluginMarketplaceInstall {
                package: download.package,
                digest: download.digest,
            });
            emit(
                &event_tx,
                pig_protocol::Event::PluginMarketplaceInstall { result },
            );
        });
    }

    pub(super) fn set_plugin_enabled(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        package: String,
        digest: String,
        enabled: bool,
    ) {
        let result = self
            .plugins
            .lock()
            .expect("plugin registry lock")
            .set_enabled(&package, &digest, enabled);
        match result {
            Ok(snapshot) => {
                self.refresh_session_plugin_surfaces();
                self.emit_plugin_snapshot(event_tx, snapshot)
            }
            Err(error) => self.plugin_action_result(
                &event_tx,
                String::new(),
                package,
                digest,
                Err(error),
                false,
            ),
        }
    }

    pub(super) fn finish_plugin_update(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        package: String,
        digest: String,
        commit: bool,
    ) {
        let result = if commit {
            self.plugins
                .lock()
                .expect("plugin registry lock")
                .commit_update(&package, &digest)
        } else {
            self.plugins
                .lock()
                .expect("plugin registry lock")
                .cancel_update(&package, &digest)
        };
        match result {
            Ok(snapshot) => {
                self.refresh_session_plugin_surfaces();
                self.emit_plugin_snapshot(event_tx, snapshot)
            }
            Err(error) => self.plugin_action_result(
                &event_tx,
                String::new(),
                package,
                digest,
                Err(error),
                false,
            ),
        }
    }

    pub(super) fn query_plugin(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        request_id: String,
        package: String,
        digest: String,
        scope: PluginScope,
        session_id: Option<String>,
    ) {
        let (capability, target) = match (&scope, &session_id) {
            (_, Some(session_id)) => ("session.read", session_id.clone()),
            (PluginScope::Project { workspace }, None) => {
                ("workspace.read", workspace.to_string_lossy().into_owned())
            }
            (PluginScope::User, None) => ("workspace.read", "*".to_owned()),
        };
        let authorized = self
            .plugins
            .lock()
            .expect("plugin registry lock")
            .authorize_named(&package, &digest, capability, &target, self.config.as_ref());
        let session_cwd = session_id
            .as_deref()
            .and_then(|id| self.sessions.get(id).map(|entry| entry.cwd.clone()));
        let result = authorized.and_then(|()| {
            if !session_scope_permitted(&scope, session_cwd.as_deref()) {
                return Err(CoreError::PluginPermission {
                    detail: "a project plugin cannot read a session from another workspace".into(),
                });
            }
            self.plugin_services(scope, session_id)
        });
        emit(
            &event_tx,
            pig_protocol::Event::PluginServices {
                request_id,
                package,
                digest,
                result,
            },
        );
    }

    pub(super) fn plugin_action(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        request_id: String,
        package: String,
        digest: String,
        action: PluginAction,
        session_id: Option<String>,
    ) {
        let refresh_views = matches!(action, PluginAction::WriteConfig { .. });
        let authorized = self
            .plugins
            .lock()
            .expect("plugin registry lock")
            .authorize(&package, &digest, &action, self.config.as_ref());
        let result = authorized
            .and_then(|()| self.execute_plugin_action(&package, action, session_id, &event_tx));
        let refresh_views = refresh_views && result.is_ok();
        self.plugin_action_result(
            &event_tx,
            request_id,
            package,
            digest,
            result,
            refresh_views,
        );
    }

    fn execute_plugin_action(
        &mut self,
        package: &str,
        action: PluginAction,
        session_id: Option<String>,
        event_tx: &async_channel::Sender<pig_protocol::Event>,
    ) -> Result<serde_json::Value, CoreError> {
        match action {
            PluginAction::OpenPage { page_id } => Ok(serde_json::json!({ "page_id": page_id })),
            PluginAction::Refresh => self
                .plugin_services(PluginScope::User, session_id)
                .and_then(json),
            PluginAction::CreateSkill {
                scope,
                dir_name,
                markdown,
            } => plugins::save_skill(&self.data_dir, &scope, &dir_name, &markdown, true)
                .map(|()| serde_json::Value::Null),
            PluginAction::UpdateSkill {
                scope,
                dir_name,
                markdown,
            } => plugins::save_skill(&self.data_dir, &scope, &dir_name, &markdown, false)
                .map(|()| serde_json::Value::Null),
            PluginAction::DeleteSkill { scope, dir_name } => {
                plugins::delete_skill(&self.data_dir, &scope, &dir_name)
                    .map(|()| serde_json::Value::Null)
            }
            PluginAction::SetSkillEnabled { path, enabled } => {
                let mut workspaces: Vec<_> = self
                    .store
                    .lock()
                    .expect("store lock")
                    .workspaces()
                    .into_iter()
                    .map(|workspace| workspace.path)
                    .collect();
                workspaces.extend(self.sessions.values().map(|entry| entry.cwd.clone()));
                plugins::set_skill_enabled(
                    &self.data_dir,
                    &self.plugins,
                    &path,
                    enabled,
                    &workspaces,
                    self.config.as_ref(),
                )
                .map(|()| serde_json::Value::Null)
            }
            PluginAction::UpsertMcp {
                scope,
                name,
                config,
            } => plugins::upsert_mcp(&self.data_dir, &scope, &name, config)
                .map(|()| serde_json::Value::Null),
            PluginAction::DeleteMcp { scope, name } => {
                plugins::delete_mcp(&self.data_dir, &scope, &name).map(|()| serde_json::Value::Null)
            }
            PluginAction::SetMcpEnabled {
                scope,
                name,
                disabled,
            } => plugins::set_mcp_enabled(&self.data_dir, &scope, &name, disabled)
                .map(|()| serde_json::Value::Null),
            PluginAction::RefreshMcp => self
                .session_services(session_id)
                .map(|session| session.statuses)
                .ok_or(CoreError::SessionNotFound)
                .and_then(json),
            PluginAction::ReadConfig { path } => {
                let workspace = session_id
                    .as_deref()
                    .and_then(|id| self.sessions.get(id))
                    .map(|entry| entry.cwd.clone());
                plugins::config::read_with_workspace(
                    &self.plugins.lock().expect("plugin registry lock"),
                    self.config.as_ref(),
                    package,
                    &path,
                    workspace.as_deref(),
                )
            }
            PluginAction::WriteConfig { path, value } => {
                let workspace = session_id
                    .as_deref()
                    .and_then(|id| self.sessions.get(id))
                    .map(|entry| entry.cwd.clone());
                let write = {
                    let registry = self.plugins.lock().expect("plugin registry lock");
                    plugins::config::write_with_workspace(
                        &registry,
                        self.config.as_ref(),
                        package,
                        &path,
                        value,
                        workspace.as_deref(),
                    )?
                };
                match write {
                    plugins::config::ConfigWrite::Plugin(value) => Ok(value),
                    plugins::config::ConfigWrite::App { config, value } => {
                        self.save_config_result(event_tx.clone(), *config)?;
                        Ok(value)
                    }
                }
            }
        }
    }

    fn plugin_services(
        &mut self,
        scope: PluginScope,
        session_id: Option<String>,
    ) -> Result<pig_protocol::PluginServiceSnapshot, CoreError> {
        let session = self.session_services(session_id);
        let workspace = match &scope {
            PluginScope::User => None,
            PluginScope::Project { workspace } => Some(workspace.clone()),
        };
        let trust = workspace
            .as_ref()
            .map(|workspace| {
                crate::config::WorkspaceTrust::for_config(self.config.as_ref(), workspace)
            })
            .or_else(|| {
                session.as_ref().map(|session| {
                    crate::config::WorkspaceTrust::for_config(self.config.as_ref(), &session.cwd)
                })
            })
            .unwrap_or_else(|| crate::config::WorkspaceTrust::untrusted(&self.default_cwd));
        let workspaces = self.store.lock().expect("store lock").workspaces();
        Ok(plugins::snapshot(
            &self.plugins.lock().expect("plugin registry lock"),
            scope,
            workspaces,
            session,
            &trust,
        ))
    }

    pub(super) fn plugin_http_request(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        request_id: String,
        package: String,
        digest: String,
        request: pig_protocol::PluginHttpRequest,
    ) {
        let plugin_registry = self.plugins.clone();
        let config = self.config.clone();
        let credentials = self.credentials.clone();
        tokio::spawn(async move {
            let result = plugins::request_http(
                &plugin_registry,
                config.as_ref(),
                &package,
                &digest,
                request,
                credentials,
            )
            .await;
            emit(
                &event_tx,
                pig_protocol::Event::PluginHttpResponse { request_id, result },
            );
        });
    }

    pub(super) fn set_plugin_secret(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        request_id: String,
        package: String,
        digest: String,
        credential: String,
        secret: pig_protocol::SecretString,
    ) {
        if let Err(error) = self
            .plugins
            .lock()
            .expect("plugin registry lock")
            .owned_credential(&package, &digest, &credential, self.config.as_ref())
        {
            emit(
                &event_tx,
                pig_protocol::Event::PluginSecretResult {
                    request_id,
                    result: Err(error),
                },
            );
            return;
        }
        let id = crate::credentials::CredentialId::plugin(&package, &credential);
        let credentials = self.credentials.clone();
        tokio::spawn(async move {
            let result = credentials
                .set(id.clone(), secret.into_secret())
                .await
                .map(|_| ())
                .map_err(|error| error.into_core(&id));
            emit(
                &event_tx,
                pig_protocol::Event::PluginSecretResult { request_id, result },
            );
        });
    }

    pub(super) fn clear_plugin_secret(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        request_id: String,
        package: String,
        digest: String,
        credential: String,
    ) {
        if let Err(error) = self
            .plugins
            .lock()
            .expect("plugin registry lock")
            .owned_credential(&package, &digest, &credential, self.config.as_ref())
        {
            emit(
                &event_tx,
                pig_protocol::Event::PluginSecretResult {
                    request_id,
                    result: Err(error),
                },
            );
            return;
        }
        let id = crate::credentials::CredentialId::plugin(&package, &credential);
        let credentials = self.credentials.clone();
        tokio::spawn(async move {
            let result = credentials
                .delete(id.clone())
                .await
                .map_err(|error| error.into_core(&id));
            emit(
                &event_tx,
                pig_protocol::Event::PluginSecretResult { request_id, result },
            );
        });
    }

    pub(super) fn set_plugin_credential_grant(
        &mut self,
        event_tx: async_channel::Sender<pig_protocol::Event>,
        package: String,
        digest: String,
        credential: String,
        origin: String,
        allowed: bool,
    ) {
        match self
            .plugins
            .lock()
            .expect("plugin registry lock")
            .set_credential_grant(
                &package,
                &digest,
                &credential,
                &origin,
                allowed,
                self.config.as_ref(),
            ) {
            Ok(snapshot) => self.emit_plugin_snapshot(event_tx, snapshot),
            Err(error) => self.plugin_action_result(
                &event_tx,
                String::new(),
                package,
                digest,
                Err(error),
                false,
            ),
        }
    }

    pub(super) fn plugin_tool_result(&mut self, result: pig_protocol::PluginToolResult) {
        if let Some(entry) = self.sessions.get(&result.session_id) {
            entry.plugin_dispatcher.resolve(result);
        } else {
            tracing::warn!("ignored a plugin result for an unknown session");
        }
    }

    pub(super) fn plugin_tools_selection(&mut self, selection: pig_protocol::PluginToolsSelection) {
        if let Some(entry) = self.sessions.get(&selection.session_id) {
            entry.plugin_dispatcher.resolve_selection(selection);
        } else {
            tracing::warn!("ignored a plugin tool selection for an unknown session");
        }
    }

    fn session_services(&mut self, session_id: Option<String>) -> Option<SessionServices> {
        let id = session_id?;
        let entry = self.sessions.get_mut(&id)?;
        if let Some(statuses) = entry
            .session
            .as_ref()
            .and_then(|session| session.mcp.as_ref())
            .map(|mcp| mcp.statuses())
        {
            entry.mcp_status = Some(statuses);
        }
        let (stats, turns) = entry
            .session
            .as_ref()
            .map(|session| session.plugin_stats())
            .unwrap_or_default();
        Some(SessionServices {
            id,
            cwd: entry.cwd.clone(),
            statuses: entry.mcp_status.clone(),
            stats,
            turns,
        })
    }

    fn plugin_action_result(
        &self,
        event_tx: &async_channel::Sender<pig_protocol::Event>,
        request_id: String,
        package: String,
        digest: String,
        result: Result<serde_json::Value, CoreError>,
        refresh_views: bool,
    ) {
        emit(
            event_tx,
            pig_protocol::Event::PluginActionResult {
                result: PluginActionResult {
                    request_id,
                    package,
                    digest,
                    result,
                    refresh_views,
                },
            },
        );
    }
}

fn json(value: impl serde::Serialize) -> Result<serde_json::Value, CoreError> {
    serde_json::to_value(value).map_err(|error| CoreError::Internal {
        detail: error.to_string(),
    })
}

fn session_scope_permitted(scope: &PluginScope, session_cwd: Option<&std::path::Path>) -> bool {
    match scope {
        PluginScope::User => true,
        PluginScope::Project { workspace } => {
            session_cwd.is_none_or(|cwd| cwd.starts_with(workspace))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use pig_protocol::PluginScope;

    use super::session_scope_permitted;

    #[test]
    fn project_session_reads_stay_inside_their_workspace() {
        let scope = PluginScope::Project {
            workspace: PathBuf::from("C:/workspace"),
        };
        assert!(session_scope_permitted(
            &scope,
            Some(Path::new("C:/workspace"))
        ));
        assert!(session_scope_permitted(
            &scope,
            Some(Path::new("C:/workspace/nested"))
        ));
        assert!(!session_scope_permitted(
            &scope,
            Some(Path::new("C:/workspace-other"))
        ));
        assert!(session_scope_permitted(
            &PluginScope::User,
            Some(Path::new("C:/workspace-other"))
        ));
    }
}
