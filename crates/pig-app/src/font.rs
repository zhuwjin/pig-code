//! Font settings: applying config → global theme, and restoring.
//!
//! GPUI panics at first layout when a font family cannot be found
//! (`Font::fallbacks` only fills missing glyphs, not missing families), so font
//! names from config must first be validated against installed system fonts;
//! uninstalled ones fall back to defaults. Enumerating system fonts takes about
//! a hundred ms (macOS), cached once per process (matching the upstream
//! mono_font probing strategy).
//!
//! Light/dark switching does not reset fonts: upstream's
//! `Theme::change → apply_config` only overrides when the theme file explicitly
//! specifies `font.family`; built-in default themes lack that key.

use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::{App, Global, SharedString};
use pig_protocol::AppConfig;
use std::sync::OnceLock;

/// Default fonts recorded at startup (used when restoring "system defaults").
/// The monospace default follows the platform (macOS Menlo / Windows Consolas /
/// Linux DejaVu Sans Mono); upstream's init swaps in a fallback when missing, so
/// what is recorded here is the actually effective value, not the platform name.
#[derive(Clone)]
pub struct FontDefaults {
    pub ui: SharedString,
    pub mono: SharedString,
}

impl Global for FontDefaults {}

/// GPUI's virtual system font family (absent from installed font lists; allowed through validation)
const SYSTEM_UI_FONT: &str = ".SystemUIFont";

/// Installed font family names (all_font_names is sorted and deduped), cached per process
pub fn installed_font_names(cx: &App) -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| cx.text_system().all_font_names())
}

/// Record the current theme fonts as defaults (called once right after `gpui_kit::init`)
pub fn capture_defaults(cx: &mut App) {
    if !cx.has_global::<FontDefaults>() {
        let theme = cx.theme();
        cx.set_global(FontDefaults {
            ui: theme.font_family.clone(),
            mono: theme.mono_font_family.clone(),
        });
    }
}

/// Apply the config's font settings to the global theme; windows are refreshed
/// only when values change (idempotent; called once each at startup from
/// ConfigSnapshot and after settings-page confirmation, the latter re-flowing
/// once more when the save persists).
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

/// Config value → actual family name: None / not installed → default; the `.SystemUIFont` virtual family is allowed through
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
            tracing::warn!("font {name:?} not installed, keeping the default");
            default.clone()
        }
    }
}
