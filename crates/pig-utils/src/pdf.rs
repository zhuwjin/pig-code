//! Pure-Rust PDF probing for the media tool path: header/EOF sniffing and the
//! input-size limit. No parsing, OCR, page rendering, or external binaries —
//! the document is forwarded to the model as opaque bytes.

/// PDF media type accepted by the provider document projection.
pub const PDF_MEDIA_TYPE: &str = "application/pdf";

/// Per-document input cap. A PDF is sent whole to the model, so the limit is
/// deliberately far below the generic media-file cap.
pub const MAX_PDF_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Header/tail probing windows. The header is only accepted at the beginning
/// (after an optional UTF-8 BOM and ASCII whitespace), so the probe only needs
/// to cover that short prefix; `%%EOF` may sit near the end of the file.
const HEADER_PROBE_BYTES: usize = 1024;
const EOF_PROBE_BYTES: usize = 2048;
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Offset of the `%PDF-` header when the buffer begins with an optional UTF-8
/// BOM followed by ASCII whitespace. Text that merely mentions the marker
/// later is not a PDF.
fn pdf_header_offset(bytes: &[u8]) -> Option<usize> {
    let mut offset = if bytes.starts_with(&UTF8_BOM) {
        UTF8_BOM.len()
    } else {
        0
    };
    while matches!(
        bytes.get(offset),
        Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
    ) {
        offset += 1;
    }
    bytes[offset..].starts_with(b"%PDF-").then_some(offset)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PdfKind {
    /// No PDF header at the start of the buffer (after the allowed prefix).
    NotPdf,
    /// A PDF header is present but the required EOF marker is missing or the
    /// buffer is too short: deterministic "corrupt/truncated" degradation.
    Truncated,
    /// Header and EOF marker both present.
    Valid,
}

/// Classify a buffer without decoding it: the header must be at the start
/// (after an optional BOM/whitespace), plus a bounded tail scan for `%%EOF`
/// (trailing whitespace/NUL padding is tolerated).
pub fn classify_pdf(bytes: &[u8]) -> PdfKind {
    let head = &bytes[..bytes.len().min(HEADER_PROBE_BYTES)];
    if pdf_header_offset(head).is_none() {
        return PdfKind::NotPdf;
    }
    let tail_start = bytes.len().saturating_sub(EOF_PROBE_BYTES);
    let tail = &bytes[tail_start..];
    if tail.windows(5).any(|window| window == b"%%EOF") {
        PdfKind::Valid
    } else {
        PdfKind::Truncated
    }
}

/// `true` only for a well-formed header + EOF pair.
pub fn sniff_pdf(bytes: &[u8]) -> bool {
    matches!(classify_pdf(bytes), PdfKind::Valid)
}

/// Whether the buffer carries a PDF header (even if truncated).
pub fn has_pdf_header(bytes: &[u8]) -> bool {
    !matches!(classify_pdf(bytes), PdfKind::NotPdf)
}

/// Provider document limits used by the token budget: the document APIs we
/// target cap a PDF at 100 pages, and one page costs roughly 1,500-3,000 input
/// tokens (extracted text plus the rendered page image). The budget uses the
/// top of that documented range. A visible page count is an advisory fast path:
/// anything else (for example pages hidden in compressed object streams) uses
/// [`unknown_document_token_reserve`] instead of an optimistic guess.
pub const MAX_DOCUMENT_PAGES: u32 = 100;
pub const PDF_TOKENS_PER_PAGE: u64 = 3_000;

/// Fraction of the model context window an unprovable page structure may
/// reserve. A single document whose page tree cannot be seen must not consume
/// the room the rest of the conversation needs: compaction would otherwise
/// discard the whole prior conversation because of uncertainty alone.
const UNKNOWN_PAGE_STRUCTURE_WINDOW_DIVISOR: u64 = 4;

/// Provider-side maximum token cost of a valid PDF (the explicit page limit at
/// the per-page upper bound). This is a real cost ceiling, not a fallback
/// constant: an unknown structure is bounded by it but never assumed to equal
/// it.
pub fn provider_document_token_ceiling() -> u64 {
    MAX_DOCUMENT_PAGES as u64 * PDF_TOKENS_PER_PAGE
}

/// Token reserve for a document whose page structure cannot be proven. The
/// provider's page ceiling still bounds the estimate, but it is also capped at
/// a quarter of the model's context window: an unknown PDF may cost up to one
/// quarter of the window, never an unbounded fixed figure. A real (visible)
/// page count is not subject to this cap — it is the provider's own cost, not
/// uncertainty.
pub fn unknown_document_token_reserve(context_window: u64) -> u64 {
    provider_document_token_ceiling()
        .min(context_window / UNKNOWN_PAGE_STRUCTURE_WINDOW_DIVISOR)
        .max(1)
}

/// Bounded structural page count: scans the document bytes (already capped by
/// [`MAX_PDF_FILE_BYTES`]) for `/Type /Page` objects and for `/Count` inside a
/// `/Type /Pages` page tree. No decompression, rendering, or external parser.
///
/// The scan is advisory: only a visible page-tree `/Count` is a reliable
/// lower bound, because individual `/Type /Page` objects are not proof of
/// completeness (modern PDFs often keep most pages inside compressed object
/// streams). Without a visible page-tree count this returns `None` and the
/// caller uses [`unknown_document_token_reserve`]. The result is clamped to the
/// provider cap.
pub fn page_count_bounded(bytes: &[u8]) -> Option<u32> {
    if bytes.len() as u64 > MAX_PDF_FILE_BYTES {
        return None;
    }
    let mut page_objects = 0u32;
    let mut tree_count: Option<u32> = None;
    let mut offset = 0usize;
    while let Some(relative) = find_bytes(&bytes[offset..], b"/Type") {
        let after_type = offset + relative + b"/Type".len();
        let Some(name) = read_name_token(bytes, after_type) else {
            offset = after_type;
            continue;
        };
        if name == b"Page" {
            page_objects = page_objects.saturating_add(1);
        } else if name == b"Pages" {
            // `/Count` belongs to the page tree; an outline's `/Count` can be
            // much larger and must not inflate the document reserve.
            let window_end = (after_type + 512).min(bytes.len());
            if let Some(count) = count_in_window(&bytes[after_type..window_end]) {
                tree_count = Some(tree_count.map_or(count, |old| old.max(count)));
            }
        }
        offset = after_type;
    }
    // No visible page-tree count: the structure is not provably complete, even
    // when a few page objects happened to be uncompressed.
    let tree_count = tree_count?;
    // A visible (if empty/invalid) page tree still costs at least one page;
    // page objects beyond the reported count only make the reserve safer.
    let pages = tree_count.max(page_objects).max(1);
    Some(pages.min(MAX_DOCUMENT_PAGES))
}

/// Provenance of a computed PDF token cost. `Measured` is the provider's own
/// page-tree cost and never changes; `WindowFallback` is the bounded reserve
/// for an unprovable structure and must be recomputed (never re-scanned) when
/// the consuming model's context window changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PdfTokenCost {
    Measured(u64),
    WindowFallback(u64),
}

