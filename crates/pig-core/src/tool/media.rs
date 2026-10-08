use super::*;

/// Image magic-byte sniffing (reads the file header, does not trust the extension): returns the mime on a hit, None otherwise.
/// Shared by Read/ReadMediaFile.
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

/// Hand-written base64 (standard alphabet + `=` padding; no extra dependency, same style as the snapshot hex encoding)
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

/// Convert raw TIFF bytes to PNG without scaling, to normalize the format for the clipboard paste entry point.
/// Dimensions are read before decoding so oversized TIFFs do not hog memory at the decode stage.
/// Error messages are English constants (model channel: they enter context verbatim as tool results / paste-failure notices).
pub fn convert_tiff_to_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    if bytes.len() as u64 > MAX_MEDIA_FILE_BYTES {
        return Err(format!(
            "TIFF file exceeds the {}MB limit",
            MAX_MEDIA_FILE_BYTES / 1024 / 1024
        ));
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("Failed to parse TIFF: {e}"))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|e| format!("Failed to read TIFF dimensions: {e}"))?;
    if width == 0 || height == 0 {
        return Err("Invalid TIFF dimensions".to_string());
    }
    if width as u64 * height as u64 > MAX_MEDIA_PIXELS {
        return Err(format!("TIFF image too large ({width}×{height})"));
    }
    let image = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("Failed to parse TIFF: {e}"))?
        .decode()
        .map_err(|e| format!("Failed to decode TIFF: {e}"))?;
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode TIFF as PNG: {e}"))?;
    Ok((output.into_inner(), width, height))
}

/// Media file size/pixel caps (shared by ReadMediaFile and paste sending)
const MAX_MEDIA_FILE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_MEDIA_PIXELS: u64 = 100_000_000;
/// Default scaling to a 2000px longest edge (no scaling when full_resolution=true)
const MEDIA_MAX_EDGE: u32 = 2000;
/// PNG output over 4MB without alpha → fall back to JPEG q85
const MAX_PNG_BYTES: usize = 4 * 1024 * 1024;

/// Compression product (the image that enters the model budget)
pub struct CompressedImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}

/// Budget-friendly encoding of a decoded image: proportional scaling to a 2000px longest edge (small ones untouched);
/// alpha present or source is PNG/GIF/WebP → PNG; otherwise JPEG q85; PNG over 4MB without alpha → JPEG fallback.
/// Shared by ReadMediaFile (after the region crop) and paste sending.
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
        return Err(format!(
            "Image too large ({w}×{h}); crop a section with the region parameter"
        ));
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
            .map_err(|e| format!("Failed to encode image: {e}"))?;
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

/// Raw bytes → compressed product (paste-send path): magic-byte sniffing when source_mime is an empty string.
/// Dimension precheck (without decoding the full image) → decode → encode_image_for_model.
pub fn compress_image_for_model(
    bytes: &[u8],
    source_mime: &str,
) -> Result<CompressedImage, String> {
    let sniffed = if source_mime.is_empty() {
        sniff_image(bytes)
            .ok_or("Not a recognizable image (PNG/JPEG/GIF/WebP supported)".to_string())?
    } else {
        source_mime
    };
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("Failed to parse image: {e}"))?;
    let (w, h) = reader
        .into_dimensions()
        .map_err(|e| format!("Failed to parse image: {e}"))?;
    if w as u64 * h as u64 > MAX_MEDIA_PIXELS {
        return Err(format!("Image too large ({w}×{h})"));
    }
    let image = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("Failed to parse image: {e}"))?
        .decode()
        .map_err(|e| format!("Failed to decode image: {e}"))?;
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
                "description": "Read an image file into context (PNG/JPEG/GIF/WebP; detected by magic bytes, not the extension). Images are scaled proportionally to a 2000px longest edge by default; region crops a section in original-image pixel coordinates, and full_resolution=true skips scaling. The model must support image input.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Image path relative to the working directory" },
                        "region": {
                            "type": "object",
                            "description": "Optional crop region (original-image pixel coordinates; clamped to the image bounds, error if disjoint)",
                            "properties": {
                                "x": { "type": "integer" },
                                "y": { "type": "integer" },
                                "width": { "type": "integer" },
                                "height": { "type": "integer" }
                            },
                            "required": ["x", "y", "width", "height"]
                        },
                        "full_resolution": { "type": "boolean", "description": "true skips the 2000px scaling (default false)" }
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
            let path = args["path"]
                .as_str()
                .ok_or("Missing required parameter: path")?;
            let full = resolve_with_access(&ctx, path, false, FsAccess::Read)?;
            if is_sensitive_file(&full) {
                return Err(sensitive_file_error(&full));
            }
            let bytes = std::fs::read(&full).map_err(|e| read_io_error(path, &full, e))?;
            if bytes.len() as u64 > MAX_MEDIA_FILE_BYTES {
                return Err(format!(
                    "File exceeds the 100MB limit ({}MB)",
                    bytes.len() / 1024 / 1024
                ));
            }
            let Some(source_mime) = sniff_image(&bytes) else {
                return Err(
                    "Not a recognizable image (PNG/JPEG/GIF/WebP supported); video is not supported yet"
                        .to_string(),
                );
            };
            // Read dimensions before decoding: reject outright when total pixels exceed the cap (prevents decoding huge images from exhausting memory)
            let reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .map_err(|e| format!("Failed to parse image: {e}"))?;
            let (orig_w, orig_h) = reader
                .into_dimensions()
                .map_err(|e| format!("Failed to parse image: {e}"))?;
            if orig_w as u64 * orig_h as u64 > MAX_MEDIA_PIXELS {
                return Err(format!(
                    "Image too large ({orig_w}×{orig_h}); crop a section with the region parameter"
                ));
            }
            let image = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .map_err(|e| format!("Failed to parse image: {e}"))?
                .decode()
                .map_err(|e| format!("Failed to decode image: {e}"))?;

            // region crop (original-image coordinates; clamped to the image bounds, errors when fully disjoint)
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
                        "Crop region (x={rx}, y={ry}, {rw}×{rh}) does not intersect the image ({orig_w}×{orig_h})"
                    ));
                }
                crop_note = format!(", cropped ({rx},{ry})->({ix2},{iy2})");
                image.crop_imm(ix, iy, ix2 - ix, iy2 - iy)
            } else {
                image
            };

            // Default proportional scaling to a 2000px longest edge; full_resolution skips scaling (the shared encoding
            // pipeline always scales, so full_resolution takes a separate branch: encode only, no scaling)
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
                        .map_err(|e| format!("Failed to encode image: {e}"))?;
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
                    "Read image {path} (original {orig_w}×{orig_h}{crop_note} -> output {w}×{h}, {}, {kb}KB)",
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

/// Read dimensions only (no decoding); returns None for non-images. Used by the paste chip display.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Full decode check: returns dimensions on success. Bad-byte guard before thumbnail rendering (for UI fallback)
pub fn decode_image_check(bytes: &[u8]) -> Option<(u32, u32)> {
    let image = image::load_from_memory(bytes).ok()?;
    Some((image.width(), image.height()))
}

/// JPEG q85 encoding (converted to RGB, alpha dropped)
fn encode_jpeg(image: &image::DynamicImage) -> Result<Vec<u8>, String> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
        .encode_image(&image.to_rgb8())
        .map_err(|e| format!("Failed to encode image: {e}"))?;
    Ok(buf.into_inner())
}
