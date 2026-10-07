use super::*;

impl Composer {
    /// Paste entry (Textarea::on_paste): the arbitration result decides whether to
    /// intercept the default text insertion.
    /// Returning true = handled as an attachment, no text inserted into the
    /// composer; false = let the engine insert the text.
    pub(crate) fn handle_paste(
        &mut self,
        item: &ClipboardItem,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::clipboard::{PasteArb, arbitrate_clipboard};
        let entry_count = item.entries.len();
        match arbitrate_clipboard(item) {
            PasteArb::FilePath(path) => {
                // Over 20MB falls back to text paste (pasting the path as text);
                // unreadable/non-image files likewise fall back to text
                let Ok(meta) = std::fs::metadata(&path) else {
                    eprintln!("[clipboard] paste FilePath failed: metadata unavailable");
                    return false;
                };
                if meta.len() > 20 * 1024 * 1024 {
                    eprintln!(
                        "[clipboard] paste FilePath skipped: size={} exceeds 20MB",
                        meta.len()
                    );
                    return false;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    eprintln!("[clipboard] paste FilePath failed: read error");
                    return false;
                };
                let Some(mime) = pig_core::tool::sniff_image(&bytes) else {
                    eprintln!("[clipboard] paste FilePath skipped: unsupported file bytes");
                    return false;
                };
                eprintln!(
                    "[clipboard] paste FilePath accepted: mime={}, bytes={}, entries={}",
                    mime,
                    bytes.len(),
                    entry_count
                );
                self.attach_image(bytes, mime, cx);
                true
            }
            PasteArb::ImageBytes { bytes, mime } => {
                eprintln!(
                    "[clipboard] paste ImageBytes accepted: mime={}, bytes={}, entries={}",
                    mime,
                    bytes.len(),
                    entry_count
                );
                self.attach_image(bytes, mime, cx);
                true
            }
            PasteArb::Text => {
                eprintln!("[clipboard] paste Text: fallback to input text");
                false
            }
            PasteArb::Nothing => {
                eprintln!("[clipboard] paste Nothing: fallback to default paste");
                false
            }
        }
    }

    /// Add an image to the attachment list (chip strip): over the limit only shows a
    /// notice without attaching; TIFF is normalized to PNG here.
    pub(crate) fn attach_image(
        &mut self,
        mut bytes: Vec<u8>,
        mut mime: &str,
        cx: &mut Context<Self>,
    ) {
        if self.pasted_images.len() >= MAX_PASTED_IMAGES {
            eprintln!(
                "[clipboard] attach_image skipped: already at max={} images",
                MAX_PASTED_IMAGES
            );
            self.paste_note =
                Some(rust_i18n::t!("composer.paste_limit", n = MAX_PASTED_IMAGES).to_string());
            cx.notify();
            return;
        }
        if mime == "image/tiff" {
            let source_bytes = bytes.len();
            match pig_core::tool::convert_tiff_to_png(&bytes) {
                Ok((png, width, height)) => {
                    eprintln!(
                        "[clipboard] TIFF converted to PNG: {}x{}, bytes={} -> {}",
                        width,
                        height,
                        source_bytes,
                        png.len()
                    );
                    bytes = png;
                    mime = "image/png";
                }
                Err(error) => {
                    eprintln!("[clipboard] TIFF conversion failed: {error}");
                    self.paste_note =
                        Some(rust_i18n::t!("composer.tiff_failed", error = error).to_string());
                    cx.notify();
                    return;
                }
            }
        }
        self.paste_note = None;
        let byte_len = bytes.len();
        let (width, height) = pig_core::tool::image_dimensions(&bytes).unwrap_or((0, 0));
        self.pasted_images.push(PastedImage {
            bytes: std::sync::Arc::new(bytes),
            mime: mime.to_string(),
            width,
            height,
        });
        eprintln!(
            "[clipboard] attach_image stored: mime={}, bytes={}, dimensions={}x{}, count={}",
            mime,
            byte_len,
            width,
            height,
            self.pasted_images.len()
        );
        cx.notify();
    }

    /// Image attachment strip: the official AttachmentGroup (one Attachment per
    /// image: thumbnail + dimensions/size + hover delete button); the paste_note
    /// warning line follows the Group (style unchanged).
    /// `surface` is the surface color behind the row (the composer container color),
    /// used for the Group's edge fade.
    pub(crate) fn render_pasted_images(&self, surface: Hsla, cx: &mut Context<Self>) -> AnyElement {
        let attachments: Vec<AnyElement> = self
            .pasted_images
            .iter()
            .enumerate()
            .map(|(ix, image)| {
                let format = match image.mime.as_str() {
                    "image/jpeg" => ImageFormat::Jpeg,
                    "image/webp" => ImageFormat::Webp,
                    "image/gif" => ImageFormat::Gif,
                    _ => ImageFormat::Png,
                };
                let thumb = std::sync::Arc::new(gpui_kit::Image {
                    format,
                    bytes: (*image.bytes).clone(),
                    id: gpui_kit::hash(&(image.bytes.as_slice(), ix)),
                });
                let title = rust_i18n::t!("composer.image_label", n = ix + 1).to_string();
                let info = format!(
                    "{}×{} · {}KB",
                    image.width,
                    image.height,
                    image.bytes.len() / 1024
                );
                Attachment::new()
                    .id(("pasted-image", ix))
                    .media(AttachmentMedia::new().src(thumb))
                    .content(
                        AttachmentContent::new()
                            .title(AttachmentTitle::new(title.clone()))
                            .description(AttachmentDescription::new(info.clone())),
                    )
                    .tooltip(format!("{title}（{info}）"))
                    .on_remove(cx.listener(move |this, _, _, cx| {
                        this.pasted_images.remove(ix);
                        cx.notify();
                    }))
                    .axis(Axis::Horizontal)
                    .small()
                    .into_any_element()
            })
            .collect();
        v_flex()
            .w_full()
            .when(!attachments.is_empty(), |this| {
                this.child(
                    AttachmentGroup::new("pasted-images")
                        .with_edge_fade(surface)
                        .children(attachments),
                )
            })
            .when_some(self.paste_note.clone(), |this, note| {
                this.child(div().text_xs().text_color(cx.theme().warning).child(note))
            })
            .into_any_element()
    }
}

/// Clipboard image attachment (shown as a chip strip; converted to PendingImage on send)
pub(crate) struct PastedImage {
    pub(crate) bytes: std::sync::Arc<Vec<u8>>,
    pub(crate) mime: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Pasted image limit (same as ZCode)
pub(crate) const MAX_PASTED_IMAGES: usize = 8;
