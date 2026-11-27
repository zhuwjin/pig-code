use std::path::PathBuf;

use pig_protocol::PluginPackageInfo;
use serde::Serialize;

#[derive(Serialize)]
struct ThemeLoadResult {
    themes: Vec<pig_protocol::PluginThemeDefinition>,
    errors: Vec<String>,
}

fn inline_background_asset(asset: &str) -> Option<String> {
    let stripped = asset.strip_prefix("file://")?;
    let path_str = if stripped.starts_with('/')
        && stripped.as_bytes().get(2) == Some(&b':')
    {
        &stripped[1..]
    } else {
        stripped
    };
    let path = PathBuf::from(path_str);
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)?;
    let mime = match ext.as_str() {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    };
    let metadata = std::fs::metadata(&path).ok()?;
    if metadata.len() > 4 * 1024 * 1024 {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    Some(format!(
        "data:{mime};base64,{}",
        pig_utils::image::base64_encode(&bytes)
    ))
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
                Ok(mut theme) => {
                    if let Some(background) = theme.background.as_mut()
                        && let Some(data_uri) = inline_background_asset(&background.asset)
                    {
                        background.asset = data_uri;
                    }
                    result.themes.push(theme);
                }
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
