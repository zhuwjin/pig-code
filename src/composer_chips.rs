use std::sync::Arc;

use nsh_base::{
    rich_editor::{
        extensions::{Extension, RichExtensionRegistry, suggestion::mention_node},
        model::{Node, NodeSpec},
        state::{Plugin, PluginSpec, ViewFactory},
        view::{NodeView, NodeViewCx},
    },
    theme,
};
use nsh_core::{element::ImageSrc, prelude::*};
use serde_json::Value;

#[derive(Clone, Copy)]
enum ChipKind {
    File,
    Image,
    Command,
}

impl ChipKind {
    fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Image => "image",
            Self::Command => "command",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::File => include_str!("../assets/icons/file-text.svg"),
            Self::Image => include_str!("../assets/icons/image.svg"),
            Self::Command => include_str!("../assets/icons/square-terminal.svg"),
        }
    }

    fn schema(self) -> NodeSpec {
        let trigger = match self {
            Self::File => "@",
            Self::Image | Self::Command => "",
        };
        mention_node(self.name(), trigger).1
    }
}

struct ChipNodeView {
    kind: ChipKind,
}

impl NodeView for ChipNodeView {
    fn create(&self, _cx: &NodeViewCx, node: &Node) -> AnyElement {
        let label = node
            .attrs()
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("");
        let label = match self.kind {
            ChipKind::File => format!("@{label}"),
            ChipKind::Image | ChipKind::Command => label.to_owned(),
        };
        let chip_id = format!("composer-chip-{}", self.kind.name());
        let icon_id = format!("composer-chip-icon-{}", self.kind.name());
        view()
            .test_id(chip_id.as_str())
            .align_text_bottom()
            .translate_y(px(-2.0 / 3.0))
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(20.0))
            .px_1()
            .rounded(px(4.0))
            .bg(move || theme().accent)
            .text_color(move || theme().primary)
            .child(
                svg(ImageSrc::svg_markup(self.kind.icon()))
                    .test_id(icon_id.as_str())
                    .size(px(12.0))
                    .pointer_events(PointerEvents::None),
            )
            .child(text(label).text_sm())
            .into_element()
    }
}

struct ChipViews;

impl PluginSpec for ChipViews {
    fn node_views(&self) -> Vec<(String, ViewFactory)> {
        [ChipKind::File, ChipKind::Image, ChipKind::Command]
            .into_iter()
            .map(|kind| {
                let view: Arc<dyn NodeView> = Arc::new(ChipNodeView { kind });
                (kind.name().to_owned(), Arc::new(view) as ViewFactory)
            })
            .collect()
    }
}

pub fn register(registry: &mut Registry) {
    registry.rich_extension("composerChips", || {
        Extension::new("composerChips")
            .add_extensions(|_| {
                vec![
                    Extension::node("file", |_| ChipKind::File.schema()),
                    Extension::node("image", |_| ChipKind::Image.schema()),
                    Extension::node("command", |_| ChipKind::Command.schema()),
                    Extension::node("space", |_| mention_node("space", "").1),
                ]
            })
            .add_plugins(|_| vec![Arc::new(ChipViews) as Plugin])
    });
}
