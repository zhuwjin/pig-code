//! Spec-level SSE decoding (WHATWG EventSource semantics, aligned with
//! `eventsource-parser` — the package the Vercel AI SDK composes under its
//! providers): line terminators `\n` / `\r\n` / bare `\r` (a chunk-trailing
//! `\r` defers dispatch to the next chunk to tell CRLF from CR), one optional
//! leading space after the field colon, multi-line `data:` fields joined with
//! `\n`, comment lines (`:`-prefixed) ignored, events with no `data:` line
//! never dispatched, incremental UTF-8 across chunk boundaries (encoding_rs —
//! a network-split multibyte char must not error), one leading BOM stripped.
//!
//! Like the SDK, a final event not terminated by a blank line at EOF is
//! dropped, and `[DONE]` filtering is the caller's business (the SDK matches
//! the exact payload string in its transform layer, not in the parser).

/// One dispatched SSE event. `event` is the `event:` field ("" when absent) —
/// dispatch should key off the payload's own `type` field when there is one
/// (the SDK ignores `event:` entirely; Anthropic payloads always carry `type`).
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

pub struct SseDecoder {
    utf8: encoding_rs::Decoder,
    /// Decoded-but-unprocessed text (decode target; consumed per push)
    text: String,
    /// Current partial line
    line: String,
    /// A trailing \r whose line ending (CR vs CRLF) the next chunk decides
    pending_cr: bool,
    started: bool,
    data: String,
    event: String,
    has_data: bool,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SseDecoder {
    pub fn new() -> Self {
        Self {
            utf8: encoding_rs::UTF_8.new_decoder(),
            text: String::new(),
            line: String::new(),
            pending_cr: false,
            started: false,
            data: String::new(),
            event: String::new(),
            has_data: false,
        }
    }

    /// Feed one network chunk; returns the events completed by it.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        // decode_to_string writes only into spare capacity (it never grows the
        // String itself — without the reserve it returns OutputFull having
        // written nothing)
        if let Some(need) = self.utf8.max_utf8_buffer_length(bytes.len()) {
            self.text.reserve(need);
        }
        // Malformed bytes decode to U+FFFD (lenient, like the platform's
        // TextDecoderStream); the result only reports that, safe to ignore
        let _ = self.utf8.decode_to_string(bytes, &mut self.text, false);
        let mut text = std::mem::take(&mut self.text);
        // One leading BOM (even when split across chunks it decodes to one char)
        if !self.started {
            self.started = true;
            if text.starts_with('\u{feff}') {
                text.remove(0);
            }
        }
        let mut out = Vec::new();
        for ch in text.chars() {
            if self.pending_cr {
                // The \r terminated the line; \n right after is the LF of CRLF
                self.pending_cr = false;
                self.process_line(&mut out);
                if ch == '\n' {
                    continue;
                }
            }
            match ch {
                '\r' => self.pending_cr = true,
                '\n' => self.process_line(&mut out),
                _ => self.line.push(ch),
            }
        }
        out
    }

    fn process_line(&mut self, out: &mut Vec<SseEvent>) {
        let line = std::mem::take(&mut self.line);
        // Blank line = event boundary
        if line.is_empty() {
            if self.has_data {
                out.push(SseEvent {
                    event: std::mem::take(&mut self.event),
                    data: std::mem::take(&mut self.data),
                });
                self.has_data = false;
            } else {
                self.event.clear();
            }
            return;
        }
        // Comment line (keep-alive heartbeats)
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let value = &line[i + 1..];
                // Exactly one optional leading space after the colon
                (&line[..i], value.strip_prefix(' ').unwrap_or(value))
            }
            None => (line.as_str(), ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.event = value.to_string(),
            // id:/retry:/unknown fields: ignored
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(decoder: &mut SseDecoder, chunks: &[&[u8]]) -> Vec<(String, String)> {
        chunks
            .iter()
            .flat_map(|c| decoder.push(c))
            .map(|e| (e.event, e.data))
            .collect()
    }

    #[test]
    fn basic_event_and_space_stripping() {
        let mut d = SseDecoder::new();
        // Both `data: x` and `data:x` are legal
        assert_eq!(
            collect(&mut d, &[b"data: one\n\ndata:two\n\n"]),
            vec![("".into(), "one".into()), ("".into(), "two".into())]
        );
    }

    #[test]
    fn all_three_line_terminators() {
        let mut d = SseDecoder::new();
        assert_eq!(
            collect(&mut d, &[b"data: a\r\n\r\ndata: b\r\rdata: c\n\n"]),
            vec![
                ("".into(), "a".into()),
                ("".into(), "b".into()),
                ("".into(), "c".into()),
            ]
        );
    }

    #[test]
    fn chunk_trailing_cr_defers_to_next_chunk() {
        let mut d = SseDecoder::new();
        // "\r" at the chunk end must NOT dispatch yet: it may be half of a CRLF
        assert!(d.push(b"data: a\r").is_empty());
        // The next chunk starts with \n -> the pair is one CRLF, one event total
        assert_eq!(d.push(b"\n\n")[0].data, "a");
    }

    #[test]
    fn bare_cr_terminates_lines() {
        let mut d = SseDecoder::new();
        // Bare \r terminates a line like \n does; a chunk-trailing \r defers
        // (it may be half of a CRLF), so the last event lands on the next push
        assert_eq!(
            collect(&mut d, &[b"data: a\r\rdata: b\r\r", b"\n"]),
            vec![("".into(), "a".into()), ("".into(), "b".into())]
        );
    }

    #[test]
    fn multiline_data_joined_with_lf() {
        let mut d = SseDecoder::new();
        assert_eq!(
            collect(&mut d, &[b"data: line1\ndata: line2\n\n"]),
            vec![("".into(), "line1\nline2".into())]
        );
    }

    #[test]
    fn comments_and_event_field() {
        let mut d = SseDecoder::new();
        assert_eq!(
            collect(
                &mut d,
                &[b": heartbeat\n\nevent: message_start\ndata: {}\n\n"]
            ),
            vec![("message_start".into(), "{}".into())]
        );
    }

    #[test]
    fn event_without_data_not_dispatched() {
        let mut d = SseDecoder::new();
        assert!(collect(&mut d, &[b"event: ping\n\n"]).is_empty());
    }

    #[test]
    fn multibyte_char_split_across_chunks() {
        let mut d = SseDecoder::new();
        let payload = "data: 密\n\n".as_bytes();
        // Split inside the 3-byte CJK char
        let split = payload.len() - 5;
        let events = collect(&mut d, &[&payload[..split], &payload[split..]]);
        assert_eq!(events, vec![("".to_string(), "密".to_string())]);
    }

    #[test]
    fn leading_bom_stripped() {
        let mut d = SseDecoder::new();
        assert_eq!(
            collect(&mut d, &[b"\xef\xbb\xbfdata: x\n\n"]),
            vec![("".into(), "x".into())]
        );
    }

    #[test]
    fn unterminated_tail_dropped_at_eof() {
        let mut d = SseDecoder::new();
        // No trailing blank line: like the SDK, the tail is not dispatched
        assert!(collect(&mut d, &[b"data: tail\n"]).is_empty());
    }
}
