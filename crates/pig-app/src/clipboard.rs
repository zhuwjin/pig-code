//! 剪贴板粘贴仲裁（composer 的 Textarea::on_paste 用）。
//!
//! gpui 的 ClipboardEntry 三变体：String / Image / ExternalPaths；
//! macOS 后端 read_from_clipboard 的返回顺序是 ExternalPaths → String → Image，
//! Image 条目的字节是编码后的文件字节（NSPasteboardTypePNG/TIFF 等原始数据）。

use gpui_kit::{ClipboardEntry, ClipboardItem, ImageFormat};

/// 粘贴仲裁结果：外部文件路径 / 图片字节 / 文本 / 空
pub enum PasteArb {
    /// ExternalPaths（Finder 复制的真实文件）：读字节嗅探后再决定 附件/路径文本
    FilePath(std::path::PathBuf),
    /// Image 条目：字节是编码后的文件字节
    ImageBytes {
        bytes: Vec<u8>,
        mime: &'static str,
    },
    /// 普通文本粘贴（引擎插文本的现有行为）
    Text,
    Nothing,
}

/// 剪贴板仲裁（kimi-code/ZCode 同款优先级）：
/// 1. ExternalPaths 优先（Finder 文件复制同时带 String 路径文本，文件语义更强）；
/// 2. 有实际内容的 String 优先于 Image（Excel 式多表示：含 tab/换行的表格文本）；
/// 3. Image 条目按管线支持的格式（png/jpeg/webp/gif/tiff）出附件，其余格式丢弃。
pub fn arbitrate_clipboard(item: &ClipboardItem) -> PasteArb {
    eprintln!(
        "[clipboard] received {} clipboard entr{}",
        item.entries.len(),
        if item.entries.len() == 1 { "y" } else { "ies" }
    );
    let mut paths = None;
    let mut string_text = None;
    let mut image = None;
    for (index, entry) in item.entries.iter().enumerate() {
        match entry {
            ClipboardEntry::ExternalPaths(external) => {
                eprintln!(
                    "[clipboard] entry[{index}] = ExternalPaths(count={})",
                    external.0.len()
                );
                paths = Some(external.0.clone());
            }
            ClipboardEntry::String(s) => {
                eprintln!(
                    "[clipboard] entry[{index}] = String(chars={}, non_whitespace={})",
                    s.text.chars().count(),
                    s.text.chars().any(|c| !c.is_whitespace())
                );
                string_text = Some(s.text.clone());
            }
            ClipboardEntry::Image(img) => {
                eprintln!(
                    "[clipboard] entry[{index}] = Image(format={}, mime={}, bytes={})",
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
        eprintln!(
            "[clipboard] arbitration = FilePath ({} path(s))",
            paths.len()
        );
        return PasteArb::FilePath(first.clone());
    }
    if let Some(text) = &string_text
        && text.chars().any(|c| !c.is_whitespace())
    {
        eprintln!("[clipboard] arbitration = Text");
        return PasteArb::Text;
    }
    if let Some(img) = image {
        let mime = match img.format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Gif => "image/gif",
            ImageFormat::Tiff => "image/tiff",
            // Svg/Bmp/Ico/Pnm 不在粘贴压缩管线内：丢弃（引擎插入空文本，无行为变化）
            _ => {
                eprintln!(
                    "[clipboard] arbitration = Nothing (unsupported image format={})",
                    image_format_name(img.format)
                );
                return PasteArb::Nothing;
            }
        };
        eprintln!(
            "[clipboard] arbitration = ImageBytes(mime={}, bytes={})",
            mime,
            img.bytes.len()
        );
        return PasteArb::ImageBytes {
            bytes: img.bytes.clone(),
            mime,
        };
    }
    eprintln!("[clipboard] arbitration = Nothing");
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
        // 纯文本 → 普通粘贴
        assert!(matches!(
            arbitrate_clipboard(&ClipboardItem::new_string("hello".into())),
            PasteArb::Text
        ));
        // 图片 → 附件（mime 按 gpui 的格式标记，不信扩展名）
        match arbitrate_clipboard(&ClipboardItem {
            entries: vec![image_entry(ImageFormat::Png)],
        }) {
            PasteArb::ImageBytes { mime, bytes } => {
                assert_eq!(mime, "image/png");
                assert_eq!(bytes, vec![0x89, 0x50]);
            }
            _ => panic!("图片应为附件"),
        }
        // Excel 式多表示：有实际内容的文本优先于图片
        let both = ClipboardItem {
            entries: vec![image_entry(ImageFormat::Png), string_entry("a\tb\nc")],
        };
        assert!(
            matches!(arbitrate_clipboard(&both), PasteArb::Text),
            "表格文本优先于图片"
        );
        // 空白文本不算实际内容：仍取图片
        let blank = ClipboardItem {
            entries: vec![image_entry(ImageFormat::Png), string_entry("   ")],
        };
        assert!(matches!(
            arbitrate_clipboard(&blank),
            PasteArb::ImageBytes { .. }
        ));
        // ExternalPaths（Finder 文件）最优先——即使带路径文本
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
            _ => panic!("文件路径优先"),
        }
        // 管线外的图片格式（Svg 等）丢弃
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
        // 空剪贴板
        assert!(matches!(
            arbitrate_clipboard(&ClipboardItem { entries: vec![] }),
            PasteArb::Nothing
        ));
    }
}
