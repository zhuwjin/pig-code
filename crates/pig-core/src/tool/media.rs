use super::*;

/// 图片魔数嗅探（读文件头，不信任扩展名）：命中返回 mime，否则 None。
/// Read/ReadMediaFile 共用。
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// 手写 base64（标准 alphabet + `=` 填充；不加依赖，与快照 hex 编码同风格）
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 将 TIFF 原始字节无缩放转换为 PNG，供剪贴板粘贴入口规范化格式。
/// 先读取尺寸再解码，避免超大 TIFF 在解码阶段占用过多内存。
pub fn convert_tiff_to_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    if bytes.len() as u64 > MAX_MEDIA_FILE_BYTES {
        return Err(format!(
            "TIFF 文件超过 {}MB 上限",
            MAX_MEDIA_FILE_BYTES / 1024 / 1024
        ));
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("TIFF 解析失败: {e}"))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|e| format!("TIFF 尺寸读取失败: {e}"))?;
    if width == 0 || height == 0 {
        return Err("TIFF 尺寸无效".to_string());
    }
    if width as u64 * height as u64 > MAX_MEDIA_PIXELS {
        return Err(format!("TIFF 图片过大（{width}×{height}）"));
    }
    let image = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("TIFF 解析失败: {e}"))?
        .decode()
        .map_err(|e| format!("TIFF 解码失败: {e}"))?;
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|e| format!("TIFF 转 PNG 编码失败: {e}"))?;
    Ok((output.into_inner(), width, height))
}

/// 媒体文件的尺寸/体积上限（ReadMediaFile 与粘贴发送共用）
const MAX_MEDIA_FILE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_MEDIA_PIXELS: u64 = 100_000_000;
/// 默认缩放到最长边 2000（full_resolution=true 时不缩）
const MEDIA_MAX_EDGE: u32 = 2000;
/// PNG 输出超过 4MB 且无 alpha → 转 JPEG q85 兜底
const MAX_PNG_BYTES: usize = 4 * 1024 * 1024;

/// 压缩产物（进模型预算的图片）
pub struct CompressedImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}

/// 解码后图片的预算内编码：最长边 2000 等比缩放（小的不动），
/// 有 alpha 或源是 PNG/GIF/WebP → PNG；否则 JPEG q85；PNG 超 4MB 无 alpha → JPEG 兜底。
/// ReadMediaFile（region 裁剪后）与粘贴发送共用这一段。
pub fn encode_image_for_model(
    mut image: image::DynamicImage,
    source_mime: &str,
) -> Result<CompressedImage, String> {
    let (mut w, mut h) = (image.width(), image.height());
    if w.max(h) > MEDIA_MAX_EDGE {
        let scale = MEDIA_MAX_EDGE as f32 / w.max(h) as f32;
        let (nw, nh) = (
            (w as f32 * scale).round().max(1.) as u32,
            (h as f32 * scale).round().max(1.) as u32,
        );
        image = image.resize(nw, nh, image::imageops::FilterType::Triangle);
        w = nw;
        h = nh;
    }
    if w as u64 * h as u64 > MAX_MEDIA_PIXELS {
        return Err(format!("图片过大（{w}×{h}），请用 region 参数裁剪局部"));
    }
    let has_alpha = image.color().has_alpha();
    let prefer_png = has_alpha || matches!(source_mime, "image/png" | "image/gif" | "image/webp");
    let mut media_type = if prefer_png {
        "image/png"
    } else {
        "image/jpeg"
    };
    let mut encoded = if prefer_png {
        let mut buf = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut buf, image::ImageFormat::Png)
            .map_err(|e| format!("图片编码失败: {e}"))?;
        buf.into_inner()
    } else {
        encode_jpeg(&image)?
    };
    if prefer_png && encoded.len() > MAX_PNG_BYTES && !has_alpha {
        encoded = encode_jpeg(&image)?;
        media_type = "image/jpeg";
    }
    Ok(CompressedImage {
        bytes: encoded,
        media_type: media_type.to_string(),
        width: w,
        height: h,
    })
}

/// 原始字节 → 压缩产物（粘贴发送路径）：source_mime 为空串时魔数嗅探。
/// 尺寸预检（不解码大图）→ 解码 → encode_image_for_model。
pub fn compress_image_for_model(
    bytes: &[u8],
    source_mime: &str,
) -> Result<CompressedImage, String> {
    let sniffed = if source_mime.is_empty() {
        sniff_image(bytes).ok_or("不是可识别的图片（支持 PNG/JPEG/GIF/WebP）".to_string())?
    } else {
        source_mime
    };
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("图片解析失败: {e}"))?;
    let (w, h) = reader
        .into_dimensions()
        .map_err(|e| format!("图片解析失败: {e}"))?;
    if w as u64 * h as u64 > MAX_MEDIA_PIXELS {
        return Err(format!("图片过大（{w}×{h}）"));
    }
    let image = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("图片解析失败: {e}"))?
        .decode()
        .map_err(|e| format!("图片解码失败: {e}"))?;
    encode_image_for_model(image, sniffed)
}

pub(crate) struct ReadMediaFile;

