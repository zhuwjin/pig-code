mod agent;
mod code_highlight;
mod composer_chips;
mod plugin_system;
mod terminal;
mod theme;

use nsh_core::prelude::*;

/// Native exports available to TSX through `pig-code/native`.
#[native_module("pig-code/native")]
fn native(registry: &mut Registry) {
    registry.class::<agent::Agent>();
    registry.class::<code_highlight::CodeHighlighter>();
    registry.class::<terminal::Terminal>();
    composer_chips::register(registry);
}
