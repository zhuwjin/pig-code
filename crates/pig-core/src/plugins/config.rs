use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pig_protocol::{
    AppConfig, CoreError, PluginConfigItem, PluginContributions, PluginPackageInfo,
};

const CONFIG_FILE: &str = "plugins-config.json";

type StoredValues = BTreeMap<String, BTreeMap<String, serde_json::Value>>;

/// `.pigcode/plugins.json`: a trusted workspace's overrides. `config` wins
/// over the user's value for the same key (undeclared keys keep inheriting);
/// `enabled: false` disables an enabled package in this workspace. A workspace
/// may enable a resource-only package it disabled globally; enabling a package
/// with a backend stays a user-level decision (the backend is global).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct WorkspacePluginLayer {
    #[serde(default)]
    pub config: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub enabled: BTreeMap<String, bool>,
}

pub(crate) const WORKSPACE_PLUGIN_CONFIG: &str = ".pigcode/plugins.json";

pub(crate) fn load_workspace_layer(workspace: &Path) -> WorkspacePluginLayer {
    std::fs::read(workspace.join(WORKSPACE_PLUGIN_CONFIG))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_workspace_layer(workspace: &Path, layer: &WorkspacePluginLayer) -> Result<(), CoreError> {
    let path = workspace.join(WORKSPACE_PLUGIN_CONFIG);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| CoreError::ConfigWrite {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
    }
    let bytes = serde_json::to_vec_pretty(layer).map_err(|error| CoreError::ConfigSerialize {
        detail: error.to_string(),
    })?;
    crate::config::write_private_file(&path, &bytes)
}

fn trusted_workspace(app_config: Option<&AppConfig>, workspace: &Path) -> bool {
    crate::config::WorkspaceTrust::for_config(app_config, workspace).admits(workspace)
}

pub(crate) enum ConfigWrite {
    Plugin(serde_json::Value),
    App {
        config: Box<AppConfig>,
        value: serde_json::Value,
    },
}

enum ConfigPath {
    App(Vec<String>),
    Plugin { package: String, key: String },
}

pub(crate) fn validate_declaration(contributions: &PluginContributions) -> Result<(), CoreError> {
    let declaration = &contributions.config;
    let mut keys = std::collections::BTreeSet::new();
    for item in &declaration.items {
        validate_key(&item.key)?;
        if !keys.insert(&item.key) {
            return Err(permission(format!(
                "duplicate configuration key `{}`",
                item.key
            )));
        }
        if let Some(schema) = &item.schema {
            super::schema::check_supported(schema).map_err(|error| {
                permission(format!("configuration `{}` schema: {error}", item.key))
            })?;
            if let Err(issues) = super::schema::validate(schema, &item.default) {
                return Err(permission(format!(
                    "configuration `{}` default does not satisfy its schema: {}",
                    item.key,
                    issues.join("; ")
                )));
            }
        }
    }
    if !declaration.items.is_empty()
        && !contributions
            .actions
            .contains(&pig_protocol::PluginCapability::ReadConfig)
        && !contributions
            .actions
            .contains(&pig_protocol::PluginCapability::WriteConfig)
    {
        return Err(permission(
            "configuration items require read_config or write_config",
        ));
    }
    validate_paths(&declaration.read, false, contributions)?;
    validate_paths(&declaration.write, true, contributions)
}

fn validate_paths(
    paths: &[String],
    write: bool,
    contributions: &PluginContributions,
) -> Result<(), CoreError> {
    let capability = if write {
        pig_protocol::PluginCapability::WriteConfig
    } else {
        pig_protocol::PluginCapability::ReadConfig
    };
    if !paths.is_empty() && !contributions.actions.contains(&capability) {
        return Err(permission(format!(
            "external configuration paths require {capability:?}"
        )));
    }
    let mut unique = std::collections::BTreeSet::new();
    for path in paths {
        parse_external_path(path)?;
        if !unique.insert(path) {
            return Err(permission(format!(
                "duplicate external configuration path `{path}`"
            )));
        }
    }
    Ok(())
}

pub(crate) fn read_with_workspace(
    registry: &super::PluginRegistry,
    app_config: Option<&AppConfig>,
    caller: &str,
    path: &str,
    workspace: Option<&Path>,
) -> Result<serde_json::Value, CoreError> {
    match parse_path(caller, path)? {
        ConfigPath::App(segments) => {
            authorize_external(registry, caller, path, false)?;
            let app_config = app_config
                .ok_or_else(|| permission("the application configuration is unavailable"))?;
            let json =
                serde_json::to_value(app_config).map_err(|error| permission(error.to_string()))?;
            value_at(&json, &segments, path).cloned()
        }
        ConfigPath::Plugin { package, key } => {
            if package != caller {
                authorize_external(registry, caller, path, false)?;
            }
            let item = target_item(registry, &package, &key)?;
            // An untrusted workspace's file is not even read: it runs on the
            // user's values.
            if let Some(workspace) = workspace
                && trusted_workspace(app_config, workspace)
                && let Some(value) = load_workspace_layer(workspace)
                    .config
                    .get(&package)
                    .and_then(|package_values| package_values.get(&key))
            {
                return Ok(value.clone());
            }
            let values = load(&registry.data_dir)?;
            Ok(values
                .get(&package)
                .and_then(|package_values| package_values.get(&key))
                .cloned()
                .unwrap_or_else(|| item.default.clone()))
        }
    }
}

pub(crate) fn write_with_workspace(
    registry: &super::PluginRegistry,
    app_config: Option<&AppConfig>,
    caller: &str,
    path: &str,
    value: serde_json::Value,
    workspace: Option<&Path>,
) -> Result<ConfigWrite, CoreError> {
    match parse_path(caller, path)? {
        ConfigPath::App(segments) => {
            authorize_external(registry, caller, path, true)?;
            let app_config = app_config
                .ok_or_else(|| permission("the application configuration is unavailable"))?;
            let mut json =
                serde_json::to_value(app_config).map_err(|error| permission(error.to_string()))?;
            set_value_at(&mut json, &segments, path, value)?;
            let config: AppConfig = serde_json::from_value(json)
                .map_err(|error| permission(format!("invalid value for `{path}`: {error}")))?;
            let json =
                serde_json::to_value(&config).map_err(|error| permission(error.to_string()))?;
            let value = value_at(&json, &segments, path)?.clone();
            Ok(ConfigWrite::App {
                config: Box::new(config),
                value,
            })
        }
        ConfigPath::Plugin { package, key } => {
            if package != caller {
                authorize_external(registry, caller, path, true)?;
            }
            let item = target_item(registry, &package, &key)?;
            validate_value(&item, &value)?;
            if let Some(workspace) = workspace {
                if !trusted_workspace(app_config, workspace) {
                    return Err(permission(
                        "the workspace is not trusted; its plugin options cannot be changed",
                    ));
                }
                let mut layer = load_workspace_layer(workspace);
                layer
                    .config
                    .entry(package)
                    .or_default()
                    .insert(key, value.clone());
                save_workspace_layer(workspace, &layer)?;
                return Ok(ConfigWrite::Plugin(value));
            }
            let mut values = load(&registry.data_dir)?;
            values
                .entry(package)
                .or_default()
                .insert(key, value.clone());
            save(&registry.data_dir, &values)?;
            Ok(ConfigWrite::Plugin(value))
        }
    }
}

/// The user-level read (tests and callers without a workspace).
#[cfg(test)]
pub(crate) fn read(
    registry: &super::PluginRegistry,
    app_config: Option<&AppConfig>,
    caller: &str,
    path: &str,
) -> Result<serde_json::Value, CoreError> {
    read_with_workspace(registry, app_config, caller, path, None)
}

/// The user-level write (tests and callers without a workspace).
#[cfg(test)]
pub(crate) fn write(
    registry: &super::PluginRegistry,
    app_config: Option<&AppConfig>,
    caller: &str,
    path: &str,
    value: serde_json::Value,
) -> Result<ConfigWrite, CoreError> {
    write_with_workspace(registry, app_config, caller, path, value, None)
}

fn validate_value(item: &PluginConfigItem, value: &serde_json::Value) -> Result<(), CoreError> {
    if let Some(schema) = &item.schema {
        if let Err(issues) = super::schema::validate(schema, value) {
            return Err(permission(format!(
                "configuration `{}` is invalid: {}",
                item.key,
                issues.join("; ")
            )));
        }
        return Ok(());
    }
    if !item.default.is_null() && !same_json_type(&item.default, value) {
        return Err(permission(format!(
            "configuration `{}` expects the same JSON type as its default",
            item.key
        )));
    }
    Ok(())
}

fn authorize_external(
    registry: &super::PluginRegistry,
    caller: &str,
    path: &str,
    write: bool,
) -> Result<(), CoreError> {
    let caller = package(registry, caller)?;
    let allowed = if write {
        &caller.contributions.config.write
    } else {
        &caller.contributions.config.read
    };
    if allowed.iter().any(|allowed| allowed == path) {
        Ok(())
    } else {
        Err(permission(format!(
            "plugin `{}` does not declare {} access to `{path}`",
            caller.id,
            if write { "write" } else { "read" }
        )))
    }
}

fn target_item<'a>(
    registry: &'a super::PluginRegistry,
    id: &str,
    key: &str,
) -> Result<&'a PluginConfigItem, CoreError> {
    package(registry, id)?
        .contributions
        .config
        .items
        .iter()
        .find(|item| item.key == key)
        .ok_or_else(|| {
            permission(format!(
                "plugin `{id}` does not declare configuration key `{key}`"
            ))
        })
}

