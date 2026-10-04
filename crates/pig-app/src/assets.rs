use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// pig 自带资产（供应商图标等），rust-embed 嵌入二进制；
/// debug 构建从源码目录读取，release 打进二进制
#[derive(rust_embed::RustEmbed)]
#[folder = "assets/"]
struct PigAssets;

/// 链式资产源：先查 pig 自带资产，未命中回落 gpui-kit 官方组件资产
///（Lucide 图标等）。替换 main.rs 原先直接挂 AllAssets 的位置
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

    /// 预设目录引用的全部图标资产必须可加载（rust-embed 键 = 相对 assets/ 的路径）；
    /// 新增预设改了文件名而漏拷文件时，这里先红
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
            assert!(PigAssets::get(path).is_some(), "缺少供应商图标资产: {path}");
        }
    }
}
