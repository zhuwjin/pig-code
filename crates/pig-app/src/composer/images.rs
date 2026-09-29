use super::*;

impl Composer {
    /// 粘贴入口（Textarea::on_paste）：仲裁结果决定是否拦截默认文本插入。
    /// 返回 true = 已作为附件处理，输入框不插文本；false = 交给引擎插文本。
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
                // >20MB 跳过到文本粘贴（粘贴路径文本）；读不出/非图片同样落回文本
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


    /// 图片进附件列表（chip 条）：超上限只提示不附加；TIFF 在这里规范化为 PNG。
    pub(crate) fn attach_image(&mut self, mut bytes: Vec<u8>, mut mime: &str, cx: &mut Context<Self>) {
        if self.pasted_images.len() >= MAX_PASTED_IMAGES {
            eprintln!(
                "[clipboard] attach_image skipped: already at max={} images",
                MAX_PASTED_IMAGES
            );
            self.paste_note = Some(format!("最多粘贴 {MAX_PASTED_IMAGES} 张图片"));
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
                    self.paste_note = Some(format!("TIFF 图片无法转换：{error}"));
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


    /// 图片附件条：官方 AttachmentGroup（每图一个 Attachment：缩略图 + 尺寸/体积 +
    /// 悬停删除钮）；paste_note 警告行跟在 Group 之后（样式不变）。
    /// `surface` 是行背后的表面色（输入框容器色），用于 Group 的边缘渐隐。
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
                let title = format!("图片 {}", ix + 1);
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
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().warning)
                        .child(note),
                )
            })
            .into_any_element()
    }


}

/// 剪贴板图片附件（chip 条展示；发送时转 PendingImage 下发）
pub(crate) struct PastedImage {
    pub(crate) bytes: std::sync::Arc<Vec<u8>>,
    pub(crate) mime: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// 粘贴图片上限（ZCode 同款）
pub(crate) const MAX_PASTED_IMAGES: usize = 8;