fn package<'a>(
    registry: &'a super::PluginRegistry,
    id: &str,
) -> Result<&'a PluginPackageInfo, CoreError> {
    registry
        .snapshot
        .packages
        .iter()
        .find(|package| package.id == id)
        .ok_or_else(|| permission(format!("unknown plugin `{id}`")))
}

fn parse_path(caller: &str, path: &str) -> Result<ConfigPath, CoreError> {
    if path.starts_with("app.") || path.starts_with("plugin.") {
        return parse_external_path(path);
    }
    validate_key(path)?;
    Ok(ConfigPath::Plugin {
        package: caller.to_owned(),
        key: path.to_owned(),
    })
}

fn parse_external_path(path: &str) -> Result<ConfigPath, CoreError> {
    if let Some(rest) = path.strip_prefix("app.") {
        let segments: Vec<String> = rest.split('.').map(str::to_owned).collect();
        if segments.is_empty() {
            return Err(permission(format!("invalid application path `{path}`")));
        }
        for segment in &segments {
            validate_segment(segment, path)?;
        }
        return Ok(ConfigPath::App(segments));
    }
    if let Some(rest) = path.strip_prefix("plugin.") {
        let (package, key) = rest
            .rsplit_once('.')
            .ok_or_else(|| permission(format!("invalid plugin configuration path `{path}`")))?;
        validate_package_id(package, path)?;
        validate_key(key)?;
        return Ok(ConfigPath::Plugin {
            package: package.to_owned(),
            key: key.to_owned(),
        });
    }
    Err(permission(format!(
        "external configuration path must start with app. or plugin.: `{path}`"
    )))
}