impl PdfTokenCost {
    pub fn tokens(self) -> u64 {
        match self {
            Self::Measured(tokens) | Self::WindowFallback(tokens) => tokens,
        }
    }
}

/// Token cost of one PDF document, computed once when the document is loaded
/// or persisted. A visible page-tree count yields the measured per-page cost;
/// anything the bounded scan cannot prove (missing/unreadable header, oversized
/// bytes, or pages hidden in compressed object streams) uses the bounded,
/// window-aware [`unknown_document_token_reserve`] and is tagged as such. The
/// caller caches the result so later context estimates never decode or scan the
/// document again; only a window fallback is repriced from its tag.
pub fn document_token_cost(bytes: &[u8], context_window: u64) -> PdfTokenCost {
    if !has_pdf_header(bytes) || bytes.len() as u64 > MAX_PDF_FILE_BYTES {
        return PdfTokenCost::WindowFallback(unknown_document_token_reserve(context_window));
    }
    match page_count_bounded(bytes) {
        Some(pages) => PdfTokenCost::Measured(pages.max(1) as u64 * PDF_TOKENS_PER_PAGE),
        None => PdfTokenCost::WindowFallback(unknown_document_token_reserve(context_window)),
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn read_name_token(bytes: &[u8], start: usize) -> Option<&[u8]> {
    let mut index = start;
    while matches!(
        bytes.get(index),
        Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
    ) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'/') {
        return None;
    }
    index += 1;
    let name_start = index;
    while bytes.get(index).is_some_and(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'#' | b'-' | b'+' | b'_')
    }) {
        index += 1;
    }
    (index > name_start).then_some(&bytes[name_start..index])
}

