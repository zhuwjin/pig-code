//! Installed font families, enumerated once for the settings pickers.

use std::sync::OnceLock;

fn names() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let families = match font_kit::source::SystemSource::new().all_families() {
            Ok(families) => families,
            Err(error) => {
                tracing::warn!("could not list installed fonts: {error}");
                Vec::new()
            }
        };
        let mut families: Vec<String> = families
            .into_iter()
            .filter(|name| {
                !name.is_empty()
                    && !name.starts_with('.')
                    && !name.eq_ignore_ascii_case("System Font")
            })
            .collect();
        families.sort_by_key(|name| name.to_lowercase());
        families.dedup();
        families
    })
}

/// Installed font family names, sorted case-insensitively and deduplicated.
pub fn installed_font_families() -> Vec<String> {
    names().to_vec()
}
