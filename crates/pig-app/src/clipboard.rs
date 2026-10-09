//! Clipboard paste arbitration (used by the composer's Textarea::on_paste).
//!
//! gpui's ClipboardEntry has three variants: String / Image / ExternalPaths;
//! the macOS backend's read_from_clipboard returns them ordered ExternalPaths
//! → String → Image, and an Image entry's bytes are encoded file bytes (raw
//! NSPasteboardTypePNG/TIFF data and the like).

use gpui_kit::{ClipboardEntry, ClipboardItem, ImageFormat};

/// Paste arbitration result: external file path / image bytes / text / nothing
pub enum PasteArb {
    /// ExternalPaths (real files copied in Finder): sniff the bytes first, then decide attachment vs path text
    FilePath(std::path::PathBuf),
    /// Image entry: the bytes are encoded file bytes
    ImageBytes {
        bytes: Vec<u8>,
        mime: &'static str,
    },
    /// Plain text paste (the engine's existing behavior for inserting text)
    Text,
    Nothing,
}

/// Clipboard arbitration (same priorities as kimi-code/ZCode):
/// 1. ExternalPaths first (a Finder file copy also carries the path as String
///    text, but the file semantics are stronger);
/// 2. a String with real content beats Image (Excel-style multiple
///    representations: table text with tabs/newlines);
/// 3. an Image entry becomes an attachment in the formats the pipeline supports
///    (png/jpeg/webp/gif/tiff); other formats are dropped.
pub fn arbitrate_clipboard(item: &ClipboardItem) -> PasteArb {
    tracing::debug!(
        "received {} clipboard entr{}",
        item.entries.len(),
        if item.entries.len() == 1 { "y" } else { "ies" }
    );
    let mut paths = None;
    let mut string_text = None;
    let mut image = None;
    for (index, entry) in item.entries.iter().enumerate() {
        match entry {
            ClipboardEntry::ExternalPaths(external) => {
                tracing::debug!("entry[{index}] = ExternalPaths(count={})", external.0.len());
                paths = Some(external.0.clone());
            }
            ClipboardEntry::String(s) => {
                tracing::debug!(
                    "entry[{index}] = String(chars={}, non_whitespace={})",
                    s.text.chars().count(),
                    s.text.chars().any(|c| !c.is_whitespace())
                );
                string_text = Some(s.text.clone());
            }
            ClipboardEntry::Image(img) => {
                tracing::debug!(
                    "entry[{index}] = Image(format={}, mime={}, bytes={})",
                    image_format_name(img.format),
                    image_format_mime(img.format).unwrap_or("unsupported"),
                    img.bytes.len()
                );
                image = Some(img);
            }
        }
    }
    if let Some(paths) = paths
        && let Some(first) = paths.first()
    {
        tracing::debug!("arbitration = FilePath ({} path(s))", paths.len());
        return PasteArb::FilePath(first.clone());
    }
    if let Some(text) = &string_text
        && text.chars().any(|c| !c.is_whitespace())
    {
        tracing::debug!("arbitration = Text");
        return PasteArb::Text;
    }
    if let Some(img) = image {
        let mime = match img.format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Gif => "image/gif",
            ImageFormat::Tiff => "image/tiff",
            // Svg/Bmp/Ico/Pnm are not in the paste-compression pipeline: dropped (the engine inserts empty text; no behavior change)
            _ => {
                tracing::debug!(
                    "arbitration = Nothing (unsupported image format={})",
                    image_format_name(img.format)
                );
                return PasteArb::Nothing;
            }
        };
        tracing::debug!(
            "arbitration = ImageBytes(mime={}, bytes={})",
            mime,
            img.bytes.len()
        );
        return PasteArb::ImageBytes {
            bytes: img.bytes.clone(),
            mime,
        };
    }
    tracing::debug!("arbitration = Nothing");
    PasteArb::Nothing
}

fn image_format_name(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "Png",
        ImageFormat::Jpeg => "Jpeg",
        ImageFormat::Webp => "Webp",
        ImageFormat::Gif => "Gif",
        ImageFormat::Svg => "Svg",
        ImageFormat::Bmp => "Bmp",
        ImageFormat::Tiff => "Tiff",
        ImageFormat::Ico => "Ico",
        ImageFormat::Pnm => "Pnm",
    }
}

fn image_format_mime(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Webp => Some("image/webp"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::Svg => Some("image/svg+xml"),
        ImageFormat::Bmp => Some("image/bmp"),
        ImageFormat::Tiff => Some("image/tiff"),
        ImageFormat::Ico => Some("image/x-icon"),
        ImageFormat::Pnm => Some("image/x-portable-anymap"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{ClipboardString, ExternalPaths, Image};

    fn string_entry(text: &str) -> ClipboardEntry {
        ClipboardEntry::String(ClipboardString::new(text.to_string()))
    }

    fn image_entry(format: ImageFormat) -> ClipboardEntry {
        ClipboardEntry::Image(Image {
            format,
            bytes: vec![0x89, 0x50],
            id: 1,
        })
    }

    #[test]
    fn clipboard_arbitration_priorities() {
        // Plain text → normal paste
        assert!(matches!(
            arbitrate_clipboard(&ClipboardItem::new_string("hello".into())),
            PasteArb::Text
        ));
        // Image → attachment (mime from gpui's format tag, not trusting the extension)
        match arbitrate_clipboard(&ClipboardItem {
            entries: vec![image_entry(ImageFormat::Png)],
        }) {
            PasteArb::ImageBytes { mime, bytes } => {
                assert_eq!(mime, "image/png");
                assert_eq!(bytes, vec![0x89, 0x50]);
            }
            _ => panic!("image should be an attachment"),
        }
        // Excel-style multiple representations: text with real content beats the image
        let both = ClipboardItem {
            entries: vec![image_entry(ImageFormat::Png), string_entry("a\tb\nc")],
        };
        assert!(
            matches!(arbitrate_clipboard(&both), PasteArb::Text),
            "table text should take precedence over the image"
        );
        // Whitespace-only text does not count as real content: the image still wins
        let blank = ClipboardItem {
            entries: vec![image_entry(ImageFormat::Png), string_entry("   ")],
        };
        assert!(matches!(
            arbitrate_clipboard(&blank),
            PasteArb::ImageBytes { .. }
        ));
        // ExternalPaths (Finder files) wins first, even with path text alongside
        let files = ClipboardItem {
            entries: vec![
                string_entry("/tmp/x.png"),
                ClipboardEntry::ExternalPaths(ExternalPaths(
                    [std::path::PathBuf::from("/tmp/x.png")]
                        .into_iter()
                        .collect(),
                )),
            ],
        };
        match arbitrate_clipboard(&files) {
            PasteArb::FilePath(path) => assert_eq!(path, std::path::PathBuf::from("/tmp/x.png")),
            _ => panic!("file path should take precedence"),
        }
        // Image formats outside the pipeline (Svg etc.) are dropped
        assert!(matches!(
            arbitrate_clipboard(&ClipboardItem {
                entries: vec![image_entry(ImageFormat::Tiff)]
            }),
            PasteArb::ImageBytes { mime: "image/tiff", bytes } if bytes == vec![0x89, 0x50]
        ));
        assert!(matches!(
            arbitrate_clipboard(&ClipboardItem {
                entries: vec![image_entry(ImageFormat::Svg)]
            }),
            PasteArb::Nothing
        ));
        // Empty clipboard
        assert!(matches!(
            arbitrate_clipboard(&ClipboardItem { entries: vec![] }),
            PasteArb::Nothing
        ));
    }
}
