use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// pig's own assets (provider icons etc.), embedded into the binary by
/// rust-embed; debug builds read from the source directory, release builds pack
/// into the binary
#[derive(rust_embed::RustEmbed)]
#[folder = "assets/"]
struct PigAssets;

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
