/// Compression note (kimi-code's caption idea): when an image is scaled/transcoded, a note is
/// appended to the text telling the model details may be lost; the original is persisted as
/// `{n}.orig.{ext}` so fine detail can be viewed via ReadMediaFile region cropping of the original.
/// Unchanged image (small-image passthrough) -> None, adding no noise to the text.
pub(crate) fn compression_note(
    ix: usize,
    pending: &pig_protocol::PendingImage,
    comp: &crate::tool::CompressedImage,
    media_dir: &std::path::Path,
    n: usize,
) -> Option<String> {
    let orig_mime = crate::tool::sniff_image(&pending.bytes).unwrap_or(pending.mime.as_str());
    let (ow, oh) = crate::tool::image_dimensions(&pending.bytes).unwrap_or((0, 0));
    if (ow, oh) == (comp.width, comp.height) && orig_mime == comp.media_type {
        return None;
    }
    let orig_ext = match orig_mime {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    let orig_file = media_dir.join(format!("{n}.orig.{orig_ext}"));
    let orig_hint = match std::fs::write(&orig_file, &pending.bytes) {
        Ok(()) => format!(
            "; the original was saved to {} — use ReadMediaFile with region on that path for fine detail (e.g. small text)",
            orig_file.display()
        ),
        Err(_) => "; the original was not kept".to_string(),
    };
    Some(format!(
        "\n[Image {ix} was compressed to fit model limits: original {ow}×{oh} {orig_mime} → sent {}×{} {} ({}KB); details may be lost{orig_hint}]",
        comp.width,
        comp.height,
        comp.media_type,
        comp.bytes.len() / 1024,
    ))
}
/// Capability projection (same as ZCode): when the model supports image input, images enter the
/// ChatMsg as-is; otherwise images are cleared and a placeholder is appended to the text.
/// media_paths matches chat_images in order and length (media files are persisted before
/// projection): the placeholder carries the paths, so the model knows images exist and where to read them.
pub fn project_images(
    text: &mut String,
    chat_images: &mut Vec<crate::provider::ChatImage>,
    media_paths: &[std::path::PathBuf],
    input_image: bool,
) {
    if !chat_images.is_empty() && !input_image {
        let paths = media_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "\n[{} image(s) were not sent with this message: the current model does not support image input; the files are at {paths} — use ReadMediaFile to view the one you need]",
            chat_images.len()
        ));
        chat_images.clear();
    }
}
