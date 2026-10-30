use std::path::Path;

use pig_protocol::{PluginPackageInfo, PluginPackageSource, PluginSource};
use serde::Serialize;

#[derive(Serialize)]
struct ThemeLoadResult {
    themes: Vec<pig_protocol::PluginThemeDefinition>,
    errors: Vec<String>,
}

pub fn package_sources(data_dir: &Path) -> Vec<PluginPackageSource> {
    let root = data_dir.join("plugins");
    let mut packages = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return packages;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.join("gpui-shell.json").is_file() {
            packages.push(PluginPackageSource {
                root: path,
                source: PluginSource::User,
            });
        }
    }
    packages
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

pub fn install_selftest_theme(data_dir: &Path) -> String {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/acme.visual");
    let target = data_dir.join("plugins/acme.visual");
    std::fs::create_dir_all(&target).expect("create selftest theme package");
    for file in [
        "gpui-shell.json",
        "pig-code.json",
        "main.js",
        "midnight.json",
        "glass.json",
        "background.svg",
    ] {
        std::fs::copy(source.join(file), target.join(file)).expect("copy selftest theme file");
    }
    pig_core::plugins::package_digest(&target).expect("digest selftest theme package")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let data = root.join("data");
        let digest = install_selftest_theme(&data);
        let package_root = data.join("plugins/acme.visual");
        let contributions: pig_protocol::PluginContributions = serde_json::from_slice(
            &std::fs::read(package_root.join("pig-code.json")).expect("read contributions"),
        )
        .expect("valid contributions");
        let package = PluginPackageInfo {
            id: "acme.visual".into(),
            name: "Acme Visual Themes".into(),
            version: "1.0.0".into(),
            digest,
            root: package_root,
            source: PluginSource::User,
            enabled: true,
            contributions,
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
