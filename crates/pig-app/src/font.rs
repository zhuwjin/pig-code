//! 字体设置：配置 → 全局主题的应用与恢复。
//!
//! GPUI 对找不到的字体家族会在首次排版时 panic（`Font::fallbacks` 只补缺
//! 字形、不救缺家族），所以配置里的字体名必须先对系统已装字体校验，未安装
//! 的按默认处理。枚举系统字体约百毫秒（macOS），进程内缓存一次（对齐上游
//! mono_font 探测策略）。
//!
//! 亮/暗切换不重置字体：上游 `Theme::change → apply_config` 只在主题文件显式
//! 指定 `font.family` 时才覆盖，内置默认主题不含该键。

use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::{App, Global, SharedString};
use pig_protocol::AppConfig;
use std::sync::OnceLock;

/// 启动时记录的默认字体（恢复「系统默认」时用）。等宽默认随平台（macOS
/// Menlo / Windows Consolas / Linux DejaVu Sans Mono），上游 init 时若缺装
/// 会换成备选——这里记录的是实际生效值，不是平台名。
#[derive(Clone)]
pub struct FontDefaults {
    pub ui: SharedString,
    pub mono: SharedString,
}

impl Global for FontDefaults {}

/// GPUI 的虚拟系统字体家族（不出现在已装字体列表里，校验时放行）
const SYSTEM_UI_FONT: &str = ".SystemUIFont";

/// 已安装字体家族名（all_font_names 已排序去重），进程内缓存
pub fn installed_font_names(cx: &App) -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| cx.text_system().all_font_names())
}

/// 记录当前主题字体为默认值（`gpui_kit::init` 之后立即调用一次）
pub fn capture_defaults(cx: &mut App) {
    if !cx.has_global::<FontDefaults>() {
        let theme = cx.theme();
        cx.set_global(FontDefaults {
            ui: theme.font_family.clone(),
            mono: theme.mono_font_family.clone(),
        });
    }
}

/// 把配置里的字体设置应用到全局主题；值有变化才刷新窗口（幂等，
/// 启动 ConfigSnapshot 与设置页确认后各调一次，后者保存落盘还会再回流一次）。
pub fn apply_config_fonts(config: &AppConfig, cx: &mut App) {
    let Some(defaults) = cx.try_global::<FontDefaults>().cloned() else {
        return;
    };
    let installed = installed_font_names(cx);
    let ui = resolve_font(config.ui_font.as_deref(), installed, &defaults.ui);
    let mono = resolve_font(config.mono_font.as_deref(), installed, &defaults.mono);
    let theme = cx.global_mut::<Theme>();
    if theme.font_family == ui && theme.mono_font_family == mono {
        return;
    }
    theme.font_family = ui;
    theme.mono_font_family = mono;
    cx.refresh_windows();
}

/// 配置值 → 实际家族名：None / 未安装 → 默认；`.SystemUIFont` 虚拟家族放行
fn resolve_font(
    configured: Option<&str>,
    installed: &[String],
    default: &SharedString,
) -> SharedString {
    match configured {
        None => default.clone(),
        Some(name) if name == SYSTEM_UI_FONT || installed.iter().any(|n| n == name) => {
            SharedString::from(name.to_string())
        }
        Some(name) => {
            eprintln!("[font] 字体 {name:?} 未安装，沿用默认字体");
            default.clone()
        }
    }
}
