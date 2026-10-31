use std::path::Path;

use pig_protocol::{
    CoreError, PLUGIN_THEME_SLOTS, PluginPackageInfo, PluginThemeContribution,
    PluginThemeDefinition, PluginThemeKind,
};

use super::registry::{canonical, package_digest, relative_file, validate_id};

const MAX_THEME_BYTES: u64 = 1024 * 1024;

pub(crate) fn validate_theme(
    root: &Path,
    contribution: &PluginThemeContribution,
) -> Result<(), CoreError> {
    validate_id(&contribution.id)?;
    relative_file(&contribution.entry)?;
    let path = root.join(&contribution.entry);
    let size = std::fs::metadata(&path)
        .map_err(|error| manifest(format!("{}: {error}", path.display())))?
        .len();
    if size > MAX_THEME_BYTES {
        return Err(manifest("theme definition exceeds the 1 MiB limit"));
    }
    let bytes = std::fs::read(&path).map_err(|error| manifest(error.to_string()))?;
    let definition: PluginThemeDefinition = serde_json::from_slice(&bytes)
        .map_err(|error| manifest(format!("{}: {error}", path.display())))?;
    validate_definition(root, contribution, &definition)
}

pub fn load_theme(
    package: &PluginPackageInfo,
    contribution: &PluginThemeContribution,
) -> Result<PluginThemeDefinition, CoreError> {
    if !package.enabled {
        return Err(permission("the plugin is disabled"));
    }
    if package_digest(&package.root)? != package.content_digest {
        return Err(permission("the plugin files changed after discovery"));
    }
    let root = canonical(&package.root)?;
    let mut definition = validate_and_read(&root, contribution)?;
    if let Some(background) = &mut definition.background {
        let asset = canonical(&root.join(&background.asset))?;
        if !asset.starts_with(&root) {
            return Err(manifest("theme background must stay inside the package"));
        }
        background.asset = reqwest::Url::from_file_path(&asset)
            .map_err(|()| manifest("theme background is not a valid file URL"))?
            .to_string();
    }
    Ok(definition)
}

fn validate_and_read(
    root: &Path,
    contribution: &PluginThemeContribution,
) -> Result<PluginThemeDefinition, CoreError> {
    validate_theme(root, contribution)?;
    let path = root.join(&contribution.entry);
    let bytes = std::fs::read(&path).map_err(|error| manifest(error.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|error| manifest(format!("{}: {error}", path.display())))
}

fn validate_definition(
    root: &Path,
    contribution: &PluginThemeContribution,
    definition: &PluginThemeDefinition,
) -> Result<(), CoreError> {
    if definition.id != contribution.id {
        return Err(manifest(format!(
            "theme id `{}` does not match its contribution `{}`",
            definition.id, contribution.id
        )));
    }
    if definition.name.trim().is_empty() {
        return Err(manifest("theme name must not be empty"));
    }
    match definition.kind {
        PluginThemeKind::Theme if !definition.target_themes.is_empty() => {
            return Err(manifest("a selectable theme cannot declare targetThemes"));
        }
        PluginThemeKind::Style if definition.target_themes.is_empty() => {
            return Err(manifest("a style pack must declare targetThemes"));
        }
        _ => {}
    }
    for target in &definition.target_themes {
        if target != "*" {
            validate_id(target)?;
        }
    }
    for section in [
        &definition.css_vars.theme,
        &definition.css_vars.light,
        &definition.css_vars.dark,
    ] {
        for (name, value) in section {
            validate_id(name)?;
            if !matches!(
                value,
                serde_json::Value::String(_) | serde_json::Value::Number(_)
            ) {
                return Err(manifest(format!(
                    "theme token `{name}` must be a CSS string or number"
                )));
            }
        }
    }
    for (slot, part) in &definition.parts {
        if !PLUGIN_THEME_SLOTS.contains(&slot.as_str()) {
            return Err(manifest(format!("unknown theme slot `{slot}`")));
        }
        if part
            .class
            .as_deref()
            .is_some_and(|class| class.len() > 16 * 1024)
        {
            return Err(manifest(format!("theme slot `{slot}` class is too long")));
        }
    }
    if let Some(background) = &definition.background {
        relative_file(&background.asset)?;
        let path = root.join(&background.asset);
        if !path.is_file() {
            return Err(manifest(format!(
                "theme background does not exist: {}",
                path.display()
            )));
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(
            extension.as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "svg" | "gif"
        ) {
            return Err(manifest(
                "theme background must be a PNG, JPEG, WebP, SVG or GIF",
            ));
        }
        if background
            .opacity
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(manifest("theme background opacity must be between 0 and 1"));
        }
        if background
            .blur
            .is_some_and(|value| !value.is_finite() || !(0.0..=64.0).contains(&value))
        {
            return Err(manifest("theme background blur must be between 0 and 64"));
        }
    }
    Ok(())
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
