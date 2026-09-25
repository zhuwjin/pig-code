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
/// 3. Image 条目按管线支持的格式（png/jpeg/webp/gif）出附件，其余格式丢弃。
pub fn arbitrate_clipboard(item: &ClipboardItem) -> PasteArb {
    let mut paths = None;
    let mut string_text = None;
    let mut image = None;
    for entry in &item.entries {
        match entry {
            ClipboardEntry::ExternalPaths(external) => paths = Some(external.0.clone()),
            ClipboardEntry::String(s) => string_text = Some(s.text.clone()),
            ClipboardEntry::Image(img) => image = Some(img),
        }
    }
    if let Some(paths) = paths
        && let Some(first) = paths.first()
    {
        return PasteArb::FilePath(first.clone());
    }
    if let Some(text) = &string_text
        && text.chars().any(|c| !c.is_whitespace())
    {
        return PasteArb::Text;
    }
    if let Some(img) = image {
        let mime = match img.format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Gif => "image/gif",
            // Svg/Bmp/Tiff/Ico 不在压缩管线内：丢弃（引擎插入空文本，无行为变化）
            _ => return PasteArb::Nothing,
        };
        return PasteArb::ImageBytes {
            bytes: img.bytes.clone(),
            mime,
        };
    }
    PasteArb::Nothing
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
        // 管线外的图片格式（Svg/Tiff 等）丢弃
        assert!(matches!(
            arbitrate_clipboard(&ClipboardItem {
                entries: vec![image_entry(ImageFormat::Tiff)]
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
