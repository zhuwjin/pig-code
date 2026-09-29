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

/// 子进程字节流 → 文本：UTF-8 严格优先；出现确定无效字节时 Windows 上按 GBK
/// 回退（中文 Windows 的原生工具如 ipconfig 输出 OEM 代码页 cp936 字节，
/// Git Bash 只能保证 bash 自身与 coreutils 输出 UTF-8），其余平台退化为
/// U+FFFD 替换。跨 chunk 的不完整 UTF-8 尾部与 GBK 双字节对的后半字节
/// 都挂起等待下一块；每块独立判定，兼容「UTF-8 工具 && GBK 工具」混排输出。
#[derive(Default)]
pub struct StreamDecoder {
    pending: Vec<u8>,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一块字节，返回当前可确定的文本（不完整尾部留在内部等下一块）。
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
                    .expect("valid_up_to 前缀必为合法 UTF-8")
                    .to_string();
                match error.error_len() {
                    // 尾部是不完整的多字节序列：挂起，拼下一块再判
                    None => {
                        self.pending.drain(..valid);
                    }
                    // 确定无效：整段余量走兜底解码
                    Some(_) => {
                        let rest = self.pending.split_off(valid);
                        out.push_str(&Self::fallback_decode(&rest, &mut self.pending));
                    }
                }
                out
            }
        }
    }

    /// 流结束：残留字节按平台兜底出清（不再有下一块可等，孤立的
    /// 不完整序列由解码器有损处理）。
    pub fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        if !cfg!(target_os = "windows") {
            return String::from_utf8_lossy(&pending).into_owned();
        }
        let (text, _, _) = encoding_rs::GBK.decode(&pending);
        text.into_owned()
    }

    /// 无效字节的兜底：Windows 上 GBK，其余平台 UTF-8 有损替换。
    /// `pending` 回传挂起字节。
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

/// GBK 双字节配对扫描：返回「完整 GBK 序列」的字节长度——
/// ASCII 单字节过；0x81..=0xFE 引导字节吃一个续字节（0x40..=0xFE 非 0x7F）；
/// 末尾孤立引导字节（续字节在下一块）不计入，留给挂起。
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
            // 无续字节（流截断在双字节中间）：到此为止完整
            _ => return i,
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::StreamDecoder;

    /// 多字节 UTF-8 字符跨 chunk 分割：前半挂起、后半拼上后完整还原。
    #[test]
    fn decoder_holds_incomplete_utf8_across_chunks() {
        let mut decoder = StreamDecoder::new();
        let bytes = "中文".as_bytes(); // 每字 3 字节
        assert_eq!(decoder.push(&bytes[..4]), "中", "前 4 字节只应出第一个字");
        assert_eq!(decoder.push(&bytes[4..]), "文");
        assert_eq!(decoder.finish(), "");
    }

    /// 确定无效的字节：Windows 走 GBK 回退（D6 D0 CE C4 = GBK 的「中文」，
    /// 选这段是因为它必然不是合法 UTF-8——「目录」(C4BF C2BC) 恰好是）。
    #[cfg(windows)]
    #[test]
    fn decoder_falls_back_to_gbk_on_windows() {
        let mut decoder = StreamDecoder::new();
        let gbk = [0xd6, 0xd0, 0xce, 0xc4];
        let mut chunk = b"zh: ".to_vec();
        chunk.extend_from_slice(&gbk);
        let out = decoder.push(&chunk);
        assert_eq!(out, "zh: 中文", "GBK 字节应整段回退解码");
    }

    /// GBK 双字节字符跨 chunk 分割：孤立引导字节挂起，下一块补全。
    #[cfg(windows)]
    #[test]
    fn decoder_holds_gbk_lead_byte_across_chunks() {
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.push(&[0xd6]), "", "孤立 D6 按不完整序列挂起");
        assert_eq!(decoder.push(&[0xd0, b'a']), "中a");
    }

    /// 非 Windows：无效字节退化为 U+FFFD，不尝试 GBK。
    #[cfg(not(windows))]
    #[test]
    fn decoder_lossy_on_unix() {
        let mut decoder = StreamDecoder::new();
        let out = decoder.push(&[0xd6, 0xd0]);
        assert_eq!(out.chars().count(), 2, "两个无效字节各出一个替换字符");
        assert!(out.chars().all(|c| c == '\u{fffd}'));
    }

    /// 流在多字节序列中间结束：finish 出清，不丢字节。
    #[test]
    fn decoder_finish_flushes_partial() {
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.push("abc".as_bytes()), "abc");
        assert_eq!(decoder.push(&[0xe4, 0xb8]), "", "「一」的前两字节应挂起");
        assert!(!decoder.finish().is_empty(), "残留字节应出清（有损）");
    }
}
