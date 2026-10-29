//! Text pipeline: bidirectional conversion between on-disk bytes and the model view (UTF-8, LF).
//!
//! decode detection order: BOM -> BOM-less UTF-16 heuristic -> NUL binary sniffing ->
//! control-character ratio sniffing -> strict UTF-8 -> GBK round-trip. encode restores bytes by
//! the original encoding/line ending; GBK rejects unencodable characters outright to avoid silently corrupting files.

/// The file's dominant line ending
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

/// The detected file encoding
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Gbk,
}

impl FileEncoding {
    /// Encoding name for model-facing output
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
            Self::Gbk => "GBK",
        }
    }
}

/// decode output: `text` is the model view (CRLF normalized to LF); the other fields restore the original form on write-back.
#[derive(Debug)]
pub struct TextDocument {
    pub text: String,
    pub encoding: FileEncoding,
    pub bom: bool,
    pub line_ending: LineEnding,
    /// Decoding involved U+FFFD replacement (encoding detection may be wrong)
    pub lossy: bool,
}

pub fn decode(bytes: &[u8]) -> Result<TextDocument, String> {
    // 1. BOM
    let (encoding, bom, body) = if let Some(body) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        (FileEncoding::Utf8, true, body)
    } else if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (FileEncoding::Utf16Le, true, body)
    } else if let Some(body) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (FileEncoding::Utf16Be, true, body)
    // 2. BOM-less UTF-16 heuristic (kimi-code approach)
    } else if let Some(encoding) = guess_utf16(bytes) {
        (encoding, false, bytes)
    // 3. Raw bytes contain NUL -> binary
    } else if bytes.contains(&0) {
        return Err("Binary file; cannot be read as text".to_string());
    // 4. Control-character ratio sniffing (first 512 bytes)
    } else if looks_binary(bytes) {
        return Err("Binary file; cannot be read as text".to_string());
    // 5. Strict UTF-8
    } else if std::str::from_utf8(bytes).is_ok() {
        (FileEncoding::Utf8, false, bytes)
    // 6. GBK round-trip (ZCode approach): accepted only if decode-then-encode exactly equals the original bytes
    } else if gbk_roundtrip(bytes) {
        (FileEncoding::Gbk, false, bytes)
    } else {
        return Err(
            "Unrecognized text encoding (not UTF-8/UTF-16/GBK); if this is text, convert it to UTF-8 with iconv first"
                .to_string(),
        );
    };

    // 7. Decode by encoding (UTF-16 failure degrades to lossy; strict UTF-8 failure was already diverted in 5/6 and cannot reach here)
    let (text, lossy) = match encoding {
        FileEncoding::Utf8 => match std::str::from_utf8(body) {
            Ok(text) => (text.to_string(), false),
            Err(_) => (String::from_utf8_lossy(body).into_owned(), true),
        },
        FileEncoding::Gbk => {
            let (text, _, _) = encoding_rs::GBK.decode(body);
            (text.into_owned(), false)
        }
        FileEncoding::Utf16Le => decode_utf16(body, true),
        FileEncoding::Utf16Be => decode_utf16(body, false),
    };

    let (text, line_ending) = normalize_line_endings(text);
    Ok(TextDocument {
        text,
        encoding,
        bom,
        line_ending,
        lossy,
    })
}

/// Write-back: defensively normalize the input to LF first, restore by line_ending, then emit bytes by encoding.
pub fn encode(
    text_lf: &str,
    encoding: FileEncoding,
    bom: bool,
    line_ending: LineEnding,
) -> Result<Vec<u8>, String> {
    let normalized = text_lf.replace("\r\n", "\n");
    let text = match line_ending {
        LineEnding::Lf => normalized,
        LineEnding::Crlf => normalized.replace('\n', "\r\n"),
    };
    match encoding {
        FileEncoding::Utf8 => {
            let mut out = Vec::with_capacity(text.len() + 3);
            if bom {
                out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            }
            out.extend_from_slice(text.as_bytes());
            Ok(out)
        }
        FileEncoding::Utf16Le => Ok(encode_utf16(&text, true, bom)),
        FileEncoding::Utf16Be => Ok(encode_utf16(&text, false, bom)),
        FileEncoding::Gbk => {
            let (bytes, _, had_errors) = encoding_rs::GBK.encode(&text);
            if had_errors {
                return Err(
                    "The content contains characters that GBK cannot encode; the write was refused to avoid corrupting the file — convert the file encoding first"
                        .to_string(),
                );
            }
            Ok(bytes.into_owned())
        }
    }
}

