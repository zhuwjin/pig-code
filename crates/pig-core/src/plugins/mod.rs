pub(crate) mod config;
mod contributions;
mod http;
mod marketplace;
mod registry;
mod services;
mod themes;
mod tools;

pub use http::request_http;
pub use marketplace::{
    DEFAULT_MARKETPLACE_INDEX_URL, download_signed_package, fetch_catalog, fetch_root_public_key,
};
pub use themes::load_theme;
pub use tools::{PluginToolDispatcher, PluginToolFilter, PluginToolset};

pub use contributions::{
    discover_skills_with_trust, enabled_packages, load_skill_output_with_trust, mcp_servers,
    skills_section_with_trust, skills_state,
};
use registry::authorized_package;
pub use registry::package_digest;
pub use services::{
    SessionServices, delete_mcp, delete_skill, save_skill, set_mcp_enabled, set_skill_enabled,
    snapshot, upsert_mcp,
};

use pig_protocol::{PluginSnapshot, PluginSource};

pub(crate) const PLUGIN_STATE_FILE: &str = "plugins-state.json";
pub(crate) const REDACTED: &str = "${PIG_REDACTED}";
const GENERATED_TYPES: &str = "gpui-kit.d.ts";
const BUILTIN_PACKAGES: [&str; 1] = ["pig.websearch"];

pub type SharedPluginRegistry = std::sync::Arc<std::sync::Mutex<PluginRegistry>>;

pub struct PluginRegistry {
    pub(crate) data_dir: std::path::PathBuf,
    pub(crate) generation: u64,
    pub(crate) snapshot: PluginSnapshot,
    enabled: std::collections::BTreeMap<String, String>,
    disabled_builtin: std::collections::BTreeMap<String, String>,
    pending_updates: std::collections::BTreeMap<String, String>,
    runtime_epochs: std::collections::BTreeMap<String, u64>,
    credential_grants: std::collections::BTreeSet<pig_protocol::PluginCredentialGrant>,
}

