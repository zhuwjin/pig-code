use std::collections::BTreeMap;

use nsh_core::{EventKind, MountBatch, MountItem, ViewKind};
use pig_protocol::{RemoteDocument, RemoteNode};

pub const MAX_MOUNT_ITEMS: usize = 2048;
pub const MAX_REMOTE_NODES: usize = 4096;
pub const MAX_REMOTE_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;

pub fn project_batch(
    batch: MountBatch,
    instance: &str,
    epoch: u64,
    generation: u64,
    sequence: u64,
    previous: Option<&RemoteDocument>,
    resource_prefix: &str,
) -> Result<RemoteDocument, String> {
    if batch.items.len() > MAX_MOUNT_ITEMS {
        return Err(format!(
            "mount batch exceeds {MAX_MOUNT_ITEMS} operations"
        ));
    }
    let mut document = previous
        .filter(|document| document.epoch == epoch && document.generation == generation)
        .cloned()
        .unwrap_or_else(|| RemoteDocument {
            instance: instance.into(),
            epoch,
            generation,
            sequence: 0,
            base_revision: 0,
            nodes: vec![RemoteNode::new(nsh_core::ROOT_ID.to_string(), "view")],
            roots: Vec::new(),
            warnings: Vec::new(),
        });
    document.sequence = sequence;
    document.roots.clear();
    let mut nodes: BTreeMap<String, RemoteNode> = document
        .nodes
        .drain(..)
        .map(|node| (node.id.clone(), node))
        .collect();
    for item in batch.items {
        match item {
            MountItem::Create { id, kind } => {
                let kind = match kind {
                    ViewKind::View => "view",
                    ViewKind::Text => "text",
                    ViewKind::Scroll => "scroll",
                    ViewKind::Input => "input",
                    ViewKind::Image => "image",
                    other => return Err(format!("unsupported native view: {other:?}")),
                };
                if nodes.insert(id.to_string(), RemoteNode::new(id.to_string(), kind)).is_some() {
                    return Err(format!("duplicate remote node {id}"));
                }
            }
            MountItem::Insert {
                parent,
                child,
                before,
            } => {
                let child_id = child.to_string();
                if !nodes.contains_key(&child_id) {
                    return Err(format!("insert references unknown child {child}"));
                }
                for node in nodes.values_mut() {
                    node.children.retain(|id: &String| id != &child_id);
                }
                let parent = nodes
                    .get_mut(&parent.to_string())
                    .ok_or_else(|| format!("insert references unknown parent {parent}"))?;
                let index = match before {
                    Some(before) => parent
                        .children
                        .iter()
                        .position(|child| child == &before.to_string())
                        .ok_or_else(|| format!("insert references unknown sibling {before}"))?,
                    None => parent.children.len(),
                };
                parent.children.insert(index, child_id);
            }
            MountItem::Remove { id } => {
                let id = id.to_string();
                require_node(&nodes, id.clone())?;
                for node in nodes.values_mut() {
                    node.children.retain(|child| child != &id);
                }
            }
            MountItem::Delete { id } => {
                let id = id.to_string();
                require_node(&nodes, id.clone())?;
                nodes.remove(&id);
                for node in nodes.values_mut() {
                    node.children.retain(|child| child != &id);
                }
            }
            MountItem::Text { id, text, .. } => {
                let node = require_node_mut(&mut nodes, id)?;
                node.text = Some(text.text.clone());
                if let Some(span) = text.spans.first() {
                    let style = style_object(node);
                    style.insert("color".into(), color(span.style.color.0));
                    style.insert("fontSize".into(), span.style.font_size.0.into());
                    style.insert("fontWeight".into(), span.style.font_weight.into());
                    if span.style.line_height.0 > 0.0 {
                        style.insert("lineHeight".into(), span.style.line_height.0.into());
                    }
                }
            }
            MountItem::ViewStyle { id, style } => {
                let node = require_node_mut(&mut nodes, id)?;
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
            MountItem::ViewProps { id, props } => {
                let node = require_node_mut(&mut nodes, id)?;
                node.export_name = props.accessibility.label.as_deref().map(str::to_owned);
                node.test_id = props.test_id.as_deref().map(str::to_owned);
                node.props.insert("focusable".into(), props.focusable.into());
                node.props
                    .insert("selectable".into(), props.selectable_view.into());
                node.accessibility.insert(
                    "accessible".into(),
                    props.accessibility.accessible.into(),
                );
                node.accessibility.insert(
                    "accessibilityLabel".into(),
                    props.accessibility.label.as_deref().into(),
                );
                node.accessibility.insert(
                    "accessibilityHint".into(),
                    props.accessibility.hint.as_deref().into(),
                );
                node.accessibility.insert(
                    "role".into(),
                    format!("{:?}", props.accessibility.role).to_lowercase().into(),
                );
                node.accessibility.insert(
                    "accessibilityState".into(),
                    serde_json::json!({
                        "disabled": props.accessibility.state.disabled,
                        "selected": props.accessibility.state.selected,
                        "busy": props.accessibility.state.busy,
                        "expanded": props.accessibility.state.expanded,
                    }),
                );
                node.accessibility.insert(
                    "accessibilityElementsHidden".into(),
                    props.accessibility.hidden.into(),
                );
                if let Some(value) = &props.accessibility.value {
                    node.accessibility.insert(
                        "accessibilityValue".into(),
                        serde_json::json!({
                            "min": value.min,
                            "max": value.max,
                            "now": value.now,
                            "text": value.text.as_deref(),
                        }),
                    );
                }
                if !props.accessibility.actions.is_empty() {
                    node.accessibility.insert(
                        "accessibilityActions".into(),
                        serde_json::json!(props
                            .accessibility
                            .actions
                            .iter()
                            .map(|action| {
                                serde_json::json!({"name": action.name, "label": action.label})
                            })
                            .collect::<Vec<_>>()),
                    );
                }
            }
            MountItem::AccessibilityLabel { id, label } => {
                let node = require_node_mut(&mut nodes, id)?;
                node.export_name = label.as_deref().map(str::to_owned);
                node.accessibility
                    .insert("accessibilityLabel".into(), label.as_deref().into());
            }
            MountItem::Events { id, events } => {
                let node = require_node_mut(&mut nodes, id)?;
                node.events.clear();
                node.press = events.contains(EventKind::Press);
                for kind in EventKind::ALL {
                    if events.contains(*kind)
                        && let Some(name) = remote_event_name(*kind)
                    {
                        node.events.insert(name.to_owned());
                    }
                }
            }
            MountItem::Input { id, config } => {
                let node = require_node_mut(&mut nodes, id)?;
                node.props
                    .insert("placeholder".into(), config.placeholder.clone().into());
                node.props.insert("multiline".into(), config.multiline.into());
                node.props.insert("readonly".into(), (!config.editable).into());
                node.props.insert("autoFocus".into(), config.auto_focus.into());
                node.props.insert("maxLength".into(), config.max_length.into());
                node.props.insert(
                    "submitBehavior".into(),
                    match config.submit() {
                        nsh_core::SubmitBehavior::Submit => "submit",
                        nsh_core::SubmitBehavior::BlurAndSubmit => "blurAndSubmit",
                        nsh_core::SubmitBehavior::Newline => "newline",
                    }
                    .into(),
                );
                node.props.insert("autoCorrect".into(), config.auto_correct.into());
                node.props.insert(
                    "autoCapitalize".into(),
                    format!("{:?}", config.auto_capitalize).to_lowercase().into(),
                );
            }
            MountItem::InputValue { id, text, .. } => {
                require_node_mut(&mut nodes, id)?.text = Some(text);
            }
            MountItem::ScrollConfig {
                id,
                horizontal,
                scrollbar_visible,
                ..
            } => {
                let node = require_node_mut(&mut nodes, id)?;
                node.props.insert("horizontal".into(), horizontal.into());
                node.props
                    .insert("scrollbar".into(), scrollbar_visible.into());
            }
            MountItem::Image {
                id,
                source,
                resize_mode,
                ..
            } => {
                let node = require_node_mut(&mut nodes, id)?;
                node.source = source
                    .map(|source| resource_uri(&source.uri, resource_prefix))
                    .transpose()?;
                let object_fit = match resize_mode {
                    nsh_core::ResizeMode::Cover => "cover",
                    nsh_core::ResizeMode::Contain => "contain",
                    nsh_core::ResizeMode::Stretch => "fill",
                    nsh_core::ResizeMode::Center => "none",
                    nsh_core::ResizeMode::ScaleDown => "scale-down",
                };
                style_object(node).insert("objectFit".into(), object_fit.into());
            }
            MountItem::Component { name, .. } => {
                return Err(format!("native components are not allowed: {name}"));
            }
            MountItem::Command {
                command: nsh_core::Command::Tokens(_),
                ..
            } => {}
            MountItem::Command { .. } => {
                return Err("imperative native commands must use the remote UI protocol".into());
            }
            MountItem::Frame { .. }
            | MountItem::ScrollContent { .. }
            | MountItem::InputContent { .. }
            | MountItem::Overflow { .. }
            | MountItem::TrackStates { .. }
            | MountItem::LayoutAnimation(_)
            | MountItem::Animation(_)
            | MountItem::AnimationBindings { .. }
            | MountItem::Gestures { .. }
            | MountItem::ScrollBy { .. }
            | MountItem::LayoutDirection { .. }
            | MountItem::Appearance { .. }
            | MountItem::Keymap(_)
            | MountItem::StickyCells { .. }
            | MountItem::ScrollBehavior { .. }
            | MountItem::Refreshing { .. }
            | MountItem::Canvas { .. }
            | MountItem::ScreenConfig { .. }
            | MountItem::NativeProps { .. }
            | MountItem::ModalConfig { .. }
            | MountItem::ComponentPropSlots { .. }
            | MountItem::ComponentProps { .. }
            | MountItem::Shader { .. }
            | MountItem::ShaderTransition { .. }
            | MountItem::NavShader { .. }
            | MountItem::ShaderInput { .. } => {}
        }
    }
    if nodes.len() > MAX_REMOTE_NODES {
        return Err(format!("remote UI exceeds {MAX_REMOTE_NODES} nodes"));
    }
    for node in nodes.values() {
        for child in &node.children {
            if !nodes.contains_key(child) {
                return Err(format!("node {} references missing child {child}", node.id));
            }
        }
    }
    let attached: std::collections::BTreeSet<&str> = nodes
        .values()
        .flat_map(|node| node.children.iter().map(String::as_str))
        .collect();
    document.roots = nodes
        .keys()
        .filter(|id| !attached.contains(id.as_str()))
        .cloned()
        .collect();
    document.nodes = nodes.into_values().collect();
    validate_document(&document, resource_prefix)?;
    Ok(document)
}

pub fn validate_document(document: &RemoteDocument, resource_prefix: &str) -> Result<(), String> {
    if document.nodes.len() > MAX_REMOTE_NODES {
        return Err(format!("remote UI exceeds {MAX_REMOTE_NODES} nodes"));
    }
    if serde_json::to_vec(document).map_err(|error| error.to_string())?.len()
        > MAX_REMOTE_DOCUMENT_BYTES
    {
        return Err("remote UI document exceeds 2 MiB".into());
    }
    let mut nodes = BTreeMap::new();
    let mut parents = BTreeMap::new();
    for node in &document.nodes {
        if !matches!(node.kind.as_str(), "view" | "text" | "scroll" | "input" | "image" | "popover") {
            return Err(format!("unsupported remote node kind `{}`", node.kind));
        }
        if nodes.insert(node.id.as_str(), node).is_some() {
            return Err(format!("duplicate remote node {}", node.id));
        }
        if node.events.iter().any(|event| !supported_remote_event(event)) {
            return Err(format!("unsupported event on remote node {}", node.id));
        }
        if let Some(source) = &node.source {
            resource_uri(source, resource_prefix)?;
        }
        let mut children = std::collections::BTreeSet::new();
        for child in &node.children {
            if !children.insert(child.as_str()) {
                return Err(format!("remote node {} repeats a child", node.id));
            }
            if parents.insert(child.as_str(), node.id.as_str()).is_some() {
                return Err(format!("remote node {child} has multiple parents"));
            }
        }
    }
    for (child, parent) in &parents {
        if !nodes.contains_key(child) || !nodes.contains_key(parent) {
            return Err("remote UI references a missing parent or child".into());
        }
    }
    let roots: Vec<&str> = nodes
        .keys()
        .filter(|id| !parents.contains_key(*id))
        .copied()
        .collect();
    if document.roots.iter().map(String::as_str).collect::<Vec<_>>() != roots {
        return Err("remote UI roots do not match the document graph".into());
    }
    let mut visiting = std::collections::BTreeSet::new();
    let mut visited = std::collections::BTreeSet::new();
    for id in nodes.keys() {
        validate_acyclic(id, &nodes, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn supported_remote_event(event: &str) -> bool {
    matches!(
        event,
        "press" | "longPress" | "pressDown" | "pressUp" | "changeText" | "submit"
            | "focus" | "blur" | "selectionChange" | "keyPress" | "compositionChange"
            | "contentSizeChange" | "caretMove" | "scroll" | "scrollEnd" | "scrollDragStart"
            | "scrollDragEnd" | "momentumStart" | "momentumEnd" | "mouseMove" | "hoverIn"
            | "hoverOut" | "hoverMove" | "accessibilityAction" | "layout" | "openChange"
    )
}

fn validate_acyclic<'a>(
    id: &'a str,
    nodes: &BTreeMap<&'a str, &'a RemoteNode>,
    visiting: &mut std::collections::BTreeSet<&'a str>,
    visited: &mut std::collections::BTreeSet<&'a str>,
) -> Result<(), String> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(format!("remote UI contains a cycle at {id}"));
    }
    for child in &nodes[id].children {
        validate_acyclic(child, nodes, visiting, visited)?;
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

fn require_node(
    nodes: &BTreeMap<String, RemoteNode>,
    id: String,
) -> Result<(), String> {
    if nodes.contains_key(&id) {
        Ok(())
    } else {
        Err(format!("operation references unknown or deleted node {id}"))
    }
}

fn require_node_mut(
    nodes: &mut BTreeMap<String, RemoteNode>,
    id: impl ToString,
) -> Result<&mut RemoteNode, String> {
    let id = id.to_string();
    nodes
        .get_mut(&id)
        .ok_or_else(|| format!("operation references unknown or deleted node {id}"))
}

fn remote_event_name(kind: EventKind) -> Option<&'static str> {
    Some(match kind {
        EventKind::Press => "press",
        EventKind::LongPress => "longPress",
        EventKind::PressDown => "pressDown",
        EventKind::PressUp => "pressUp",
        EventKind::ChangeText => "changeText",
        EventKind::SubmitEditing => "submit",
        EventKind::Focus => "focus",
        EventKind::Blur => "blur",
        EventKind::SelectionChange => "selectionChange",
        EventKind::KeyPress => "keyPress",
        EventKind::CompositionChange => "compositionChange",
        EventKind::ContentSizeChange => "contentSizeChange",
        EventKind::CaretMove => "caretMove",
        EventKind::Scroll => "scroll",
        EventKind::ScrollEnd => "scrollEnd",
        EventKind::ScrollDragStart => "scrollDragStart",
        EventKind::ScrollDragEnd => "scrollDragEnd",
        EventKind::MomentumStart => "momentumStart",
        EventKind::MomentumEnd => "momentumEnd",
        EventKind::MouseMove => "mouseMove",
        EventKind::MouseEnter => "hoverIn",
        EventKind::MouseLeave => "hoverOut",
        EventKind::AccessibilityAction => "accessibilityAction",
        EventKind::Layout => "layout",
        _ => return None,
    })
}

fn resource_uri(uri: &str, prefix: &str) -> Result<String, String> {
    if let Some(path) = uri.strip_prefix(prefix) {
        if path.is_empty()
            || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err("invalid packaged image path".into());
        }
        return Ok(uri.to_owned());
    }
    if let Some(path) = uri.strip_prefix("asset:/") {
        if path.is_empty()
            || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err("invalid packaged image path".into());
        }
        return Ok(format!("{prefix}{path}"));
    }
    if uri.starts_with("data:image/") && uri.len() <= 1024 * 1024 {
        return Ok(uri.to_owned());
    }
    Err(format!("image source is not a signed package resource: {uri}"))
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
    fn projects_an_atomic_remote_tree_and_preserves_it_across_incremental_batches() {
        let batch = MountBatch {
            items: vec![
                MountItem::Create {
                    id: 2,
                    kind: ViewKind::View,
                },
                MountItem::Create {
                    id: 3,
                    kind: ViewKind::Text,
                },
                MountItem::Insert {
                    parent: nsh_core::ROOT_ID,
                    child: 2,
                    before: None,
                },
                MountItem::Insert {
                    parent: 2,
                    child: 3,
                    before: None,
                },
            ],
        };
        let document = project_batch(batch, "plugin", 1, 2, 1, None, "plugin://moon/digest/")
            .expect("project tree");
        assert_eq!(document.roots, ["1"]);
        assert_eq!(document.nodes[0].children, ["2"]);
        assert_eq!(document.sequence, 1);
        assert_eq!(document.generation, 2);

        let updated = project_batch(
            MountBatch {
                items: vec![MountItem::Create {
                    id: 4,
                    kind: ViewKind::Text,
                }],
            },
            "plugin",
            1,
            2,
            2,
            Some(&document),
            "plugin://moon/digest/",
        )
        .and_then(|document| {
            project_batch(
                MountBatch {
                    items: vec![MountItem::Insert {
                        parent: 2,
                        child: 4,
                        before: Some(3),
                    }],
                },
                "plugin",
                1,
                2,
                3,
                Some(&document),
                "plugin://moon/digest/",
            )
        })
        .expect("incremental insert retains existing parents and children");
        assert_eq!(updated.roots, ["1"]);
        assert_eq!(updated.nodes[1].children, ["4", "3"]);
        assert_eq!(updated.sequence, 3);
    }

    #[test]
    fn rejects_an_insert_before_create_instead_of_losing_the_node() {
        let batch = MountBatch {
            items: vec![MountItem::Insert {
                parent: 1,
                child: 2,
                before: None,
            }],
        };
        assert!(project_batch(batch, "plugin", 1, 1, 1, None, "plugin://moon/digest/").is_err());
    }
}
