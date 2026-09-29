/// 压缩附注（kimi-code caption 思路）：图片被缩放/转码后附在文本里告知模型
/// 细节可能丢失；原图同时落盘 `{n}.orig.{ext}`，需要高清局部可用 ReadMediaFile
/// region 裁剪原图查看。图片未变（小图直通）→ None，不给文本加噪音。
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
            "；原图已存到 {}，需要看清细节（例如小字）可用 ReadMediaFile 对该路径用 region 裁剪查看",
            orig_file.display()
        ),
        Err(_) => "；原图未保留".to_string(),
    };
    Some(format!(
        "\n[图片 {ix} 已压缩以适应模型限制：原始 {ow}×{oh} {orig_mime} → 发送 {}×{} {}（{}KB），细节可能丢失{orig_hint}]",
        comp.width,
        comp.height,
        comp.media_type,
        comp.bytes.len() / 1024,
    ))
}
/// 能力投影（ZCode 同款）：模型支持图片输入时 images 原样进 ChatMsg；
/// 不支持时 images 清空、文本末尾追加占位。media_paths 与 chat_images 同序等长
///（媒体文件先于投影落盘）：占位带上路径，模型知道有图、知道去哪读。
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
            .join("、");
        text.push_str(&format!(
            "\n[图片 {} 张未随消息发送：当前模型不支持图片输入；文件在 {paths}，需要看哪张可用 ReadMediaFile 读取]",
            chat_images.len()
        ));
        chat_images.clear();
    }
}
