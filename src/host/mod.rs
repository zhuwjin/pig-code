#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::run;
#[cfg(target_os = "macos")]
pub use macos::run;
#[cfg(windows)]
pub use windows::run;

pub const DEFAULT_BUNDLE_PATH: &str = ".nsh/out/default/bundle.js";

pub fn bundle_path() -> std::path::PathBuf {
    std::env::var_os("PIG_BUNDLE")
        .map(Into::into)
        .unwrap_or_else(|| DEFAULT_BUNDLE_PATH.into())
}

pub fn asset_base() -> std::path::PathBuf {
    bundle_path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new(".nsh/out"))
        .to_path_buf()
}

pub fn params() -> serde_json::Map<String, nsh_engine::JsonValue> {
    serde_json::Map::from_iter([
        ("executablePlugins".into(), true.into()),
        (
            "pluginPackage".into(),
            ".nsh/out/com.pigcodets.plugins.petal-1.nshpkg".into(),
        ),
        ("pluginRoot".into(), ".nsh/dev-keys/root.pub".into()),
    ])
}