fn count_in_window(window: &[u8]) -> Option<u32> {
    let mut offset = 0usize;
    while let Some(relative) = find_bytes(&window[offset..], b"/Count") {
        let mut index = offset + relative + b"/Count".len();
        while matches!(
            window.get(index),
            Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
        ) {
            index += 1;
        }
        let start = index;
        while window.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index > start
            && let Ok(value) = std::str::from_utf8(&window[start..index])
            && let Ok(count) = value.parse::<u32>()
        {
            return Some(count);
        }
        offset = start.max(offset + relative + 1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf(body: &str) -> Vec<u8> {
        format!("%PDF-1.7\n{body}\n%%EOF\n").into_bytes()
    }

    #[test]
    fn valid_header_and_eof_classify() {
        assert!(sniff_pdf(&pdf("1 0 obj << >> endobj")));
        assert_eq!(classify_pdf(&pdf("1 0 obj << >> endobj")), PdfKind::Valid);
        // Whitespace/NUL padding after EOF is tolerated.
        let mut padded = pdf("x");
        padded.extend_from_slice(b"\n\n\0\0");
        assert!(sniff_pdf(&padded));
    }

    #[test]
    fn header_without_eof_is_truncated_not_valid() {
        let mut bytes = pdf("body");
        bytes.truncate(bytes.len() - 6);
        assert_eq!(classify_pdf(&bytes), PdfKind::Truncated);
        assert!(!sniff_pdf(&bytes));
        assert!(has_pdf_header(&bytes));
    }

    #[test]
    fn non_pdf_inputs_are_rejected() {
        assert_eq!(classify_pdf(b"hello world"), PdfKind::NotPdf);
        assert_eq!(classify_pdf(&[]), PdfKind::NotPdf);
        assert_eq!(classify_pdf(b"%PD"), PdfKind::NotPdf);
        // A header marker in the tail only does not count.
        let mut bytes = vec![b'x'; 4096];
        bytes.extend_from_slice(b"%PDF-1.7 %%EOF");
        assert_eq!(classify_pdf(&bytes), PdfKind::NotPdf);
    }

    #[test]
    fn header_may_follow_bom_and_whitespace() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b" \r\n\t");
        bytes.extend_from_slice(&pdf("body"));
        assert!(sniff_pdf(&bytes));

        let mut offset = vec![b' '; 900];
        offset.extend_from_slice(&pdf("body"));
        assert!(
            sniff_pdf(&offset),
            "a legal whitespace offset still classifies"
        );

        let mut beyond_probe = vec![b' '; HEADER_PROBE_BYTES];
        beyond_probe.extend_from_slice(&pdf("body"));
        assert_eq!(
            classify_pdf(&beyond_probe),
            PdfKind::NotPdf,
            "the header must appear inside the bounded prefix probe"
        );
    }

    /// Ordinary text that embeds the marker bytes (e.g. this module's own
    /// source) must stay text: only a start-of-file header counts.
    #[test]
    fn text_with_pdf_markers_is_not_pdf() {
        let mut text = Vec::new();
        text.extend_from_slice(b"//! PDF probing documentation\nfn probe() {}\n");
        text.extend_from_slice(b"const HEADER: &[u8] = b\"%PDF-\";\n");
        text.extend(std::iter::repeat_n(b'x', 2048));
        text.extend_from_slice(b"\n// %%EOF is the trailer marker\n");
        assert_eq!(classify_pdf(&text), PdfKind::NotPdf);
        assert!(!sniff_pdf(&text));
        assert!(!has_pdf_header(&text));
    }

    #[test]
    fn eof_probe_is_bounded() {
        let mut bytes = pdf("body");
        bytes.extend(std::iter::repeat_n(b'x', EOF_PROBE_BYTES + 8));
        assert_eq!(
            classify_pdf(&bytes),
            PdfKind::Truncated,
            "an EOF marker beyond the bounded tail probe is not trusted"
        );
    }

    fn multi_page_pdf(pages: u32) -> Vec<u8> {
        let mut body = String::from("%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
        body.push_str(&format!("2 0 obj << /Type /Pages /Count {pages} /Kids ["));
        for page in 0..pages {
            body.push_str(&format!("{} 0 R ", page + 3));
        }
        body.push_str("] >> endobj\n");
        for page in 0..pages {
            body.push_str(&format!(
                "{} 0 obj << /Type /Page /Parent 2 0 R >> endobj\n",
                page + 3
            ));
        }
        body.push_str("%%EOF\n");
        body.into_bytes()
    }

    const WINDOW: u64 = 128_000;

    #[test]
    fn page_count_uses_page_tree_and_page_objects() {
        let three = multi_page_pdf(3);
        assert!(sniff_pdf(&three));
        assert_eq!(page_count_bounded(&three), Some(3));
        assert_eq!(
            document_token_cost(&three, WINDOW),
            PdfTokenCost::Measured(3 * PDF_TOKENS_PER_PAGE)
        );

        // Only individual page objects, no page-tree `/Count`: the structure
        // cannot be proven complete (pages may be hidden in object streams),
        // so the window-aware fallback applies.
        let body = format!(
            "%PDF-1.7\n{}%%EOF\n",
            (1..=2)
                .map(|page| format!("{page} 0 obj << /Type /Page >> endobj\n"))
                .collect::<String>()
        );
        assert_eq!(page_count_bounded(body.as_bytes()), None);
        assert_eq!(
            document_token_cost(body.as_bytes(), WINDOW),
            PdfTokenCost::WindowFallback(unknown_document_token_reserve(WINDOW))
        );

        // A visible but empty page tree still reserves one page rather than
        // falling into the opaque-structure fallback.
        let empty = b"%PDF-1.4\n1 0 obj << /Type /Pages /Kids [] /Count 0 >> endobj\n%%EOF\n";
        assert_eq!(page_count_bounded(empty), Some(1));
        assert_eq!(
            document_token_cost(empty, WINDOW),
            PdfTokenCost::Measured(PDF_TOKENS_PER_PAGE)
        );
    }

    /// A partially compressed PDF can expose one `/Type /Page` while the rest
    /// live inside an object stream. The visible page object must not become a
    /// one-page estimate; the incomplete structure uses the fallback.
    #[test]
    fn partially_hidden_pages_use_the_window_aware_reserve() {
        let body = "%PDF-1.7\n\
                    1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n\
                    2 0 obj << /Length 0 >> stream\n/Type /ObjStm\nendstream endobj\n\
                    3 0 obj << /Type /Page /Parent 2 0 R >> endobj\n%%EOF\n";
        assert_eq!(
            page_count_bounded(body.as_bytes()),
            None,
            "one visible page object is not proof of completeness"
        );
        assert_eq!(
            document_token_cost(body.as_bytes(), WINDOW),
            PdfTokenCost::WindowFallback(unknown_document_token_reserve(WINDOW))
        );
    }

    /// An outline's `/Count` is not a page count; only `/Count` near a
    /// `/Type /Pages` dictionary may set the reserve.
    #[test]
    fn outline_counts_do_not_inflate_the_page_count() {
        let body = "%PDF-1.7\n1 0 obj << /Type /Outlines /Count 999 >> endobj\n\
                    2 0 obj << /Type /Pages /Count 2 >> endobj\n\
                    3 0 obj << /Type /Page >> endobj\n\
                    4 0 obj << /Type /Page >> endobj\n%%EOF\n";
        assert_eq!(page_count_bounded(body.as_bytes()), Some(2));
    }

    /// The unknown-structure reserve scales with the model window but never
    /// exceeds the provider's page ceiling. It is not a fixed token constant:
    /// a small window reserves a small share, a huge window stops at the
    /// provider maximum. This keeps uncertainty from consuming the whole
    /// context and forcing unrelated prior conversation to be compacted.
    #[test]
    fn unknown_structure_reserve_is_window_aware_and_bounded() {
        let bytes = pdf("1 0 obj << /Length 0 >> stream\n/Type /ObjStm\nendstream");
        assert_eq!(page_count_bounded(&bytes), None);
        let provider_ceiling = provider_document_token_ceiling();
        assert_eq!(unknown_document_token_reserve(8_000), 2_000);
        assert_eq!(unknown_document_token_reserve(128_000), 32_000);
        assert_eq!(unknown_document_token_reserve(400_000), 100_000);
        assert_eq!(
            unknown_document_token_reserve(8_000_000),
            provider_ceiling,
            "the provider page ceiling still bounds the fallback"
        );
        for window in [1_000, 8_000, 128_000, 400_000, 8_000_000] {
            let reserve = unknown_document_token_reserve(window);
            assert!(reserve > 0, "every window reserves something");
            assert!(
                reserve <= provider_ceiling,
                "never exceeds the provider's page ceiling"
            );
            assert!(
                reserve <= window.div_ceil(4),
                "an unknown document reserves at most a quarter of the window"
            );
        }
        assert_eq!(
            document_token_cost(&bytes, 128_000),
            PdfTokenCost::WindowFallback(unknown_document_token_reserve(128_000))
        );
    }

    /// A visible page count is the provider's real cost, not uncertainty: it
    /// is clamped to the explicit provider page limit and is not reduced by a
    /// small model window.
    #[test]
    fn provider_page_limit_still_bounds_visible_page_trees() {
        let oversized = multi_page_pdf(150);
        assert_eq!(
            page_count_bounded(&oversized),
            Some(MAX_DOCUMENT_PAGES),
            "the provider accepts at most 100 pages"
        );
        assert_eq!(
            document_token_cost(&oversized, 8_000),
            PdfTokenCost::Measured(provider_document_token_ceiling())
        );
    }

    #[test]
    fn oversize_documents_use_the_window_aware_reserve() {
        let mut bytes = multi_page_pdf(2);
        bytes.resize(MAX_PDF_FILE_BYTES as usize + 1, b' ');
        assert_eq!(
            document_token_cost(&bytes, WINDOW),
            PdfTokenCost::WindowFallback(unknown_document_token_reserve(WINDOW))
        );
    }
}
