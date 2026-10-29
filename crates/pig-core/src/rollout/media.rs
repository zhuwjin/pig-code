use std::io::Write as _;
use std::path::{Path, PathBuf};

use pig_provider::{ChatDocument, ChatImage, DocumentTokenEstimate};
use serde::{Deserialize, Serialize};

/// Persisted reference to an image attached to a message; bytes live beside the rollout.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageRef {
    pub path: PathBuf,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Persisted reference to a PDF attached to a tool result; bytes live beside
/// the rollout (never in the JSONL).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentRef {
    pub path: PathBuf,
    pub media_type: String,
    /// Byte size of the persisted document (diagnostics; no content)
    #[serde(default)]
    pub bytes: u64,
    /// Token cost computed once when the document was persisted. A measured
    /// page-tree cost is window-independent; a window fallback records the
    /// window it came from so rehydration can reprice it for the model that
    /// will consume the document without scanning the bytes again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_estimate: Option<DocumentTokenEstimate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Media-file numbering continues across user attachments and tool results.
pub fn next_media_index(media_dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(media_dir) else {
        return 1;
    };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let stem = name.to_string_lossy().split('.').next()?.to_string();
            stem.parse::<usize>().ok()
        })
        .max()
        .unwrap_or(0)
        + 1
}

/// Rehydrate one persisted image. Every media read is rooted: the resolved
/// path must stay inside the owning session's media directory (a corrupt or
/// tampered rollout cannot point the model at an arbitrary file), and the bytes
/// must be decodable, match the recorded type, and stay under the pixel cap.
pub fn rehydrate_image(image_ref: &ImageRef, media_root: &Path) -> Option<ChatImage> {
    let bytes = read_media(&image_ref.path, media_root)?;
    let media_type = validate_image(&bytes, &image_ref.media_type)?;
    let label = image_ref.label.clone().or_else(|| {
        image_ref
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
    });
    Some(ChatImage {
        media_type,
        data_base64: crate::tool::base64_encode(&bytes),
        label,
    })
}

/// Image-file numbers used by the UI to locate thumbnails without embedding paths in clean text.
pub fn image_nums(image_refs: &[ImageRef]) -> Vec<u32> {
    image_refs
        .iter()
        .filter_map(|image_ref| image_ref.path.file_stem()?.to_str()?.parse().ok())
        .collect()
}

