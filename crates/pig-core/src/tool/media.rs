use super::*;
use pig_utils::image::{
    CompressedImage, MAX_MEDIA_FILE_BYTES, MAX_MEDIA_PIXELS, MAX_PNG_BYTES, base64_encode,
    encode_image_for_model, encode_jpeg, sniff_image,
};

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
