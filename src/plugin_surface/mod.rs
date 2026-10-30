use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

pub const COMPONENT_NAME: &str = "pig/plugin-surface";

#[derive(Default)]
struct Props {
    package: Option<PathBuf>,
    root: Option<PathBuf>,
    slot: Option<String>,
    theme: Option<String>,
}

impl Props {
    fn set(&mut self, name: &str, value: Option<&JsonValue>) {
        match name {
            "package" => self.package = value.and_then(|value| value.as_str()).map(PathBuf::from),
            "root" => self.root = value.and_then(|value| value.as_str()).map(PathBuf::from),
            "slot" => self.slot = value.and_then(|value| value.as_str()).map(str::to_owned),
            "theme" => self.theme = value.and_then(|value| value.as_str()).map(str::to_owned),
            _ => {}
        }
    }

    fn ready(&self) -> Option<(&Path, &Path, &str)> {
        Some((
            self.package.as_deref()?,
            self.root.as_deref()?,
            self.slot.as_deref()?,
        ))
    }

    fn theme(&self) -> Option<&str> {
        self.theme.as_deref()
    }
}