/// BOM-less UTF-16 heuristic: over the first 512 bytes (rounded to an even length), count 0x00 at
/// even/odd positions; total zeros >= 2 and one side is 0 or >= 3x the other -> UTF-16 (more even-position zeros = BE, more odd-position zeros = LE).
fn guess_utf16(bytes: &[u8]) -> Option<FileEncoding> {
    let len = bytes.len().min(512);
    let len = len - len % 2;
    let sample = &bytes[..len];
    let mut even_zeros = 0usize;
    let mut odd_zeros = 0usize;
    for (index, byte) in sample.iter().enumerate() {
        if *byte == 0 {
            if index % 2 == 0 {
                even_zeros += 1;
            } else {
                odd_zeros += 1;
            }
        }
    }
    if even_zeros + odd_zeros < 2 {
        return None;
    }
    let (more, less, encoding) = if even_zeros >= odd_zeros {
        (even_zeros, odd_zeros, FileEncoding::Utf16Be)
    } else {
        (odd_zeros, even_zeros, FileEncoding::Utf16Le)
    };
    if less == 0 || more >= less * 3 {
        Some(encoding)
    } else {
        None
    }
}

/// Control-character ratio: if bytes < 0x09 or in 0x0e..=0x1f exceed 30% of the first 512 bytes, judge as binary.
fn looks_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(512)];
    if sample.is_empty() {
        return false;
    }
    let controls = sample
        .iter()
        .filter(|&&b| b < 0x09 || (0x0e..=0x1f).contains(&b))
        .count();
    controls * 10 > sample.len() * 3
}

/// GBK round-trip: accepted as GBK only if decode-then-encode exactly equals the original bytes.
fn gbk_roundtrip(bytes: &[u8]) -> bool {
    let (text, _, _) = encoding_rs::GBK.decode(bytes);
    let (encoded, _, had_errors) = encoding_rs::GBK.encode(&text);
    !had_errors && encoded.as_ref() == bytes
}

/// Assemble u16 units by endianness; failure degrades to lossy, the trailing odd byte is dropped. Returns (text, lossy).
fn decode_utf16(body: &[u8], little_endian: bool) -> (String, bool) {
    let (chunks, remainder) = body.as_chunks::<2>();
    let units: Vec<u16> = chunks
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();
    let trailing_odd = !remainder.is_empty();
    match String::from_utf16(&units) {
        Ok(text) => (text, trailing_odd),
        Err(_) => (String::from_utf16_lossy(&units), true),
    }
}

fn encode_utf16(text: &str, little_endian: bool, bom: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2 + 2);
    if bom {
        out.extend_from_slice(if little_endian {
            &[0xFF, 0xFE][..]
        } else {
            &[0xFE, 0xFF][..]
        });
    }
    for unit in text.encode_utf16() {
        let pair = if little_endian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        out.extend_from_slice(&pair);
    }
    out
}

/// Count \r\n vs lone \n to decide the dominant line ending; the model view always normalizes \r\n to \n (lone \r preserved).
fn normalize_line_endings(text: String) -> (String, LineEnding) {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    let dominant = if crlf > lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };
    (text.replace("\r\n", "\n"), dominant)
}

/// Subprocess byte stream -> text: strict UTF-8 first; on definitely invalid bytes, Windows falls back to GBK
/// (native tools on Chinese Windows such as ipconfig emit OEM code page cp936 bytes, while Git Bash can only
/// guarantee UTF-8 from bash itself and coreutils); other platforms degrade to U+FFFD replacement. Incomplete
/// UTF-8 tails and the second byte of a GBK double-byte pair spanning chunks are held pending the next chunk;
/// each chunk is judged independently, tolerating mixed "UTF-8 tool && GBK tool" output.
#[derive(Default)]
pub struct StreamDecoder {
    pending: Vec<u8>,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk of bytes, returning the text that can be resolved now (incomplete tails stay inside awaiting the next chunk).
    pub fn push(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let out = text.to_string();
                self.pending.clear();
                out
            }
            Err(error) => {
                let valid = error.valid_up_to();
                let mut out = std::str::from_utf8(&self.pending[..valid])
                    .expect("valid_up_to prefix must be valid UTF-8")
                    .to_string();
                match error.error_len() {
                    // Tail is an incomplete multibyte sequence: hold it and re-judge with the next chunk
                    None => {
                        self.pending.drain(..valid);
                    }
                    // Definitely invalid: the whole remainder goes through fallback decoding
                    Some(_) => {
                        let rest = self.pending.split_off(valid);
                        out.push_str(&Self::fallback_decode(&rest, &mut self.pending));
                    }
                }
                out
            }
        }
    }

    /// Stream end: flush leftover bytes through the platform fallback (no next chunk to wait for;
    /// isolated incomplete sequences are handled lossily by the decoder).
    pub fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        if !cfg!(target_os = "windows") {
            return String::from_utf8_lossy(&pending).into_owned();
        }
        let (text, _, _) = encoding_rs::GBK.decode(&pending);
        text.into_owned()
    }

    /// Fallback for invalid bytes: GBK on Windows, lossy UTF-8 replacement elsewhere.
    /// `pending` passes back held bytes.
    fn fallback_decode(bytes: &[u8], pending: &mut Vec<u8>) -> String {
        if !cfg!(target_os = "windows") {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let cut = gbk_complete_prefix(bytes);
        pending.extend_from_slice(&bytes[cut..]);
        let (text, _, _) = encoding_rs::GBK.decode(&bytes[..cut]);
        text.into_owned()
    }
}

