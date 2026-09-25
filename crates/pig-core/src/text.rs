//! 文本管线：磁盘字节 ↔ 模型视图（UTF-8、LF）双向转换。
//!
//! decode 判定顺序：BOM → 无 BOM 的 UTF-16 启发式 → NUL 二进制嗅探 →
//! 控制字符占比嗅探 → 严格 UTF-8 → GBK round-trip。encode 按原编码/行尾还原字节，
//! GBK 遇到不可编码字符直接拒绝，避免静默破坏文件。

/// 文件主导行尾符
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

/// 识别出的文件编码
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Gbk,
}

impl FileEncoding {
    /// 面向模型输出的编码名
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
            Self::Gbk => "GBK",
        }
    }
}

/// decode 产物：`text` 为模型视图（CRLF 已归一为 LF），其余字段供写回时还原。
#[derive(Debug)]
pub struct TextDocument {
    pub text: String,
    pub encoding: FileEncoding,
    pub bom: bool,
    pub line_ending: LineEnding,
    /// 解码发生了 U+FFFD 替换（编码识别可能有误）
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
    // 2. 无 BOM 的 UTF-16 启发式（kimi-code 做法）
    } else if let Some(encoding) = guess_utf16(bytes) {
        (encoding, false, bytes)
    // 3. 原始字节含 NUL → 二进制
    } else if bytes.contains(&0) {
        return Err("二进制文件，无法作为文本读取".to_string());
    // 4. 控制字符占比嗅探（前 512 字节）
    } else if looks_binary(bytes) {
        return Err("二进制文件，无法作为文本读取".to_string());
    // 5. 严格 UTF-8
    } else if std::str::from_utf8(bytes).is_ok() {
        (FileEncoding::Utf8, false, bytes)
    // 6. GBK round-trip（ZCode 做法）：解码再编码与原始字节完全相等才接受
    } else if gbk_roundtrip(bytes) {
        (FileEncoding::Gbk, false, bytes)
    } else {
        return Err(
            "无法识别的文本编码（非 UTF-8/UTF-16/GBK）：如确认是文本，请先用 iconv 转为 UTF-8"
                .to_string(),
        );
    };

    // 7. 按编码解码（UTF-16 失败降级 lossy；UTF-8 严格失败已在 5/6 分流，不会走到这）
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

/// 写回：先防御性归一入参为 LF，再按 line_ending 还原，最后按编码出字节。
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
                    "内容包含 GBK 无法编码的字符，已拒绝写入以免破坏文件；请先转换文件编码"
                        .to_string(),
                );
            }
            Ok(bytes.into_owned())
        }
    }
}

/// 无 BOM 的 UTF-16 启发式：前 512 字节（取偶数长度）统计偶/奇数位的 0x00；
/// zeros 总数 ≥ 2 且一侧为 0 或 ≥ 另一侧 3 倍 → 判 UTF-16（偶数位零多 = BE，奇数位零多 = LE）。
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

/// 控制字符占比：前 512 字节中 < 0x09 或 0x0e..=0x1f 的字节超过 30% 判二进制。
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

/// GBK round-trip：解码再编码与原始字节完全相等才接受为 GBK。
fn gbk_roundtrip(bytes: &[u8]) -> bool {
    let (text, _, _) = encoding_rs::GBK.decode(bytes);
    let (encoded, _, had_errors) = encoding_rs::GBK.encode(&text);
    !had_errors && encoded.as_ref() == bytes
}

/// u16 单元按端序组装；失败降级 lossy，奇数尾字节丢弃。返回 (text, lossy)。
fn decode_utf16(body: &[u8], little_endian: bool) -> (String, bool) {
    let mut chunks = body.chunks_exact(2);
    let units: Vec<u16> = (&mut chunks)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    let trailing_odd = !chunks.remainder().is_empty();
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

/// 统计 \r\n 与孤立 \n 定主导行尾；模型视图一律把 \r\n 归一为 \n（孤立 \r 保留）。
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
