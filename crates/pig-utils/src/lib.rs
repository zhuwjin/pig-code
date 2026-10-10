//! Shared non-UI utilities used by both the engine (pig-core) and the GUI
//! (pig-app): the text decode/encode pipeline (UTF-8/UTF-16/GBK, line
//! endings), image sniffing/compression helpers (model-budget-friendly
//! encoding shared by the ReadMediaFile tool and paste sending), skill
//! discovery/parsing, and data-dir/workspace path conventions.
//!
//! Same rules as the other core-side crates: English-only, no gpui, no i18n
//! registry; errors are plain English strings (or pig-protocol's `CoreError`
//! where a structured kind is needed) and localization happens in pig-app at
//! render points. Pure utilities only — anything coupled to engine state
//! (session persistence, tool execution) stays in pig-core.

pub mod image;
pub mod paths;
pub mod skills;
pub mod text;

pub use paths::{data_dir, media_dir};