fn validate_key(key: &str) -> Result<(), CoreError> {
    validate_segment(key, key)
}

fn validate_package_id(id: &str, path: &str) -> Result<(), CoreError> {
    if id.is_empty()
        || id.starts_with(['.', '-', '_'])
        || id.ends_with(['.', '-', '_'])
        || id.contains("..")
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(permission(format!(
            "invalid plugin id in configuration path `{path}`"
        )));
    }
    Ok(())
}

fn validate_segment(segment: &str, path: &str) -> Result<(), CoreError> {
    if segment.is_empty()
        || !segment.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(permission(format!(
            "invalid configuration key in `{path}`; keys must be single lowercase segments"
        )));
    }
    Ok(())
}

fn value_at<'a>(
    root: &'a serde_json::Value,
    segments: &[String],
    path: &str,
) -> Result<&'a serde_json::Value, CoreError> {
    let mut current = root;
    for segment in segments {
        current = current.get(segment).ok_or_else(|| {
            permission(format!(
                "application configuration path `{path}` does not exist"
            ))
        })?;
    }
    Ok(current)
}

fn set_value_at(
    root: &mut serde_json::Value,
    segments: &[String],
    path: &str,
    value: serde_json::Value,
) -> Result<(), CoreError> {
    let (last, parents) = segments
        .split_last()
        .ok_or_else(|| permission("an application configuration path cannot be empty"))?;
    let mut current = root;
    for segment in parents {
        current = current.get_mut(segment).ok_or_else(|| {
            permission(format!(
                "application configuration path `{path}` does not exist"
            ))
        })?;
    }
    let slot = current.get_mut(last).ok_or_else(|| {
        permission(format!(
            "application configuration path `{path}` does not exist"
        ))
    })?;
    *slot = value;
    Ok(())
}

