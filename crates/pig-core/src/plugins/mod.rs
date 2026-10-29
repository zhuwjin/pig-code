pub(crate) mod config;
mod contributions;
mod http;
mod marketplace;
mod registry;
mod services;
mod tools;

pub use http::request_http;
pub use marketplace::{DEFAULT_MARKETPLACE_INDEX_URL, fetch_catalog, install_package};
pub use tools::{PluginToolDispatcher, PluginToolFilter, PluginToolset};

pub use contributions::{
    discover_skills_with_trust, enabled_packages, load_skill_output_with_trust, mcp_servers,
    skills_section_with_trust, skills_state,
};
pub use registry::package_digest;
use registry::{PersistedPlugins, authorized_package};
pub use services::{
    SessionServices, delete_mcp, delete_skill, save_skill, set_mcp_enabled, set_skill_enabled,
    snapshot, upsert_mcp,
};

use pig_protocol::{PluginSnapshot, PluginSource};

pub(crate) const PLUGIN_STATE_FILE: &str = "plugins-state.json";
pub(crate) const REDACTED: &str = "${PIG_REDACTED}";
const GENERATED_TYPES: &str = "gpui-kit.d.ts";
const BUILTIN_PACKAGES: [&str; 1] = ["pig.websearch"];

pub struct PluginRegistry {
    pub(crate) data_dir: std::path::PathBuf,
    pub(crate) generation: u64,
    pub(crate) snapshot: PluginSnapshot,
    enabled: std::collections::BTreeMap<String, String>,
    disabled_builtin: std::collections::BTreeMap<String, String>,
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
            &root.join("gpui-shell.json"),
            r#"{"id":"demo","name":"Demo","version":"1.0.0","entry":"main.js"}"#,
        );
        write(&root.join("main.js"), "export default class Demo {}");
        write(
            &root.join("pig-code.json"),
            r#"{"actions":["read_settings"],"pages":[{"id":"home","entry":"main.js","title":{"en":"Home"}}]}"#,
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
            &root.join("gpui-shell.json"),
            r#"{"id":"pig.websearch","name":"Web Search","version":"1.0.0","entry":"main.js"}"#,
        );
        write(&root.join("main.js"), "export default class Search {}");
        write(&root.join("pig-code.json"), r#"{"actions":[]}"#);
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
            enabled_packages(&data, &trust)
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
            enabled_packages(&data, &trust)
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
        set_skill_enabled(&data, &path, false, &[], None).expect("disable");
        assert!(discover_skills_with_trust(&temp.0, &data, false).is_empty());
        set_skill_enabled(&data, &path, true, &[], None).expect("enable");
        assert_eq!(discover_skills_with_trust(&temp.0, &data, false).len(), 1);
        delete_skill(&data, &PluginScope::User, "demo").expect("delete");
        assert!(!path.exists());
    }

    #[test]
    fn one_package_contributes_multiple_skills_and_mcp_servers() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        write(
            &root.join("pig-code.json"),
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

        let skills = discover_skills_with_trust(&temp.0, &data, false);
        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        let prompt = skills_section_with_trust(&temp.0, &data, false);
        assert!(prompt.contains("First skill") && prompt.contains("Second skill"));
        assert!(
            load_skill_output_with_trust(&temp.0, &data, "alpha", false)
                .expect("load first skill")
                .contains("Alpha instructions")
        );
        assert!(
            load_skill_output_with_trust(&temp.0, &data, "beta", false)
                .expect("load second skill")
                .contains("Beta instructions")
        );
        let trust = crate::config::WorkspaceTrust::untrusted(&temp.0);
        let servers = crate::mcp::McpServerConfig::load_with_trust(&temp.0, &data, &trust);
        assert_eq!(
            servers
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );

        registry
            .set_enabled("demo", &snapshot.packages[0].digest, false)
            .unwrap();
        assert!(discover_skills_with_trust(&temp.0, &data, false).is_empty());
        assert!(crate::mcp::McpServerConfig::load_with_trust(&temp.0, &data, &trust).is_empty());
    }

    #[test]
    fn tools_and_http_require_exact_authority() {
        let temp = TempDir::new();
        let root = temp.0.join("plugin");
        user_plugin(&root);
        write(
            &root.join("pig-code.json"),
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
        write(
            &root.join("pig-code.json"),
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
        write(
            &root.join("pig-code.json"),
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
        write(
            &root.join("pig-code.json"),
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
}