impl Tool for ReadMediaFile {
    fn name(&self) -> &'static str {
        "ReadMediaFile"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "ReadMediaFile",
                "description": "读取图片文件进上下文（PNG/JPEG/GIF/WebP，魔数嗅探不信任扩展名）。默认等比缩放到最长边 2000 像素；region 可按原图坐标裁剪局部，full_resolution=true 不缩放。需要模型支持图片输入。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相对工作目录的图片路径" },
                        "region": {
                            "type": "object",
                            "description": "可选裁剪区域（原图像素坐标；越界自动夹紧，不相交报错）",
                            "properties": {
                                "x": { "type": "integer" },
                                "y": { "type": "integer" },
                                "width": { "type": "integer" },
                                "height": { "type": "integer" }
                            },
                            "required": ["x", "y", "width", "height"]
                        },
                        "full_resolution": { "type": "boolean", "description": "true 时不做 2000px 缩放（默认 false）" }
                    },
                    "required": ["path"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let path = args["path"].as_str().ok_or("缺少参数 path")?;
            let full = resolve_with_access(ctx.state, ctx.cwd, path, false, FsAccess::Read)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            let bytes = std::fs::read(&full).map_err(|e| read_io_error(path, &full, e))?;
            if bytes.len() as u64 > MAX_MEDIA_FILE_BYTES {
                return Err(format!(
                    "文件超过 100MB 上限（{}MB）",
                    bytes.len() / 1024 / 1024
                ));
            }
            let Some(source_mime) = sniff_image(&bytes) else {
                return Err("不是可识别的图片（支持 PNG/JPEG/GIF/WebP）；视频暂不支持".to_string());
            };
            // 先读尺寸再解码：总像素超限直接拒（防解码大图撑爆内存）
            let reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .map_err(|e| format!("图片解析失败: {e}"))?;
            let (orig_w, orig_h) = reader
                .into_dimensions()
                .map_err(|e| format!("图片解析失败: {e}"))?;
            if orig_w as u64 * orig_h as u64 > MAX_MEDIA_PIXELS {
                return Err(format!(
                    "图片过大（{orig_w}×{orig_h}），请用 region 参数裁剪局部"
                ));
            }
            let image = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .map_err(|e| format!("图片解析失败: {e}"))?
                .decode()
                .map_err(|e| format!("图片解码失败: {e}"))?;

            // region 裁剪（原图坐标；夹紧到图内，完全不相交报错）
            let mut crop_note = String::new();
            let image = if let Some(region) = args.get("region") {
                let (rx, ry, rw, rh) = (
                    region["x"].as_u64().unwrap_or(0),
                    region["y"].as_u64().unwrap_or(0),
                    region["width"].as_u64().unwrap_or(0),
                    region["height"].as_u64().unwrap_or(0),
                );
                let (ix, iy) = (rx.min(orig_w as u64) as u32, ry.min(orig_h as u64) as u32);
                let (ix2, iy2) = (
                    (rx + rw).min(orig_w as u64) as u32,
                    (ry + rh).min(orig_h as u64) as u32,
                );
                if ix >= ix2 || iy >= iy2 {
                    return Err(format!(
                        "裁剪区域（x={rx}, y={ry}, {rw}×{rh}）与图片（{orig_w}×{orig_h}）不相交"
                    ));
                }
                crop_note = format!("，裁剪 ({rx},{ry})→({ix2},{iy2})");
                image.crop_imm(ix, iy, ix2 - ix, iy2 - iy)
            } else {
                image
            };

            // 默认等比缩到最长边 2000；full_resolution 不缩（共享编码管线恒定缩放，
            // 故 full_resolution 走独立分支：只编码不缩放）
            let full_resolution = args["full_resolution"].as_bool().unwrap_or(false);
            let compressed = if full_resolution {
                let has_alpha = image.color().has_alpha();
                let prefer_png =
                    has_alpha || matches!(source_mime, "image/png" | "image/gif" | "image/webp");
                let w = image.width();
                let h = image.height();
                let mut media_type = if prefer_png {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                let mut encoded = if prefer_png {
                    let mut buf = std::io::Cursor::new(Vec::new());
                    image
                        .write_to(&mut buf, image::ImageFormat::Png)
                        .map_err(|e| format!("图片编码失败: {e}"))?;
                    buf.into_inner()
                } else {
                    encode_jpeg(&image)?
                };
                if prefer_png && encoded.len() > MAX_PNG_BYTES && !has_alpha {
                    encoded = encode_jpeg(&image)?;
                    media_type = "image/jpeg";
                }
                CompressedImage {
                    bytes: encoded,
                    media_type: media_type.to_string(),
                    width: w,
                    height: h,
                }
            } else {
                encode_image_for_model(image, source_mime)?
            };
            let (w, h) = (compressed.width, compressed.height);
            let kb = compressed.bytes.len() / 1024;
            Ok(ToolEffect {
                output: format!(
                    "已读取图片 {path}（原始 {orig_w}×{orig_h}{crop_note} → 输出 {w}×{h}，{}，{kb}KB）",
                    compressed.media_type
                ),
                file_change: None,
                edit_diff: None,
                images: vec![ToolImage {
                    media_type: compressed.media_type,
                    data_base64: base64_encode(&compressed.bytes),
                    width: w,
                    height: h,
                }],
            })
        })
    }
}

/// 只读尺寸（不解码）；非图片返回 None。粘贴 chip 展示用。
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// 完整解码校验：成功返回尺寸。缩略图渲染前的坏字节防护（UI 降级用）
pub fn decode_image_check(bytes: &[u8]) -> Option<(u32, u32)> {
    let image = image::load_from_memory(bytes).ok()?;
    Some((image.width(), image.height()))
}

/// JPEG q85 编码（转 RGB 丢弃 alpha）
fn encode_jpeg(image: &image::DynamicImage) -> Result<Vec<u8>, String> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
        .encode_image(&image.to_rgb8())
        .map_err(|e| format!("图片编码失败: {e}"))?;
    Ok(buf.into_inner())
}