/// Rehydrate persisted PDF refs, validating the bytes on disk. Missing,
/// truncated, corrupt, or oversized files are skipped so the caller's
/// reference count produces the deterministic "no longer available" note
/// instead of sending invalid bytes to the provider.
///
/// A measured persisted cost is reused as-is. A persisted window fallback is
/// repriced from its tag for the current model window; only old refs without a
/// usable cache are decoded and scanned, once. Every result is carried in
/// [`ChatDocument::token_estimate`] so context estimation never touches the
/// bytes again.
pub fn rehydrate_documents(
    documents: &[DocumentRef],
    media_root: &Path,
    context_window: Option<u64>,
) -> Vec<ChatDocument> {
    documents
        .iter()
        .filter_map(|document| {
            if !document
                .media_type
                .eq_ignore_ascii_case(pig_utils::pdf::PDF_MEDIA_TYPE)
            {
                return None;
            }
            let bytes = read_media(&document.path, media_root)?;
            if bytes.len() as u64 > pig_utils::pdf::MAX_PDF_FILE_BYTES
                || !pig_utils::pdf::sniff_pdf(&bytes)
            {
                return None;
            }
            let token_estimate =
                cached_document_estimate(document.token_estimate, context_window, &bytes);
            let filename = document
                .label
                .clone()
                .or_else(|| {
                    document
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| "document.pdf".to_string());
            Some(ChatDocument {
                media_type: document.media_type.clone(),
                data_base64: crate::tool::base64_encode(&bytes),
                filename: filename.clone(),
                label: document.label.clone().or(Some(filename)),
                token_estimate,
            })
        })
        .collect()
}

/// Price one PDF for a given model window, preserving whether the cost was
/// measured or is a window-derived fallback.
pub(crate) fn document_token_estimate(bytes: &[u8], context_window: u64) -> DocumentTokenEstimate {
    match pig_utils::pdf::document_token_cost(bytes, context_window) {
        pig_utils::pdf::PdfTokenCost::Measured(tokens) => DocumentTokenEstimate::measured(tokens),
        pig_utils::pdf::PdfTokenCost::WindowFallback(tokens) => {
            DocumentTokenEstimate::window_fallback(tokens, context_window)
        }
    }
}

/// Reuse a persisted estimate when it is plausible. A window fallback is
/// repriced for the current window (a pure arithmetic update); a measured cost
/// is never recomputed. Without a current window the stored fallback keeps its
/// source window so a later model resolution can reprice it.
fn cached_document_estimate(
    stored: Option<DocumentTokenEstimate>,
    context_window: Option<u64>,
    bytes: &[u8],
) -> Option<DocumentTokenEstimate> {
    if let Some(cost) = stored
        && cost.tokens > 0
        && cost.tokens <= pig_utils::pdf::provider_document_token_ceiling()
    {
        return match (cost.window, context_window) {
            (Some(_), Some(window)) => Some(DocumentTokenEstimate::window_fallback(
                pig_utils::pdf::unknown_document_token_reserve(window),
                window,
            )),
            _ => Some(cost),
        };
    }
    context_window.map(|window| document_token_estimate(bytes, window))
}

/// Read a media file under the size cap, refusing every path outside the
/// owning session's media root. Both the root and the file are canonicalized,
/// so `..`, absolute refs and symlinks cannot escape it.
fn read_media(path: &Path, media_root: &Path) -> Option<Vec<u8>> {
    #[cfg(test)]
    note_media_read();
    let root = media_root.canonicalize().ok()?;
    let resolved = path.canonicalize().ok()?;
    if !resolved.starts_with(&root) {
        tracing::warn!(
            path = %path.display(),
            media_root = %root.display(),
            "refusing to rehydrate media outside the owning session media root"
        );
        return None;
    }
    let metadata = std::fs::metadata(&resolved).ok()?;
    if !metadata.is_file() || metadata.len() > pig_utils::image::MAX_MEDIA_FILE_BYTES {
        return None;
    }
    std::fs::read(&resolved).ok()
}

/// Header-level image validation: magic bytes must agree with the recorded
/// media type, dimensions must decode, and the pixel cap must hold. The
/// returned media type is the sniffed one (the recorded value is only trusted
/// after it matches).
fn validate_image(bytes: &[u8], recorded_type: &str) -> Option<String> {
    if bytes.len() as u64 > pig_utils::image::MAX_MEDIA_FILE_BYTES {
        return None;
    }
    let sniffed = pig_utils::image::sniff_image(bytes)?;
    if !sniffed.eq_ignore_ascii_case(recorded_type) {
        tracing::warn!(
            recorded = recorded_type,
            sniffed,
            "persisted image media type does not match its bytes; treating it as missing"
        );
        return None;
    }
    let (width, height) = pig_utils::image::image_dimensions(bytes)?;
    if width == 0
        || height == 0
        || width as u64 * height as u64 > pig_utils::image::MAX_MEDIA_PIXELS
    {
        return None;
    }
    Some(sniffed.to_string())
}

/// Persist PDF payloads beside the rollout and cache their token cost on both
/// the live message and the durable reference. `context_window` is the model
/// window that will consume these documents, so the one-time scan can apply
/// the window-bounded fallback when the page tree is not visible; the cached
/// value records which case it was, so a later window switch reprices only the
/// fallback.
pub fn persist_chat_documents(
    media_dir: &Path,
    documents: &mut [ChatDocument],
    context_window: u64,
) -> Result<Vec<DocumentRef>, String> {
    if documents.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(media_dir).map_err(|error| {
        format!(
            "Failed to create media directory {}: {error}",
            media_dir.display()
        )
    })?;
    let mut validated = Vec::with_capacity(documents.len());
    for document in documents.iter() {
        if !document
            .media_type
            .eq_ignore_ascii_case(pig_utils::pdf::PDF_MEDIA_TYPE)
        {
            return Err(format!(
                "Unsupported document media type \"{}\": only {} is accepted",
                document.media_type,
                pig_utils::pdf::PDF_MEDIA_TYPE
            ));
        }
        let bytes = pig_utils::image::base64_decode(&document.data_base64)?;
        if bytes.len() as u64 > pig_utils::pdf::MAX_PDF_FILE_BYTES
            || !pig_utils::pdf::sniff_pdf(&bytes)
        {
            return Err("refusing to persist a corrupt or oversized PDF document".to_string());
        }
        let tokens = document_token_estimate(&bytes, context_window);
        validated.push((bytes, tokens));
    }
    let mut refs = Vec::with_capacity(validated.len());
    for (document, (bytes, tokens)) in documents.iter_mut().zip(validated) {
        let path = create_media_file(media_dir, "pdf", &bytes)?;
        document.token_estimate = Some(tokens);
        refs.push(DocumentRef {
            path,
            media_type: document.media_type.clone(),
            bytes: bytes.len() as u64,
            token_estimate: Some(tokens),
            label: document
                .label
                .clone()
                .or_else(|| Some(document.filename.clone())),
        });
    }
    Ok(refs)
}

pub fn persist_chat_images(
    media_dir: &Path,
    images: &[ChatImage],
) -> Result<Vec<ImageRef>, String> {
    if images.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(media_dir).map_err(|error| {
        format!(
            "Failed to create media directory {}: {error}",
            media_dir.display()
        )
    })?;
    let mut validated = Vec::with_capacity(images.len());
    for image in images {
        let bytes = pig_utils::image::base64_decode(&image.data_base64)?;
        validated.push((image, bytes));
    }
    let mut refs = Vec::with_capacity(validated.len());
    for (image, bytes) in validated {
        let extension = match image.media_type.as_str() {
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "png",
        };
        let path = create_media_file(media_dir, extension, &bytes)?;
        let (width, height) = pig_utils::image::image_dimensions(&bytes).unwrap_or((0, 0));
        refs.push(ImageRef {
            path,
            media_type: image.media_type.clone(),
            width,
            height,
            label: image.label.clone(),
        });
    }
    Ok(refs)
}

// Test-only count of media reads on the current thread (structure-only
// rebuild passes must not read discarded media).
#[cfg(test)]
thread_local! {
    static MEDIA_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_media_read() {
    MEDIA_READS.with(|reads| reads.set(reads.get() + 1));
}

#[cfg(test)]
pub(crate) fn media_reads() -> usize {
    MEDIA_READS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_media_reads() {
    MEDIA_READS.with(|reads| reads.set(0));
}

/// Create a brand-new numbered media file and write it. `create_new` reserves
/// the name atomically: two concurrent writers (parallel tool calls) can never
/// overwrite each other, and a crash mid-write only leaves an unreferenced
/// partial file (the reference is written to the rollout only after this
/// returns). Partial files are rejected by the validation on rehydration.
fn create_media_file(media_dir: &Path, extension: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let mut next = next_media_index(media_dir);
    for _ in 0..10_000 {
        let path = media_dir.join(format!("{next}.{extension}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let result = file.write_all(bytes).and_then(|()| file.sync_all());
                if let Err(error) = result {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(format!(
                        "Failed to write media file {}: {error}",
                        path.display()
                    ));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                next += 1;
            }
            Err(error) => {
                return Err(format!(
                    "Failed to create media file {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Err(format!(
        "Failed to allocate a media file name in {}",
        media_dir.display()
    ))
}
