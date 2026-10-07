//! GUI i18n: locale resolution and application (the registry macro lives in
//! main.rs; string keys live in crates/pig-app/locales/).
//!
//! Same mechanism as gpui-component (rust-i18n, process-wide global current
//! language): one `set_locale` call flips both the gpui components' built-in
//! strings and this registry's strings at once. fallback = en (missing
//! translations show the English fallback).

use gpui_kit::*;
use pig_protocol::AppConfig;

/// Supported languages (locale id, dropdown display name, matching system-locale
/// prefixes — display names are deliberately written in each language itself,
/// independent of the UI language; system-locale detection picks the first
/// entry whose prefix matches). Adding a language = adding one row here plus
/// its translations in locales/.
pub const SUPPORTED: &[(&str, &str, &[&str])] =
    &[("zh-CN", "简体中文", &["zh"]), ("en", "English", &["en"])];

/// The fallback primary language (unsupported configured values and
/// undetectable system locales land here)
pub const FALLBACK: &str = "en";

/// Resolve the effective locale: explicit config wins; None = follow system
/// (table-driven: the first SUPPORTED entry whose prefix matches the system
/// locale, e.g. zh* → zh-CN). Unsupported/unrecognized configured values fall
/// back to the fallback primary language.
pub fn effective_locale(configured: Option<&str>) -> String {
    match configured {
        Some(lang) if SUPPORTED.iter().any(|(id, ..)| *id == lang) => lang.to_string(),
        Some(_) => FALLBACK.to_string(),
        None => sys_locale::get_locale()
            .as_deref()
            .and_then(|sys| {
                SUPPORTED
                    .iter()
                    .find(|(_, _, prefixes)| prefixes.iter().any(|p| sys.starts_with(p)))
                    .map(|(id, ..)| id.to_string())
            })
            .unwrap_or_else(|| FALLBACK.to_string()),
    }
}

/// Language dropdown options ("follow system" itself changes with the UI
/// language; language names are written in each language itself, fixed)
pub fn language_options() -> Vec<String> {
    let mut options = vec![rust_i18n::t!("common.follow_system").to_string()];
    options.extend(SUPPORTED.iter().map(|(_, label, ..)| label.to_string()));
    options
}

/// config.language → dropdown option label (unrecognized values display as
/// follow system)
pub fn language_label(configured: Option<&str>) -> String {
    match configured {
        Some(lang) => SUPPORTED
            .iter()
            .find(|(id, ..)| id == &lang)
            .map(|(_, label, ..)| label.to_string())
            .unwrap_or_else(|| rust_i18n::t!("common.follow_system").to_string()),
        None => rust_i18n::t!("common.follow_system").to_string(),
    }
}

/// Dropdown option label → config.language (follow system → None;
/// unrecognizable → unchanged, returning None for the caller to ignore)
pub fn language_value_for(label: &str) -> Option<Option<String>> {
    if label == rust_i18n::t!("common.follow_system").as_ref() {
        return Some(None);
    }
    SUPPORTED
        .iter()
        .find(|(_, l, ..)| *l == label)
        .map(|(id, ..)| Some(id.to_string()))
}

/// Apply the config's language globally (called once each on startup
/// ConfigSnapshot and after settings confirmation; the save round-trip calls it
/// again — idempotent, windows are not refreshed when the effective locale is
/// unchanged)
pub fn apply_config_language(config: &AppConfig, cx: &mut App) {
    let target = effective_locale(config.language.as_deref());
    let current: String = rust_i18n::locale().to_string();
    if current != target {
        rust_i18n::set_locale(&target);
        cx.refresh_windows();
    }
}

#[cfg(test)]
mod tests {
    // rust-i18n's process-current locale defaults to the hardcoded "en" (fallback
    // only bottoms out on missing keys): the test process does not go through
    // main()/apply_config_language, and without pinning, all Chinese assertions
    // fail. Pin zh-CN uniformly before process start (ctor stays in dev-deps
    // only; same approach as pig-core integration tests).
    #[ctor::ctor(unsafe)]
    fn pin_test_locale_zh_cn() {
        rust_i18n::set_locale("zh-CN");
    }

    #[test]
    fn effective_locale_resolution() {
        assert_eq!(super::effective_locale(Some("zh-CN")), "zh-CN");
        assert_eq!(super::effective_locale(Some("en")), "en");
        // Unsupported configured values fall back to the product's fallback
        // primary language
        assert_eq!(super::effective_locale(Some("fr")), "en");
        // Follow system: a local zh environment → zh-CN, otherwise en (both are
        // legal; only assert no panic and being supported)
        let auto = super::effective_locale(None);
        assert!(super::SUPPORTED.iter().any(|(id, ..)| *id == auto));
    }

    /// Switching smoke test: the same key fetches its own string under each of
    /// the two locales (the registry genuinely takes effect), then resets to
    /// zh-CN right after the assertion (the process-global locale is shared with
    /// other test threads; not resetting would pollute them). Note t! only looks
    /// up this crate's registry — core is language-neutral with no core.* keys
    /// (the errors.* strings for structured errors are covered by errors.rs
    /// tests).
    #[test]
    fn locale_switch_changes_lookup() {
        for (locale, needle) in [("zh-CN", "向 pig-code 提问"), ("en", "Ask pig-code")] {
            rust_i18n::set_locale(locale);
            assert!(
                rust_i18n::t!("composer.placeholder_idle").contains(needle),
                "placeholder should read {needle} in {locale}"
            );
        }
        rust_i18n::set_locale("zh-CN");
        assert_eq!(rust_i18n::t!("settings.language.label"), "语言");
        rust_i18n::set_locale("en");
        assert_eq!(rust_i18n::t!("settings.language.label"), "Language");
        rust_i18n::set_locale("zh-CN");
    }

