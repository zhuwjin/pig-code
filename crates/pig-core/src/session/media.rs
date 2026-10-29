use super::*;

/// ToolImage → ChatImage: the label comes from the path in the call arguments
/// (shared by the gated path and the parallel read-only segment).
pub(super) fn tool_images_to_chat(
    arguments: &str,
    images: Vec<tool::ToolImage>,
) -> Vec<pig_provider::ChatImage> {
    let label = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v["path"].as_str().map(str::to_string));
    images
        .into_iter()
        .map(|img| pig_provider::ChatImage {
            media_type: img.media_type,
            data_base64: img.data_base64,
            label: label.clone(),
        })
        .collect()
}

pub(super) fn tool_documents_to_chat(
    documents: Vec<tool::ToolDocument>,
) -> Vec<pig_provider::ChatDocument> {
    documents
        .into_iter()
        .map(|document| pig_provider::ChatDocument {
            media_type: document.media_type,
            data_base64: crate::tool::base64_encode(&document.bytes),
            filename: document.filename,
            label: document.label,
            // Filled by `persist_tool_documents` (root) or the child JSONL
            // writer before the message can enter a context estimate.
            token_estimate: None,
        })
        .collect()
}

/// Capability projection for a tool's media: unsupported kinds are removed
/// from the context and replaced by one deterministic English note naming the
/// source path, so content is never silently dropped.
pub(crate) fn project_tool_media(
    arguments: &str,
    output: &mut String,
    media: &mut tool::ToolOutputMedia,
    caps: tool::MediaCapabilities,
) {
    let path = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| value["path"].as_str().map(str::to_string))
        .unwrap_or_default();
    let location = if path.is_empty() {
        String::new()
    } else {
        format!("; the file is at {path}")
    };
    if !media.images.is_empty() && !caps.input_image {
        let count = media.images.len();
        media.images.clear();
        output.push_str(&format!(
            "\n\n[{count} image(s) were not sent with this message: the current model does not support image input{location} — use ReadMediaFile with a model that does]"
        ));
    }
    if !media.documents.is_empty() && !caps.input_pdf {
        let count = media.documents.len();
        media.documents.clear();
        output.push_str(&format!(
            "\n\n[{count} PDF document(s) were not sent with this message: the current model does not support PDF input{location} — use ReadMediaFile with a model that does]"
        ));
    }
}
