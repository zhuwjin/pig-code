use pig_protocol::PluginPackageInfo;
use serde::Serialize;

#[derive(Serialize)]
struct ThemeLoadResult {
    themes: Vec<pig_protocol::PluginThemeDefinition>,
    errors: Vec<String>,
}

pub fn load_themes(packages_json: &str) -> Result<String, String> {
    let packages: Vec<PluginPackageInfo> =
        serde_json::from_str(packages_json).map_err(|error| error.to_string())?;
    let mut result = ThemeLoadResult {
        themes: Vec::new(),
        errors: Vec::new(),
    };
    for package in packages.iter().filter(|package| package.enabled) {
        for contribution in &package.contributions.themes {
            match pig_core::plugins::load_theme(package, contribution) {
                Ok(theme) => result.themes.push(theme),
                Err(error) => {
                    let detail = match &error {
                        pig_protocol::CoreError::PluginManifest { detail }
                        | pig_protocol::CoreError::PluginPermission { detail } => detail.clone(),
                        other => format!("{other:?}"),
                    };
                    result
                        .errors
                        .push(format!("{} / {}: {}", package.id, contribution.id, detail));
                }
            }
        }
    }
    serde_json::to_string(&result).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::PluginSource;
    use std::path::Path;

    #[test]
    fn example_package_loads_a_selectable_theme_and_style_pack() {
        let root = std::env::temp_dir().join(format!(
            "pig-code-ts-theme-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let package_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/moon-plugin");
        let manifest: pig_protocol::PluginManifest = serde_json::from_slice(
            &std::fs::read(package_root.join("pig-plugin.json")).expect("read manifest"),
        )
        .expect("valid manifest");
        let digest = pig_core::plugins::package_digest(&package_root).expect("package digest");
        let package = PluginPackageInfo {
            id: manifest.id,
            name: manifest.name.fallback(),
            version: manifest.version,
            digest: digest.clone(),
            content_digest: digest,
            root: package_root,
            source: PluginSource::User,
            enabled: true,
            ui_entry: manifest.ui_entry,
            component_entry: manifest.component_entry,
            backend: manifest.backend,
            contributions: manifest.contributions,
            capabilities: manifest.capabilities,
        };
        let encoded =
            load_themes(&serde_json::to_string(&vec![package.clone()]).expect("encode package"))
                .expect("load themes");
        let result: serde_json::Value = serde_json::from_str(&encoded).expect("decode result");
        assert_eq!(result["themes"].as_array().expect("themes").len(), 2);
        assert!(result["errors"].as_array().expect("errors").is_empty());
        assert_eq!(result["themes"][0]["kind"], "theme");
        assert_eq!(result["themes"][1]["kind"], "style");
        assert!(
            result["themes"][1]["background"]["asset"]
                .as_str()
                .expect("background URL")
                .starts_with("file://")
        );

        let disabled = PluginPackageInfo {
            enabled: false,
            ..package
        };
        let encoded =
            load_themes(&serde_json::to_string(&vec![disabled]).expect("encode disabled"))
                .expect("load disabled");
        let result: serde_json::Value = serde_json::from_str(&encoded).expect("decode disabled");
        assert!(result["themes"].as_array().expect("themes").is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