pub fn is_builtin(id: &str, source: &PluginSource) -> bool {
    matches!(source, PluginSource::BuiltIn) && BUILTIN_PACKAGES.contains(&id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::{PluginAction, PluginPackageSource, PluginScope, PluginSource};
    use std::path::{Path, PathBuf};

    struct TempDir(PathBuf);

    static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "pig-plugin-core-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos(),
                NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("temp directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        std::fs::write(path, contents).expect("fixture");
    }

    fn user_plugin(root: &Path) {
        write(
            &root.join("pig-plugin.json"),
            r#"{
              "apiVersion": 1,
              "id": "demo",
              "name": "Demo",
              "version": "1.0.0",
              "build": 1,
              "entry": "App.tsx",
              "minHostVersion": "1.0.0",
              "protocols": {"ui": 1, "services": 1},
              "capabilities": [],
              "contributions": {
                "actions":["read_settings"],
                "pages":[{"id":"home","entry":"App.tsx","title":{"en":"Home"}}]
              }
            }"#,
        );
        write(
            &root.join("App.tsx"),
            "export default function App() { return null; }",
        );
    }

    fn extend_user_plugin(root: &Path, contributions: &str) {
        let path = root.join("pig-plugin.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read plugin manifest"))
                .expect("plugin manifest");
        let contributions: serde_json::Value =
            serde_json::from_str(contributions).expect("additional contributions");
        manifest["contributions"]
            .as_object_mut()
            .expect("contributions object")
            .extend(
                contributions
                    .as_object()
                    .expect("contributions object")
                    .clone(),
            );
        write(
            &path,
            &serde_json::to_string_pretty(&manifest).expect("manifest JSON"),
        );
    }

    fn sync(registry: &mut PluginRegistry, root: &Path) -> PluginSnapshot {
        let snapshot = registry.sync(vec![PluginPackageSource {
            root: root.to_path_buf(),
            source: PluginSource::User,
        }]);
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        snapshot
    }

    #[test]
    fn updates_invalidate_enabled_state_and_action_authority() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        let mut registry = PluginRegistry::new(temp.0.join("data"));
        let snapshot = sync(&mut registry, &root);
        let package = &snapshot.packages[0];
        assert!(!package.enabled);
        registry
            .set_enabled(&package.id, &package.digest, true)
            .expect("enable exact package");
        registry
            .authorize(&package.id, &package.digest, &PluginAction::Refresh, None)
            .expect("current package is authorized");

        write(&root.join("main.js"), "export default class Changed {}");
        let updated = sync(&mut registry, &root);
        let changed = &updated.packages[0];
        assert_ne!(changed.digest, package.digest);
        assert!(!changed.enabled, "a new digest needs a new enable decision");
        let error = registry
            .authorize(&package.id, &package.digest, &PluginAction::Refresh, None)
            .expect_err("the old digest must not authorize new code");
        assert!(matches!(
            error,
            pig_protocol::CoreError::PluginPermission { .. }
        ));
    }

    #[test]
    fn registry_reloads_the_exact_enabled_digest() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        let data = temp.0.join("data");
        let mut registry = PluginRegistry::new(data.clone());
        let snapshot = sync(&mut registry, &root);
        let package = &snapshot.packages[0];
        registry
            .set_enabled(&package.id, &package.digest, true)
            .unwrap();
        let restored = PluginRegistry::new(data);
        assert!(restored.snapshot.packages[0].enabled);
        restored
            .authorize(&package.id, &package.digest, &PluginAction::Refresh, None)
            .expect("persisted exact digest");
    }

    #[test]
    fn builtin_enablement_persists_by_exact_digest() {
        let temp = TempDir::new();
        let data = temp.0.join("data");
        let root = data.join("plugins/builtin/pig.websearch");
        write(
            &root.join("pig-plugin.json"),
            r#"{
              "apiVersion": 1,
              "id": "pig.websearch",
              "name": "Web Search",
              "version": "1.0.0",
              "build": 1,
              "entry": "App.tsx",
              "minHostVersion": "1.0.0",
              "protocols": {"ui": 1, "services": 1},
              "capabilities": [],
              "contributions": {}
            }"#,
        );
        write(
            &root.join("App.tsx"),
            "export default function App() { return null; }",
        );
        extend_user_plugin(&root, r#"{"actions":[]}"#);
        let sources = || {
            vec![PluginPackageSource {
                root: root.clone(),
                source: PluginSource::BuiltIn,
            }]
        };
        let mut registry = PluginRegistry::new(data.clone());
        let snapshot = registry.sync(sources());
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        assert!(snapshot.packages[0].enabled);
        let trust = crate::config::WorkspaceTrust::untrusted(&temp.0);
        assert_eq!(
            enabled_packages(&registry, &trust)
                .expect("enabled contributions")
                .len(),
            1
        );
        let digest = snapshot.packages[0].digest.clone();

        let disabled = registry
            .set_enabled("pig.websearch", &digest, false)
            .expect("disable built-in package");
        assert!(!disabled.packages[0].enabled);
        assert!(
            enabled_packages(&registry, &trust)
                .expect("disabled contributions")
                .is_empty()
        );

        let mut restored = PluginRegistry::new(data.clone());
        assert!(!restored.snapshot.packages[0].enabled);
        let rediscovered = restored.sync(sources());
        assert!(!rediscovered.packages[0].enabled);

        write(&root.join("main.js"), "export default class Changed {}");
        let updated = restored.sync(sources());
        assert_ne!(updated.packages[0].digest, digest);
        assert!(updated.packages[0].enabled);
        let digest = updated.packages[0].digest.clone();
        restored
            .set_enabled("pig.websearch", &digest, false)
            .expect("disable updated package");
        let enabled = restored
            .set_enabled("pig.websearch", &digest, true)
            .expect("re-enable built-in package");
        assert!(enabled.packages[0].enabled);
    }

    #[test]
    fn mcp_snapshots_redact_and_saving_preserves_secret_values() {
        let temp = TempDir::new();
        let data = temp.0.join("data");
        upsert_mcp(
            &data,
            &PluginScope::User,
            "docs",
            serde_json::json!({
                "url": "https://example.test/mcp",
                "headers": {"Authorization": "Bearer secret"},
                "oauth": {"client": "preserved"}
            }),
        )
        .expect("initial config");
        let registry = PluginRegistry::new(data.clone());
        let snapshot = snapshot(
            &registry,
            PluginScope::User,
            vec![],
            None,
            &crate::config::WorkspaceTrust::untrusted(&temp.0),
        );
        let server = &snapshot.mcp.servers[0];
        assert_eq!(server.config["headers"]["Authorization"], REDACTED);
        assert!(!serde_json::to_string(&server).unwrap().contains("secret"));
        upsert_mcp(&data, &PluginScope::User, "docs", server.config.clone())
            .expect("save redacted edit");
        let raw = std::fs::read_to_string(data.join("mcp.json")).unwrap();
        assert!(raw.contains("Bearer secret"));
        assert!(raw.contains("preserved"));
    }

    #[test]
    fn managed_skill_actions_preserve_discovery_and_enable_state() {
        let temp = TempDir::new();
        let data = temp.0.join("data");
        save_skill(
            &data,
            &PluginScope::User,
            "demo",
            "---\nname: demo\ndescription: Demo skill\n---\nBody\n",
            true,
        )
        .expect("create");
        let path = pig_utils::skills::user_root(&data).join("demo/SKILL.md");
        let plugins = PluginRegistry::shared(data.clone());
        set_skill_enabled(&data, &plugins, &path, false, &[], None).expect("disable");
        assert!(discover_skills_with_trust(&temp.0, &plugins, false).is_empty());
        set_skill_enabled(&data, &plugins, &path, true, &[], None).expect("enable");
        assert_eq!(
            discover_skills_with_trust(&temp.0, &plugins, false).len(),
            1
        );
        delete_skill(&data, &PluginScope::User, "demo").expect("delete");
        assert!(!path.exists());
    }

    #[test]
    fn one_package_contributes_multiple_skills_and_mcp_servers() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        extend_user_plugin(
            &root,
            r#"{
              "actions": ["provide_mcp"],
              "skills": [{"path":"skills/alpha"},{"path":"skills/beta"}],
              "mcp": [
                {"name":"first","config":{"command":"first-server"}},
                {"name":"second","config":{"url":"https://second.example/mcp"}}
              ]
            }"#,
        );
        write(
            &root.join("skills/alpha/SKILL.md"),
            "---\nname: alpha\ndescription: First skill\n---\nAlpha instructions\n",
        );
        write(
            &root.join("skills/beta/SKILL.md"),
            "---\nname: beta\ndescription: Second skill\n---\nBeta instructions\n",
        );
        let data = temp.0.join("data");
        let mut registry = PluginRegistry::new(data.clone());
        let snapshot = sync(&mut registry, &root);
        assert_eq!(snapshot.packages[0].contributions.skills.len(), 2);
        assert_eq!(snapshot.packages[0].contributions.mcp.len(), 2);
        registry
            .set_enabled("demo", &snapshot.packages[0].digest, true)
            .expect("enable resource package");
        let plugins = registry.into_shared();

        let skills = discover_skills_with_trust(&temp.0, &plugins, false);
        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        let prompt = skills_section_with_trust(&temp.0, &plugins, false);
        assert!(prompt.contains("First skill") && prompt.contains("Second skill"));
        assert!(
            load_skill_output_with_trust(&temp.0, &plugins, "alpha", false)
                .expect("load first skill")
                .contains("Alpha instructions")
        );
        assert!(
            load_skill_output_with_trust(&temp.0, &plugins, "beta", false)
                .expect("load second skill")
                .contains("Beta instructions")
        );
        let trust = crate::config::WorkspaceTrust::untrusted(&temp.0);
        let servers =
            crate::mcp::McpServerConfig::load_with_trust(&temp.0, &data, &trust, &plugins);
        assert_eq!(
            servers
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );

        plugins
            .lock()
            .expect("plugin registry lock")
            .set_enabled("demo", &snapshot.packages[0].digest, false)
            .unwrap();
        assert!(discover_skills_with_trust(&temp.0, &plugins, false).is_empty());
        assert!(
            crate::mcp::McpServerConfig::load_with_trust(&temp.0, &data, &trust, &plugins)
                .is_empty()
        );
    }

    #[test]
    fn tools_and_http_require_exact_authority() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        extend_user_plugin(
            &root,
            r#"{
              "actions": ["provide_tools", "http"],
              "tools": [{
                "name": "DemoRead",
                "description": "Read a demo resource",
                "read_only": true,
                "parameters": {"type": "object"}
              }],
              "credentials": [{
                "kind": "owned",
                "id": "test-token",
                "label": "Test token"
              }],
              "http": [{
                "id": "api",
                "url": "https://example.test/api",
                "methods": ["POST"],
                "credentials": ["test-token"]
              }]
            }"#,
        );
        let mut registry = PluginRegistry::new(temp.0.join("data"));
        let snapshot = sync(&mut registry, &root);
        let package = &snapshot.packages[0];
        assert!(
            registry
                .tool("demo", &package.digest, "missing", None)
                .is_err()
        );
        assert!(
            registry
                .http_origin("demo", &package.digest, "api", "GET", None)
                .is_err()
        );
        assert!(
            registry
                .credential_allowed("demo", &package.digest, "OTHER_TOKEN", None)
                .is_err()
        );
        registry.set_enabled("demo", &package.digest, true).unwrap();
        let tool = registry
            .tool("demo", &package.digest, "DemoRead", None)
            .unwrap();
        assert!(tool.read_only);
        registry
            .http_origin("demo", &package.digest, "api", "POST", None)
            .unwrap();
        registry
            .credential_allowed("demo", &package.digest, "test-token", None)
            .unwrap();

        registry
            .set_enabled("demo", &package.digest, false)
            .unwrap();
        assert!(
            registry
                .tool("demo", &package.digest, "DemoRead", None)
                .is_err()
        );
        registry.set_enabled("demo", &package.digest, true).unwrap();
        write(&root.join("main.js"), "export default class Changed {}");
        let changed = sync(&mut registry, &root);
        assert_ne!(changed.packages[0].digest, package.digest);
        assert!(
            registry
                .tool("demo", &package.digest, "DemoRead", None)
                .is_err()
        );
        assert!(
            registry
                .credential_allowed("demo", &package.digest, "test-token", None)
                .is_err()
        );
    }

    #[test]
    fn config_actions_require_enabled_exact_digest_and_declared_capabilities() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        extend_user_plugin(
            &root,
            r#"{
              "actions": ["read_config", "write_config"],
              "config": {"items": [{"key": "mode", "default": "auto"}]}
            }"#,
        );
        let mut registry = PluginRegistry::new(temp.0.join("data"));
        let snapshot = sync(&mut registry, &root);
        let package = &snapshot.packages[0];
        registry.set_enabled("demo", &package.digest, true).unwrap();
        registry
            .authorize(
                "demo",
                &package.digest,
                &PluginAction::ReadConfig {
                    path: "mode".into(),
                },
                None,
            )
            .expect("read config capability");
        registry
            .authorize(
                "demo",
                &package.digest,
                &PluginAction::WriteConfig {
                    path: "mode".into(),
                    value: "brave".into(),
                },
                None,
            )
            .expect("write config capability");
        registry
            .set_enabled("demo", &package.digest, false)
            .unwrap();
        assert!(
            registry
                .authorize(
                    "demo",
                    &package.digest,
                    &PluginAction::ReadConfig {
                        path: "mode".into(),
                    },
                    None,
                )
                .is_err()
        );

        registry.set_enabled("demo", &package.digest, true).unwrap();
        write(&root.join("main.js"), "export default class Changed {}");
        let changed = sync(&mut registry, &root);
        assert_ne!(changed.packages[0].digest, package.digest);
        assert!(
            registry
                .authorize(
                    "demo",
                    &package.digest,
                    &PluginAction::WriteConfig {
                        path: "mode".into(),
                        value: "brave".into(),
                    },
                    None,
                )
                .is_err(),
            "an old digest must not retain configuration authority"
        );
    }

    #[test]
    fn shared_provider_credentials_require_a_digest_scoped_grant() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        extend_user_plugin(
            &root,
            r#"{
              "actions": ["http", "manage_own_secrets"],
              "credentials": [
                {"kind": "provider", "id": "model-key", "provider_id": "deepseek"},
                {"kind": "owned", "id": "own-key", "label": "Own key"}
              ],
              "http": [{
                "id": "api",
                "url": "https://example.test/api",
                "methods": ["POST"],
                "credentials": ["model-key"]
              }]
            }"#,
        );
        let mut registry = PluginRegistry::new(temp.0.join("data"));
        let snapshot = sync(&mut registry, &root);
        let package = &snapshot.packages[0];
        registry.set_enabled("demo", &package.digest, true).unwrap();
        assert!(
            registry
                .credential_for_origin("demo", &package.digest, "api", "POST", "model-key", None)
                .is_err()
        );
        assert!(
            registry
                .owned_credential("demo", &package.digest, "model-key", None)
                .is_err()
        );
        registry
            .owned_credential("demo", &package.digest, "own-key", None)
            .unwrap();
        registry
            .set_credential_grant("demo", &package.digest, "model-key", "api", true, None)
            .unwrap();
        registry
            .credential_for_origin("demo", &package.digest, "api", "POST", "model-key", None)
            .unwrap();
        registry
            .set_credential_grant("demo", &package.digest, "model-key", "api", false, None)
            .unwrap();
        assert!(
            registry
                .credential_for_origin("demo", &package.digest, "api", "POST", "model-key", None)
                .is_err()
        );
        registry
            .set_credential_grant("demo", &package.digest, "model-key", "api", true, None)
            .unwrap();
        write(&root.join("main.js"), "export default class Changed {}");
        let changed = sync(&mut registry, &root);
        assert_ne!(changed.packages[0].digest, package.digest);
        assert!(
            registry
                .credential_for_origin("demo", &package.digest, "api", "POST", "model-key", None)
                .is_err()
        );
    }

    #[test]
    fn reserved_tool_names_never_replace_native_or_mcp_tools() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        extend_user_plugin(
            &root,
            r#"{
              "actions": ["provide_tools"],
              "tools": [{
                "name": "mcp__reserved",
                "description": "Reserved name",
                "parameters": {"type": "object"}
              }]
            }"#,
        );
        let mut registry = PluginRegistry::new(temp.0.join("data"));
        let snapshot = registry.sync(vec![PluginPackageSource {
            root,
            source: PluginSource::User,
        }]);
        assert_eq!(snapshot.errors.len(), 1);
    }

    #[test]
    fn update_candidates_keep_the_old_version_until_commit_and_support_rollback() {
        let temp = TempDir::new();
        let first = temp.0.join("first");
        let second = temp.0.join("second");
        user_plugin(&first);
        user_plugin(&second);
        write(
            &second.join("App.tsx"),
            "export default function App() { return 'new'; }",
        );
        let data = temp.0.join("data");
        let mut registry = PluginRegistry::new(data);
        let sources = || {
            vec![
                PluginPackageSource {
                    root: first.clone(),
                    source: PluginSource::User,
                },
                PluginPackageSource {
                    root: second.clone(),
                    source: PluginSource::User,
                },
            ]
        };
        let initial = sync(&mut registry, &first);
        let old_digest = initial.packages[0].digest.clone();
        registry.set_enabled("demo", &old_digest, true).unwrap();
        let discovered = registry.sync(sources());
        let new_digest = discovered
            .packages
            .iter()
            .find(|package| package.digest != old_digest)
            .expect("new package")
            .digest
            .clone();
        let pending = registry.set_enabled("demo", &new_digest, true).unwrap();
        assert!(pending.packages.iter().all(|package| package.enabled));
        registry
            .sync_runtime_contributions(
                "demo",
                &old_digest,
                1,
                pending.generation,
                pig_protocol::PluginContributions::default(),
            )
            .unwrap();
        registry
            .sync_runtime_contributions(
                "demo",
                &new_digest,
                2,
                pending.generation,
                pig_protocol::PluginContributions::default(),
            )
            .unwrap();
        let cancelled = registry.cancel_update("demo", &new_digest).unwrap();
        assert!(
            cancelled
                .packages
                .iter()
                .find(|package| package.digest == old_digest)
                .unwrap()
                .enabled
        );
        assert!(
            !cancelled
                .packages
                .iter()
                .find(|package| package.digest == new_digest)
                .unwrap()
                .enabled
        );
        assert!(
            cancelled
                .runtime_contributions
                .contains_key(&format!("demo:{old_digest}"))
        );
        assert!(
            !cancelled
                .runtime_contributions
                .contains_key(&format!("demo:{new_digest}"))
        );
        registry.set_enabled("demo", &new_digest, true).unwrap();
        registry
            .sync_runtime_contributions(
                "demo",
                &new_digest,
                3,
                registry.snapshot.generation,
                pig_protocol::PluginContributions::default(),
            )
            .unwrap();
        let committed = registry.commit_update("demo", &new_digest).unwrap();
        assert!(
            committed
                .runtime_contributions
                .contains_key(&format!("demo:{new_digest}"))
        );
        assert!(
            !committed
                .runtime_contributions
                .contains_key(&format!("demo:{old_digest}"))
        );
        assert!(
            committed
                .packages
                .iter()
                .find(|package| package.digest == new_digest)
                .unwrap()
                .enabled
        );
        assert!(
            !committed
                .packages
                .iter()
                .find(|package| package.digest == old_digest)
                .unwrap()
                .enabled
        );
        let restored = PluginRegistry::new(temp.0.join("data"));
        assert!(
            restored
                .snapshot
                .packages
                .iter()
                .find(|package| package.digest == new_digest)
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn dynamic_contributions_survive_rediscovery_and_respect_engine_epochs() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        extend_user_plugin(
            &root,
            r#"{"actions":["provide_tools","provide_themes","provide_mcp"]}"#,
        );
        let manifest_path = root.join("pig-plugin.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest")).unwrap();
        manifest["capabilities"] = serde_json::json!([
            {"name":"ui.add","targets":["main","commands"]}
        ]);
        write(&manifest_path, &serde_json::to_string(&manifest).unwrap());
        write(
            &root.join("runtime-theme.json"),
            r#"{"id":"runtime-theme","name":"Runtime","cssVars":{"dark":{}}}"#,
        );
        write(
            &root.join("skills/runtime/SKILL.md"),
            "---\nname: runtime\ndescription: Runtime skill\n---\nRuntime instructions\n",
        );
        let data = temp.0.join("data");
        let mut registry = PluginRegistry::new(data.clone());
        let snapshot = sync(&mut registry, &root);
        let digest = snapshot.packages[0].digest.clone();
        registry.set_enabled("demo", &digest, true).unwrap();
        let engine_generation = registry.snapshot.generation;
        let contributions: pig_protocol::PluginContributions = serde_json::from_value(
            serde_json::json!({
                "pages":[{"id":"runtime","entry":"App.tsx","title":{"en":"Runtime"}}],
                "ui":[{"id":"runtime-panel","kind":"panel","target":"main","export":"App","preserve":[]}],
                "themes":[{"id":"runtime-theme","entry":"runtime-theme.json"}],
                "skills":[{"path":"skills/runtime"}],
                "mcp":[{"name":"runtime","config":{"command":"runtime-server"}}],
                "tools":[{"name":"DynamicEcho","description":"Echo","parameters":{"type":"object"}}],
                "commands":[{"id":"runtime-command","title":{"en":"Runtime"},"order":0,"enabled":true}]
            }),
        )
        .unwrap();
        let accepted = registry
            .sync_runtime_contributions(
                "demo",
                &digest,
                7,
                engine_generation,
                contributions.clone(),
            )
            .expect("register all dynamic contribution kinds");
        let registered = &accepted.runtime_contributions[&format!("demo:{digest}")];
        assert_eq!(registered.pages.len(), 1);
        assert_eq!(registered.ui.len(), 1);
        assert_eq!(registered.themes.len(), 1);
        assert_eq!(registered.skills.len(), 1);
        assert_eq!(registered.mcp.len(), 1);
        assert_eq!(registered.tools.len(), 1);
        assert_eq!(registered.commands.len(), 1);

        let rediscovered = sync(&mut registry, &root);
        assert_eq!(
            rediscovered.runtime_contributions[&format!("demo:{digest}")],
            contributions
        );
        registry
            .sync_runtime_contributions(
                "demo",
                &digest,
                7,
                engine_generation,
                contributions.clone(),
            )
            .expect("an unchanged engine may register after another snapshot generation");
        registry
            .sync_runtime_contributions(
                "demo",
                &digest,
                6,
                registry.snapshot.generation,
                pig_protocol::PluginContributions::default(),
            )
            .expect("an old engine cleanup is ignored");
        assert!(
            registry
                .snapshot
                .runtime_contributions
                .contains_key(&format!("demo:{digest}"))
        );
        assert!(!data.join("plugins-runtime.json").exists());

        let plugins = registry.into_shared();
        let trust = crate::config::WorkspaceTrust::untrusted(&temp.0);
        let toolset = PluginToolset::new(&plugins, &trust, None, "s-runtime");
        assert!(
            toolset
                .tools()
                .iter()
                .any(|tool| tool.name() == "DynamicEcho")
        );
        assert!(
            discover_skills_with_trust(&temp.0, &plugins, false)
                .iter()
                .any(|skill| skill.name == "runtime")
        );
        assert!(
            crate::mcp::McpServerConfig::load_with_trust(&temp.0, &data, &trust, &plugins)
                .iter()
                .any(|server| server.name == "runtime")
        );

        let restarted = PluginRegistry::new(data.clone());
        assert!(restarted.snapshot.runtime_contributions.is_empty());
        let generation = plugins
            .lock()
            .expect("plugin registry lock")
            .snapshot
            .generation;
        plugins
            .lock()
            .expect("plugin registry lock")
            .sync_runtime_contributions(
                "demo",
                &digest,
                7,
                generation,
                pig_protocol::PluginContributions::default(),
            )
            .expect("the current engine removes its contributions");
        assert!(
            plugins
                .lock()
                .expect("plugin registry lock")
                .snapshot
                .runtime_contributions
                .is_empty()
        );
    }
}