    /// Bilingual completeness of locales/*.yml: each key's locale set must be
    /// exactly {zh-CN, en} (a missing en translation would silently drop the
    /// English UI back to the Chinese fallback — this goes red first). Simple
    /// indentation parsing: our yml follows a strict hand-written format (2-space
    /// indent, no block scalars, `locale: value` leaf lines), so we add no
    /// serde_yaml dependency (same convention as the hand-written parsing in
    /// skills.rs).
    fn yml_leaf_locales(content: &str) -> std::collections::HashMap<String, Vec<String>> {
        const LOCALES: [&str; 2] = ["zh-CN", "en"];
        let mut grouped: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut stack: Vec<String> = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            let level = indent / 2;
            let Some((key, _)) = trimmed.split_once(':') else {
                continue;
            };
            let key = key.trim();
            stack.truncate(level);
            if LOCALES.contains(&key) {
                grouped
                    .entry(stack.join("."))
                    .or_default()
                    .push(key.to_string());
                continue;
            }
            stack.push(key.to_string());
        }
        grouped
    }

    fn locale_files_have_both_languages_in(dir: &std::path::Path) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "yml"))
            .collect();
        entries.sort();
        assert!(!entries.is_empty(), "{} has no yml files", dir.display());
        for path in entries {
            let content = std::fs::read_to_string(&path).expect("read yml");
            let grouped = yml_leaf_locales(&content);
            assert!(
                !grouped.is_empty(),
                "{} has no translation keys",
                path.display()
            );
            for (key, mut locales) in grouped {
                locales.sort_unstable();
                assert_eq!(
                    locales,
                    ["en", "zh-CN"],
                    "{} key {key} should have locale set {{en, zh-CN}}, got {locales:?}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn locale_files_have_both_languages() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        locale_files_have_both_languages_in(&manifest.join("locales"));
    }

    /// Keys referenced by t!("literal") in code must actually exist in the
    /// corresponding registry (rust-i18n does no compile-time key checking, and a
    /// typo would silently fall back — this goes red first). Dynamic keys
    /// (t!(format!(...))) cannot be parsed statically and are skipped
    /// (approval.danger.* is covered by the guard in composer/tests.rs).
    fn t_keys_in_sources(src_dir: &std::path::Path) -> Vec<String> {
        let mut keys = Vec::new();
        let mut stack = vec![src_dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let content = std::fs::read_to_string(&path).expect("read rs");
                let bytes = content.as_bytes();
                let mut pos = 0;
                while let Some(found) = content[pos..].find("t!(") {
                    let start = pos + found + 3;
                    // Whole-line comments (examples inside doc comments) do not
                    // count as references
                    let line_start = content[..pos + found].rfind('\n').map_or(0, |n| n + 1);
                    if content[line_start..pos + found]
                        .trim_start()
                        .starts_with("//")
                    {
                        pos = start;
                        continue;
                    }
                    // Skip whitespace (including newlines — multi-line t! calls)
                    let mut ix = start;
                    while ix < bytes.len() && bytes[ix].is_ascii_whitespace() {
                        ix += 1;
                    }
                    if ix >= bytes.len() || bytes[ix] != b'"' {
                        pos = start;
                        continue; // Dynamic key (format! etc.), skip
                    }
                    let rest = &content[ix + 1..];
                    let Some(end) = rest.find('"') else {
                        break;
                    };
                    let key = &rest[..end];
                    // t! keys look like a.b.c; exclude strings in non-key form
                    // (containing spaces/%)
                    if !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                    {
                        keys.push(key.to_string());
                    }
                    pos = ix + 1 + end;
                }
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }

    fn yml_keys_in(dir: &std::path::Path) -> std::collections::HashSet<String> {
        let mut keys = std::collections::HashSet::new();
        for entry in std::fs::read_dir(dir).expect("read locales dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().is_some_and(|ext| ext == "yml") {
                let content = std::fs::read_to_string(&path).expect("read yml");
                keys.extend(yml_leaf_locales(&content).into_keys());
            }
        }
        keys
    }

    #[test]
    fn t_macro_keys_exist_in_registries() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // pig-app code → pig-app registry
        let app_keys = t_keys_in_sources(&manifest.join("src"));
        let app_registry = yml_keys_in(&manifest.join("locales"));
        let missing: Vec<_> = app_keys
            .iter()
            .filter(|k| !app_registry.contains(*k))
            .collect();
        assert!(
            missing.is_empty(),
            "pig-app code references keys missing from the registry: {missing:?}"
        );
    }

    /// Architecture guard (red line): pig-core is language-neutral; rust_i18n is
    /// banned from the whole tree — string localization must happen only at
    /// pig-app render points. Substring scan (including mentions in comments/doc
    /// comments).
    #[test]
    fn pig_core_has_zero_rust_i18n_references() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let core_src = manifest.join("../pig-core/src");
        let mut stack = vec![core_src];
        let mut offenders = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read pig-core src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let content = std::fs::read_to_string(&path).expect("read rs");
                if content.contains("rust_i18n") || content.contains("rust-i18n") {
                    offenders.push(path.display().to_string());
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "pig-core must not reference rust_i18n (localization belongs at pig-app render points): {offenders:?}"
        );
    }
}