/// GBK double-byte pairing scan: returns the byte length of the "complete GBK sequences" —
/// ASCII single bytes pass; a 0x81..=0xFE lead byte consumes one continuation byte (0x40..=0xFE excluding 0x7F);
/// a trailing lone lead byte (its continuation is in the next chunk) is not counted and left pending.
fn gbk_complete_prefix(bytes: &[u8]) -> usize {
    let mut i = 0;
    while i < bytes.len() {
        let lead = bytes[i];
        if lead < 0x80 {
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(next) if (0x40..=0xfe).contains(next) && *next != 0x7f => i += 2,
            // No continuation byte (stream truncated mid-double-byte): complete up to here
            _ => return i,
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::StreamDecoder;

    /// Multibyte UTF-8 char split across chunks: the first half is held; the second half completes the restoration.
    #[test]
    fn decoder_holds_incomplete_utf8_across_chunks() {
        let mut decoder = StreamDecoder::new();
        let bytes = "中文".as_bytes(); // 3 bytes per char
        assert_eq!(
            decoder.push(&bytes[..4]),
            "中",
            "the first 4 bytes should emit only the first char"
        );
        assert_eq!(decoder.push(&bytes[4..]), "文");
        assert_eq!(decoder.finish(), "");
    }

    /// Definitely invalid bytes: Windows falls back to GBK (D6 D0 CE C4 is the GBK encoding of
    /// the two CJK chars asserted below — chosen because it can never be valid UTF-8, whereas
    /// the sequence C4BF C2BC happens to be).
    #[cfg(windows)]
    #[test]
    fn decoder_falls_back_to_gbk_on_windows() {
        let mut decoder = StreamDecoder::new();
        let gbk = [0xd6, 0xd0, 0xce, 0xc4];
        let mut chunk = b"zh: ".to_vec();
        chunk.extend_from_slice(&gbk);
        let out = decoder.push(&chunk);
        assert_eq!(
            out, "zh: 中文",
            "GBK bytes should fall back to whole-segment decoding"
        );
    }

    /// GBK double-byte char split across chunks: the lone lead byte is held; the next chunk completes it.
    #[cfg(windows)]
    #[test]
    fn decoder_holds_gbk_lead_byte_across_chunks() {
        let mut decoder = StreamDecoder::new();
        assert_eq!(
            decoder.push(&[0xd6]),
            "",
            "a lone D6 is held as an incomplete sequence"
        );
        assert_eq!(decoder.push(&[0xd0, b'a']), "中a");
    }

    /// Non-Windows: invalid bytes degrade to U+FFFD; GBK is not attempted.
    #[cfg(not(windows))]
    #[test]
    fn decoder_lossy_on_unix() {
        let mut decoder = StreamDecoder::new();
        let out = decoder.push(&[0xd6, 0xd0]);
        assert_eq!(
            out.chars().count(),
            2,
            "two invalid bytes each emit one replacement char"
        );
        assert!(out.chars().all(|c| c == '\u{fffd}'));
    }

    /// Stream ends in the middle of a multibyte sequence: finish flushes it without losing bytes.
    #[test]
    fn decoder_finish_flushes_partial() {
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.push("abc".as_bytes()), "abc");
        assert_eq!(
            decoder.push(&[0xe4, 0xb8]),
            "",
            "the first two bytes of the 3-byte char should be held"
        );
        assert!(
            !decoder.finish().is_empty(),
            "leftover bytes should be flushed (lossy)"
        );
    }
}
