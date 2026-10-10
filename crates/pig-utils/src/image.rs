//! Image helpers shared by the engine's ReadMediaFile tool and the GUI's
//! paste/attachment paths: magic-byte sniffing, model-budget-friendly
//! compression (2000px longest edge, PNG/JPEG fallback), TIFF normalization,
//! dimension probing, and a hand-rolled base64.

/// Image magic-byte sniffing (reads the file header, does not trust the extension): returns the mime on a hit, None otherwise.
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

/// Standard base64 with padding (hand-rolled: the workspace keeps a zero-dep
/// stance on trivial codecs)
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
pub const MAX_MEDIA_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// Total-pixel cap applied before any full decode (memory guard)
pub const MAX_MEDIA_PIXELS: u64 = 100_000_000;
/// Default scaling to a 2000px longest edge (no scaling when full_resolution=true)
const MEDIA_MAX_EDGE: u32 = 2000;
/// PNG output over 4MB without alpha → fall back to JPEG q85 (shared by the scaled and unscaled encoding paths)
pub const MAX_PNG_BYTES: usize = 4 * 1024 * 1024;

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

/// Dimensions without a full decode (header probe)
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
pub fn encode_jpeg(image: &image::DynamicImage) -> Result<Vec<u8>, String> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
        .encode_image(&image.to_rgb8())
        .map_err(|e| format!("Failed to encode image: {e}"))?;
    Ok(buf.into_inner())
}
