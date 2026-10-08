use gpui_kit::{AssetSource, Image, ImageFormat, Result, SharedString};
use std::borrow::Cow;
use std::sync::Arc;

/// pig's own assets (provider icons etc.), embedded into the binary by
/// rust-embed; debug builds read from the source directory, release builds pack
/// into the binary
#[derive(rust_embed::RustEmbed)]
#[folder = "assets/"]
struct PigAssets;

/// The Win11-style colored folder icon (64px PNG; sourced from ZCode's
/// Apache-2.0 material-icons set — the same icon its "open with" spots use).
/// Used as the title-bar "open in file manager" icon on non-macOS and as the
/// pre-fetch fallback on macOS (the real NSWorkspace Finder icon replaces it)
pub(crate) fn folder_icon() -> Option<Arc<Image>> {
    let file = PigAssets::get("icons/folder-win.png")?;
    Some(Arc::new(Image::from_bytes(
        ImageFormat::Png,
        file.data.to_vec(),
    )))
}

/// Chained asset source: look up pig's own assets first, falling back to
/// gpui-kit official component assets on a miss (Lucide icons etc.). Replaces
/// where main.rs previously attached AllAssets directly
pub(crate) struct ChainedAssets;

impl AssetSource for ChainedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(file) = PigAssets::get(path) {
            return Ok(Some(file.data));
        }
        gpui_kit::assets::AllAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut items: Vec<SharedString> = PigAssets::iter()
            .filter(|name| name.starts_with(path))
            .map(Into::into)
            .collect();
        items.extend(gpui_kit::assets::AllAssets.list(path)?);
        items.sort();
        items.dedup();
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded Win11-style folder icon must be a plausible PNG (loaded at
    /// startup as the "open in file manager" button icon on non-macOS; sourced
    /// from ZCode's Apache-2.0 material-icons set, rasterized to 64px)
    #[test]
    fn folder_icon_png_loadable() {
        let file = PigAssets::get("icons/folder-win.png").expect("icons/folder-win.png embedded");
        let bytes = file.data.as_ref();
        assert!(bytes.len() > 500, "folder icon PNG suspiciously small");
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
    }

    /// All icon assets referenced by the preset directory must be loadable
    /// (rust-embed key = path relative to assets/); when a new preset changes a
    /// file name but the file is not copied, this goes red first
    #[test]
    fn provider_icons_loadable() {
        for path in [
            "provider-icons/bigmodel.svg",
            "provider-icons/zai.png",
            "provider-icons/kimi.png",
            "provider-icons/minimax.png",
            "provider-icons/deepseek.png",
            "provider-icons/alibaba.png",
            "provider-icons/mimo.png",
            "provider-icons/anthropic.png",
            "provider-icons/openai.png",
            "provider-icons/xai.png",
            "provider-icons/openrouter-light.svg",
            "provider-icons/openrouter-dark.svg",
        ] {
            assert!(
                PigAssets::get(path).is_some(),
                "missing provider icon asset: {path}"
            );
        }
    }
}