fn same_json_type(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    matches!(
        (left, right),
        (serde_json::Value::Null, serde_json::Value::Null)
            | (serde_json::Value::Bool(_), serde_json::Value::Bool(_))
            | (serde_json::Value::Number(_), serde_json::Value::Number(_))
            | (serde_json::Value::String(_), serde_json::Value::String(_))
            | (serde_json::Value::Array(_), serde_json::Value::Array(_))
            | (serde_json::Value::Object(_), serde_json::Value::Object(_))
    )
}

fn load(data_dir: &Path) -> Result<StoredValues, CoreError> {
    let path = config_path(data_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(CoreError::PluginManifest {
                detail: error.to_string(),
            });
        }
    };
    serde_json::from_slice(&bytes).map_err(|error| CoreError::PluginManifest {
        detail: format!("{}: {error}", path.display()),
    })
}

fn save(data_dir: &Path, values: &StoredValues) -> Result<(), CoreError> {
    let mut bytes =
        serde_json::to_vec_pretty(values).map_err(|error| CoreError::PluginManifest {
            detail: error.to_string(),
        })?;
    bytes.push(b'\n');
    crate::config::write_private_file(&config_path(data_dir), &bytes)
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CONFIG_FILE)
}

fn permission(detail: impl Into<String>) -> CoreError {
    CoreError::PluginPermission {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::{
        PluginCapability, PluginConfigDeclaration, PluginContributions, PluginSource,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "pig-plugin-config-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
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

    fn item(key: &str, default: serde_json::Value) -> PluginConfigItem {
        PluginConfigItem {
            key: key.into(),
            description: Default::default(),
            default,
            schema: None,
        }
    }

    fn package(
        id: &str,
        items: Vec<PluginConfigItem>,
        read: &[&str],
        write: &[&str],
    ) -> PluginPackageInfo {
        PluginPackageInfo {
            id: id.into(),
            name: id.into(),
            version: "1".into(),
            digest: "digest".into(),
            content_digest: "digest".into(),
            root: PathBuf::new(),
            source: PluginSource::User,
            enabled: true,
            incompatible: None,
            backend: None,
            contributions: PluginContributions {
                config: PluginConfigDeclaration {
                    items,
                    read: read.iter().map(|path| (*path).into()).collect(),
                    write: write.iter().map(|path| (*path).into()).collect(),
                },
                actions: vec![PluginCapability::ReadConfig, PluginCapability::WriteConfig],
                ..Default::default()
            },
            capabilities: Vec::new(),
        }
    }

    #[test]
    fn plugin_values_use_defaults_persist_and_enforce_target_declarations() {
        let temp = TempDir::new();
        let mut registry = super::super::PluginRegistry::new(temp.0.clone());
        registry.snapshot.packages = vec![
            package("caller", vec![item("mode", "auto".into())], &[], &[]),
            package(
                "target",
                vec![item("endpoint", serde_json::Value::Null)],
                &[],
                &[],
            ),
        ];

        assert_eq!(
            read(&registry, None, "caller", "mode").expect("default"),
            "auto"
        );
        write(&registry, None, "caller", "mode", "brave".into()).expect("write own value");
        assert_eq!(
            read(&registry, None, "caller", "mode").expect("stored value"),
            "brave"
        );
        write(&registry, None, "caller", "mode", "brave".into()).expect("idempotent write");
        let path = config_path(&temp.0);
        let first = std::fs::read(&path).expect("first persisted state");
        write(&registry, None, "caller", "mode", "brave".into()).expect("replayed write");
        assert_eq!(
            std::fs::read(&path).expect("replayed persisted state"),
            first,
            "replaying the same value is byte-for-byte idempotent"
        );
        assert!(
            load(&temp.0)
                .expect("reload plugin state")
                .get("caller")
                .and_then(|values| values.get("mode"))
                == Some(&serde_json::Value::String("brave".into())),
            "the scoped value survives reload"
        );
        assert!(
            std::fs::read_dir(&temp.0)
                .expect("plugin data dir")
                .all(|entry| !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp-")),
            "atomic persistence leaves no temporary files"
        );
        assert!(
            write(&registry, None, "caller", "mode", 1.into()).is_err(),
            "a different JSON type must be rejected"
        );
        assert!(
            read(&registry, None, "caller", "plugin.target.endpoint").is_err(),
            "cross-plugin reads require an explicit grant"
        );

        registry.snapshot.packages[0]
            .contributions
            .config
            .read
            .push("plugin.target.endpoint".into());
        registry.snapshot.packages[0]
            .contributions
            .config
            .write
            .push("plugin.target.endpoint".into());
        write(
            &registry,
            None,
            "caller",
            "plugin.target.endpoint",
            serde_json::Value::Null,
        )
        .expect("write external value");
        assert_eq!(
            read(&registry, None, "caller", "plugin.target.endpoint")
                .expect("explicit null must not fall back"),
            serde_json::Value::Null
        );

        registry.snapshot.packages[1]
            .contributions
            .config
            .items
            .clear();
        assert!(
            read(&registry, None, "caller", "plugin.target.endpoint").is_err(),
            "a removed target declaration makes its stored value dormant"
        );
    }

    #[test]
    fn application_paths_validate_permissions_values_and_schema() {
        let temp = TempDir::new();
        let mut registry = super::super::PluginRegistry::new(temp.0.clone());
        registry.snapshot.packages = vec![package(
            "caller",
            vec![],
            &["app.theme"],
            &["app.default_delivery"],
        )];
        let app = AppConfig::default();

        assert_eq!(
            read(&registry, Some(&app), "caller", "app.theme").expect("theme"),
            serde_json::Value::Null
        );
        assert!(
            read(&registry, Some(&app), "caller", "app.default_delivery").is_err(),
            "write permission does not imply read permission"
        );
        let ConfigWrite::App { config, value } = write(
            &registry,
            Some(&app),
            "caller",
            "app.default_delivery",
            "steer".into(),
        )
        .expect("write app value") else {
            panic!("plugin write returned the wrong namespace");
        };
        assert_eq!(
            config.default_delivery,
            pig_protocol::MessageDelivery::Steer
        );
        assert_eq!(value, "steer");
        assert!(
            write(
                &registry,
                Some(&app),
                "caller",
                "app.default_delivery",
                "not-a-delivery".into(),
            )
            .is_err(),
            "AppConfig must reject invalid enum values"
        );
        assert!(
            read(&registry, Some(&app), "caller", "app.not_a_field").is_err(),
            "unknown application paths must be rejected"
        );
    }

    #[test]
    fn declaration_validation_rejects_bad_keys_paths_and_missing_capabilities() {
        let mut contributions = PluginContributions {
            config: PluginConfigDeclaration {
                items: vec![item("bad.key", serde_json::Value::Null)],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate_declaration(&contributions).is_err());

        contributions.config.items = vec![item("mode", serde_json::Value::Null)];
        assert!(validate_declaration(&contributions).is_err());
        contributions.actions.push(PluginCapability::ReadConfig);
        assert!(validate_declaration(&contributions).is_ok());

        contributions.config.read = vec!["app.*".into()];
        assert!(validate_declaration(&contributions).is_err());
        contributions.config.read = vec!["app.theme".into()];
        assert!(validate_declaration(&contributions).is_ok());
    }

    #[test]
    fn a_workspace_overrides_options_only_when_trusted_and_keys_inherit() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let mut registry = super::super::PluginRegistry::new(temp.0.clone());
        registry.snapshot.packages = vec![package(
            "caller",
            vec![item("mode", "auto".into()), item("other", "user".into())],
            &[],
            &[],
        )];
        let app = AppConfig {
            providers: vec![],
            default_provider: "x".into(),
            default_model: "y".into(),
            ..Default::default()
        };
        assert!(
            write_with_workspace(
                &registry,
                Some(&app),
                "caller",
                "mode",
                "workspace".into(),
                Some(&workspace),
            )
            .is_err(),
            "an untrusted workspace cannot write options"
        );
        write(&registry, Some(&app), "caller", "mode", "user".into()).expect("user write");
        let mut trusted = app.clone();
        trusted.hooks.trusted_projects = vec![
            workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.clone())
                .display()
                .to_string(),
        ];
        write_with_workspace(
            &registry,
            Some(&trusted),
            "caller",
            "mode",
            "workspace".into(),
            Some(&workspace),
        )
        .expect("trusted workspace write");
        assert_eq!(
            read_with_workspace(
                &registry,
                Some(&trusted),
                "caller",
                "mode",
                Some(&workspace)
            )
            .expect("workspace value"),
            "workspace"
        );
        assert_eq!(
            read(&registry, Some(&trusted), "caller", "mode").expect("user value"),
            "user",
            "the user store is untouched"
        );
        assert_eq!(
            read_with_workspace(
                &registry,
                Some(&trusted),
                "caller",
                "other",
                Some(&workspace)
            )
            .expect("inherited value"),
            "user",
            "an undeclared key keeps inheriting the user value"
        );
        let layer = std::fs::read_to_string(workspace.join(super::WORKSPACE_PLUGIN_CONFIG))
            .expect("workspace layer file");
        assert!(layer.contains("workspace"));
    }

    #[test]
    fn configuration_schemas_validate_writes_and_defaults() {
        let mut contributions = PluginContributions {
            config: PluginConfigDeclaration {
                items: vec![PluginConfigItem {
                    key: "mode".into(),
                    description: Default::default(),
                    default: "a".into(),
                    schema: Some(serde_json::json!({
                        "type": "string",
                        "enum": ["a", "b"]
                    })),
                }],
                ..Default::default()
            },
            actions: vec![PluginCapability::ReadConfig, PluginCapability::WriteConfig],
            ..Default::default()
        };
        assert!(validate_declaration(&contributions).is_ok());
        let temp = TempDir::new();
        let mut registry = super::super::PluginRegistry::new(temp.0.clone());
        registry.snapshot.packages = vec![package(
            "caller",
            contributions.config.items.clone(),
            &[],
            &[],
        )];
        assert!(
            write(&registry, None, "caller", "mode", "c".into()).is_err(),
            "the schema rejects a value outside its enum"
        );
        write(&registry, None, "caller", "mode", "b".into())
            .expect("a schema-valid value must persist");
        contributions.config.items[0].default = "c".into();
        assert!(
            validate_declaration(&contributions).is_err(),
            "a default outside its schema is a declaration error"
        );
    }
}
