use super::*;
use pig_utils::image::{
    CompressedImage, MAX_MEDIA_FILE_BYTES, MAX_MEDIA_PIXELS, MAX_PNG_BYTES, base64_encode,
    encode_image_for_model, encode_jpeg, sniff_image,
};
use pig_utils::pdf::{MAX_PDF_FILE_BYTES, PDF_MEDIA_TYPE, PdfKind, classify_pdf, has_pdf_header};
use std::io::{Read, Seek, SeekFrom};

/// Header probe window; a full window is read even when the reader returns
/// short reads (the header rule itself lives in `pig_utils::pdf`).
const PDF_HEADER_PROBE_BYTES: usize = 1024;

/// Fill the probe window across short reads; stops at EOF.
fn read_probe(reader: &mut impl Read, probe: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < probe.len() {
        match reader.read(&mut probe[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Read at most `cap + 1` bytes from the current position: the extra byte is
/// enough to prove the file is over the cap without loading it whole.
fn read_limited(reader: &mut impl Read, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.by_ref().take(cap + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Final validation of the image that will enter model context.
///
/// The source bytes are decoded before encoding, but the encoded product is
/// what the provider receives: re-sniff it, enforce the size cap, and decode it
/// once more so a broken encoder or a corrupted intermediate cannot turn into a
/// successful `ReadMediaFile` with unusable media. Dimensions must survive the
/// round trip; the media type must match the magic bytes actually sent.
fn validated_tool_image(compressed: CompressedImage) -> Result<ToolImage, String> {
    if compressed.bytes.is_empty() {
        return Err("Image encoding produced no bytes".to_string());
    }
    if compressed.bytes.len() as u64 > MAX_MEDIA_FILE_BYTES {
        return Err(format!(
            "Encoded image exceeds the {}MB limit",
            MAX_MEDIA_FILE_BYTES / 1024 / 1024
        ));
    }
    let Some(actual) = sniff_image(&compressed.bytes) else {
        return Err("Encoded image is not a recognizable PNG/JPEG/GIF/WebP".to_string());
    };
    if actual != compressed.media_type.as_str() {
        return Err(format!(
            "Encoded image media type mismatch: declared {}, bytes are {actual}",
            compressed.media_type
        ));
    }
    let Some((width, height)) = decode_image_check(&compressed.bytes) else {
        return Err("Encoded image failed to decode; the source may be corrupt".to_string());
    };
    if (width, height) != (compressed.width, compressed.height) {
        return Err(format!(
            "Encoded image dimensions changed ({}×{} -> {width}×{height})",
            compressed.width, compressed.height
        ));
    }
    Ok(ToolImage {
        media_type: compressed.media_type,
        data_base64: base64_encode(&compressed.bytes),
        width,
        height,
    })
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
                "description": "Read an image (PNG/JPEG/GIF/WebP) or PDF into context; the type is detected by magic bytes, not the extension. Images are scaled proportionally to a 2000px longest edge by default; region crops a section in original-image pixel coordinates, and full_resolution=true skips scaling. PDFs are sent whole as documents (up to 10MB). The model must support the matching input capability.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Image or PDF path relative to the working directory" },
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
            if let Some(message) = sandbox_read_rejection(&ctx, path, &full) {
                return Err(message);
            }
            if is_sensitive_file(&full) && !approved_this_call(&ctx, &full) {
                return Err(sensitive_file_error(&full));
            }
            // Header-first probing with a full window read: a short read must
            // not hide a header that fits the probe. Only a plausible image or
            // PDF continues; one file handle has bounded reads until the end,
            // so a stat/read race cannot bypass the size limits.
            let mut file = std::fs::File::open(&full).map_err(|e| read_io_error(path, &full, e))?;
            let mut probe = [0u8; PDF_HEADER_PROBE_BYTES];
            let probed =
                read_probe(&mut file, &mut probe).map_err(|e| read_io_error(path, &full, e))?;
            let probe = &probe[..probed];
            let probe_image = sniff_image(probe);
            file.seek(SeekFrom::Start(0))
                .map_err(|e| read_io_error(path, &full, e))?;
            if probe_image.is_none() && has_pdf_header(probe) {
                let bytes = read_limited(&mut file, MAX_PDF_FILE_BYTES)
                    .map_err(|e| read_io_error(path, &full, e))?;
                if bytes.len() as u64 > MAX_PDF_FILE_BYTES {
                    return Err(format!(
                        "PDF exceeds the 10MB limit ({}MB); split it into smaller documents",
                        bytes.len() / 1024 / 1024
                    ));
                }
                if classify_pdf(&bytes) != PdfKind::Valid {
                    return Err(
                        "PDF is truncated or corrupted (missing the %%EOF marker); re-export it and try again"
                            .to_string(),
                    );
                }
                let filename = full
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "document.pdf".to_string());
                return Ok(ToolEffect {
                    output: format!(
                        "Read PDF {path} ({}, {}KB); the document was attached to this tool result",
                        PDF_MEDIA_TYPE,
                        bytes.len() / 1024
                    ),
                    file_change: None,
                    edit_diff: None,
                    images: vec![],
                    documents: vec![ToolDocument {
                        media_type: PDF_MEDIA_TYPE.to_string(),
                        bytes,
                        filename: filename.clone(),
                        label: Some(filename),
                    }],
                    delivered_notification_ids: vec![],
                });
            }
            if probe_image.is_none() {
                return Err(
                    "Not a recognizable image (PNG/JPEG/GIF/WebP) or PDF; video is not supported yet"
                        .to_string(),
                );
            }
            let bytes = read_limited(&mut file, MAX_MEDIA_FILE_BYTES)
                .map_err(|e| read_io_error(path, &full, e))?;
            if bytes.len() as u64 > MAX_MEDIA_FILE_BYTES {
                return Err(format!(
                    "File exceeds the 100MB limit ({}MB)",
                    bytes.len() / 1024 / 1024
                ));
            }
            let Some(source_mime) = sniff_image(&bytes) else {
                return Err(
                    "Not a recognizable image (PNG/JPEG/GIF/WebP) or PDF; video is not supported yet"
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
            let image = validated_tool_image(compressed)?;
            Ok(ToolEffect {
                output: format!(
                    "Read image {path} (original {orig_w}×{orig_h}{crop_note} -> output {w}×{h}, {}, {kb}KB)",
                    image.media_type
                ),
                file_change: None,
                edit_diff: None,
                images: vec![image],
                documents: vec![],
                delivered_notification_ids: vec![],
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reader that returns at most `step` bytes per call, like a slow pipe.
    struct ShortReader {
        bytes: Vec<u8>,
        step: usize,
    }

    impl Read for ShortReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let take = self.step.min(buf.len()).min(self.bytes.len());
            buf[..take].copy_from_slice(&self.bytes[..take]);
            self.bytes.drain(..take);
            Ok(take)
        }
    }

    #[test]
    fn header_probe_fills_across_short_reads() {
        let source = vec![b'x'; PDF_HEADER_PROBE_BYTES];
        let mut reader = ShortReader {
            bytes: source.clone(),
            step: 7,
        };
        let mut probe = [0u8; PDF_HEADER_PROBE_BYTES];
        let filled = read_probe(&mut reader, &mut probe).expect("probe");
        assert_eq!(filled, PDF_HEADER_PROBE_BYTES);
        assert_eq!(&probe[..], source.as_slice());
    }

    #[test]
    fn header_probe_stops_at_eof() {
        let source = vec![b'y'; 42];
        let mut reader = ShortReader {
            bytes: source.clone(),
            step: 5,
        };
        let mut probe = [0u8; PDF_HEADER_PROBE_BYTES];
        let filled = read_probe(&mut reader, &mut probe).expect("probe");
        assert_eq!(filled, source.len());
        assert_eq!(&probe[..filled], source.as_slice());
    }

    #[test]
    fn bounded_read_stops_at_cap_plus_one() {
        let mut reader = std::io::Cursor::new(vec![b'z'; 100]);
        let bytes = read_limited(&mut reader, 10).expect("bounded read");
        assert_eq!(
            bytes.len(),
            11,
            "the extra byte proves the cap was exceeded"
        );

        let mut small = std::io::Cursor::new(vec![b'z'; 4]);
        let bytes = read_limited(&mut small, 10).expect("bounded read");
        assert_eq!(bytes.len(), 4);
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([12, 34, 56, 255]));
        let mut cursor = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .expect("encode fixture png");
        cursor.into_inner()
    }

    fn media_fixture(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pig-core-media-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create media fixture");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write media fixture");
        dir.canonicalize().expect("canonical fixture")
    }

    async fn read_media(dir: &std::path::Path, path: &str) -> Result<ToolEffect, String> {
        let mut tracker = ChangeTracker::default();
        let state = crate::task::SessionToolState::for_test();
        ReadMediaFile
            .execute(
                serde_json::json!({ "path": path }),
                ToolContext {
                    cwd: dir,
                    tracker: &mut tracker,
                    state: &state,
                    fs_grant: None,
                    sandbox_override: Default::default(),
                    sandbox_profile: Default::default(),
                    additional_permissions: Default::default(),
                },
            )
            .await
    }

    /// The bytes that actually enter model context are re-sniffed and decoded:
    /// a valid source must survive the encode → validate round trip with the
    /// declared media type and dimensions.
    #[tokio::test]
    async fn a_valid_image_is_revalidated_before_media_output() {
        let bytes = png_bytes(3, 2);
        let dir = media_fixture("valid", &bytes);
        let effect = read_media(&dir, "valid").await.expect("valid image reads");
        assert_eq!(effect.images.len(), 1);
        let image = &effect.images[0];
        assert_eq!(image.media_type, "image/png");
        let encoded = pig_utils::image::base64_decode(&image.data_base64).expect("base64");
        assert_eq!(pig_utils::image::sniff_image(&encoded), Some("image/png"));
        assert_eq!(pig_utils::image::image_dimensions(&encoded), Some((3, 2)));
        assert_eq!((image.width, image.height), (3, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// JPEG is the other encoded media type the validation accepts; its magic
    /// bytes must match the declared type after the encode round trip.
    #[tokio::test]
    async fn a_valid_jpeg_is_revalidated_with_its_mime_type() {
        let image = image::RgbImage::from_pixel(2, 3, image::Rgb([90, 20, 40]));
        let mut cursor = std::io::Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 85)
            .encode_image(&image)
            .expect("encode jpeg fixture");
        let dir = media_fixture("valid.jpg", &cursor.into_inner());
        let effect = read_media(&dir, "valid.jpg")
            .await
            .expect("valid jpeg reads");
        assert_eq!(effect.images.len(), 1);
        let image = &effect.images[0];
        assert_eq!(image.media_type, "image/jpeg");
        let encoded = pig_utils::image::base64_decode(&image.data_base64).expect("base64");
        assert_eq!(pig_utils::image::sniff_image(&encoded), Some("image/jpeg"));
        assert_eq!((image.width, image.height), (2, 3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt or mislabeled image must fail at the tool boundary rather than
    /// producing a "Read image" success with unusable media. The header alone is
    /// enough for the magic-byte sniff, so the decode/validation step is what
    /// has to catch these.
    #[tokio::test]
    async fn corrupt_images_are_refused_at_media_load() {
        let valid = png_bytes(2, 2);
        let mut truncated = valid.clone();
        truncated.truncate(40);
        let cases: [(&str, Vec<u8>); 3] = [
            // Header present, no complete IHDR/IDAT.
            (
                "header-only.png",
                b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01".to_vec(),
            ),
            // A valid PNG cut before its image data.
            ("truncated.png", truncated),
            // Magic bytes plus unrelated garbage.
            (
                "garbage.png",
                [&b"\x89PNG\r\n\x1a\n"[..], &b"this is not a png"[..]].concat(),
            ),
        ];
        for (name, bytes) in cases {
            let dir = media_fixture(name, &bytes);
            let error = match read_media(&dir, name).await {
                Ok(_) => panic!("{name} must not produce media output"),
                Err(error) => error,
            };
            assert!(
                error.contains("parse") || error.contains("decode") || error.contains("corrupt"),
                "{name}: {error}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The final check also rejects a correctly decodable product whose
    /// declared media type does not match its bytes (a lying encoder or a
    /// changed intermediate).
    #[test]
    fn encoded_media_must_match_its_declared_type_and_dimensions() {
        let bytes = png_bytes(2, 2);
        let mislabeled = validated_tool_image(CompressedImage {
            bytes: bytes.clone(),
            media_type: "image/jpeg".to_string(),
            width: 2,
            height: 2,
        });
        assert!(mislabeled.is_err(), "media type must match magic bytes");

        let wrong_dimensions = validated_tool_image(CompressedImage {
            bytes,
            media_type: "image/png".to_string(),
            width: 9,
            height: 9,
        });
        assert!(
            wrong_dimensions.is_err(),
            "dimensions must survive the round trip"
        );
    }
}
