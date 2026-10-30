mod agent;
pub mod plugin_runtime;
pub mod plugin_surface;
mod theme;

use nsh_core::prelude::*;

/// Native exports available to TSX through `pig-code-ts/native`.
#[native_module("pig-code-ts/native")]
fn native(registry: &mut Registry) {
    registry.class::<agent::Agent>();
}
