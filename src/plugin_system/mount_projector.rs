use std::collections::{BTreeMap, BTreeSet};

use nsh_core::{EventKind, MountBatch, MountItem, NodeId, ViewKind};
use serde::Serialize;

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDocument {
    pub instance: String,
    pub epoch: u64,
    pub sequence: u64,
    pub nodes: Vec<RemoteNode>,
    pub roots: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteNode {
    pub id: String,
    pub kind: String,
    pub text: Option<String>,
    pub export_name: Option<String>,
    pub test_id: Option<String>,
    pub press: bool,
    pub children: Vec<String>,
    pub style: serde_json::Value,
}

impl RemoteNode {
    fn new(id: NodeId, kind: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: kind.into(),
            text: None,
            export_name: None,
            test_id: None,
            press: false,
            children: Vec::new(),
            style: serde_json::json!({}),
        }
    }
}

pub fn project_batch(
    batch: MountBatch,
    instance: &str,
    epoch: u64,
    sequence: u64,
    previous: &str,
) -> RemoteDocument {
    let mut documents: BTreeMap<String, RemoteDocument> =
        serde_json::from_str(previous).unwrap_or_default();
    let mut document = documents
        .remove(instance)
        .unwrap_or_else(|| RemoteDocument {
            instance: instance.into(),
            epoch,
            sequence: 0,
            nodes: Vec::new(),
            roots: Vec::new(),
            warnings: Vec::new(),
        });
    if document.epoch != epoch {
        document = RemoteDocument {
            instance: instance.into(),
            epoch,
            sequence: 0,
            nodes: Vec::new(),
            roots: Vec::new(),
            warnings: Vec::new(),
        };
    }
    document.sequence = sequence;
    let mut nodes: BTreeMap<String, RemoteNode> = document
        .nodes
        .drain(..)
        .map(|node| (node.id.clone(), node))
        .collect();
    let mut roots: BTreeSet<String> = document.roots.drain(..).collect();
    for item in batch.items {
        match item {
            MountItem::Create { id, kind } => {
                let kind = match kind {
                    ViewKind::View => "view",
                    ViewKind::Text => "text",
                    ViewKind::Scroll => "scroll",
                    ViewKind::Input => "input",
                    ViewKind::Image => "image",
                    other => {
                        document
                            .warnings
                            .push(format!("unsupported native view: {other:?}"));
                        "view"
                    }
                };
                nodes.insert(id.to_string(), RemoteNode::new(id, kind));
                roots.insert(id.to_string());
            }
            MountItem::Insert {
                parent,
                child,
                before,
            } => {
                let child_id = child.to_string();
                for node in nodes.values_mut() {
                    node.children.retain(|id| id != &child_id);
                }
                roots.remove(&child_id);
                if let Some(parent) = nodes.get_mut(&parent.to_string()) {
                    let index = before
                        .and_then(|id| {
                            parent
                                .children
                                .iter()
                                .position(|child| child == &id.to_string())
                        })
                        .unwrap_or(parent.children.len());
                    parent.children.insert(index, child_id);
                }
            }
            MountItem::Remove { id } => {
                let id = id.to_string();
                for node in nodes.values_mut() {
                    node.children.retain(|child| child != &id);
                }
                roots.insert(id);
            }
            MountItem::Delete { id } => {
                let id = id.to_string();
                nodes.remove(&id);
                roots.remove(&id);
                for node in nodes.values_mut() {
                    node.children.retain(|child| child != &id);
                }
            }
            MountItem::Text { id, text, .. } => {
                if let Some(node) = nodes.get_mut(&id.to_string()) {
                    node.text = Some(text.text.clone());
                    let style = style_object(node);
                    if let Some(span) = text.spans.first() {
                        style.insert("color".into(), color(span.style.color.0));
                        style.insert("fontSize".into(), span.style.font_size.0.into());
                        style.insert("fontWeight".into(), span.style.font_weight.into());
                        if span.style.line_height.0 > 0.0 {
                            style.insert("lineHeight".into(), span.style.line_height.0.into());
                        }
                    }
                }
            }
            MountItem::ViewStyle { id, style } => {
                if let Some(node) = nodes.get_mut(&id.to_string()) {
                    let object = style_object(node);
                    if style.background_color.0 != 0 {
                        object.insert("backgroundColor".into(), color(style.background_color.0));
                    }
                    object.insert("borderRadius".into(), style.border_radius.0.into());
                    object.insert("opacity".into(), style.opacity.into());
                    object.insert(
                        "display".into(),
                        if style.hidden { "none" } else { "flex" }.into(),
                    );
                }
            }
            MountItem::ViewProps { id, props } => {
                if let Some(node) = nodes.get_mut(&id.to_string()) {
                    node.export_name = props.accessibility.label.as_deref().map(str::to_owned);
                    node.test_id = props.test_id.as_deref().map(str::to_owned);
                }
            }
            MountItem::AccessibilityLabel { id, label } => {
                if let Some(node) = nodes.get_mut(&id.to_string()) {
                    node.export_name = label.as_deref().map(str::to_owned);
                }
            }
            MountItem::Events { id, events } => {
                if let Some(node) = nodes.get_mut(&id.to_string()) {
                    node.press = events.contains(EventKind::Press);
                }
            }
            MountItem::Frame { id, frame } => {
                if let Some(node) = nodes.get_mut(&id.to_string()) {
                    let style = style_object(node);
                    style.insert("width".into(), frame.size.width.0.into());
                    style.insert("height".into(), frame.size.height.0.into());
                }
            }
            MountItem::InputValue { id, text, .. } => {
                if let Some(node) = nodes.get_mut(&id.to_string()) {
                    node.text = Some(text);
                }
            }
            MountItem::Component { name, .. } => {
                document
                    .warnings
                    .push(format!("native components are not allowed: {name}"));
            }
            _ => {}
        }
    }
    let children: BTreeSet<String> = nodes
        .values()
        .flat_map(|node| node.children.iter().cloned())
        .collect();
    document.roots = nodes
        .keys()
        .filter(|id| !children.contains(*id))
        .cloned()
        .collect();
    document.nodes = nodes.into_values().collect();
    document
}

fn style_object(node: &mut RemoteNode) -> &mut serde_json::Map<String, serde_json::Value> {
    if !node.style.is_object() {
        node.style = serde_json::json!({});
    }
    node.style.as_object_mut().expect("remote style object")
}

fn color(value: u32) -> serde_json::Value {
    format!("#{value:08x}").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_core::{MountItem, ViewKind};

    #[test]
    fn projects_an_atomic_remote_tree() {
        let batch = MountBatch {
            items: vec![
                MountItem::Create {
                    id: 1,
                    kind: ViewKind::View,
                },
                MountItem::Create {
                    id: 2,
                    kind: ViewKind::Text,
                },
                MountItem::Insert {
                    parent: 1,
                    child: 2,
                    before: None,
                },
            ],
        };
        let document = project_batch(batch, "plugin", 1, 1, "{}");
        assert_eq!(document.roots, ["1"]);
        assert_eq!(document.nodes[0].children, ["2"]);
        assert_eq!(document.sequence, 1);
    }
}
