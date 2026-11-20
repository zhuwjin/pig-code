//! Indexed streaming tool-call accumulation for Anthropic content blocks and
//! Responses output items. Chat Completions uses the correlation-aware
//! ChatToolStream instead.
//!
//! Live-argument events follow ZCode's model (streaming-tool-input-preview +
//! the newline flush in its agent core): a `ToolCallStart` fires once a call's
//! id+name are known, argument fragments batch until a line break (a real
//! `\n` or a JSON-escaped `\n` — the Write/Edit "line by line" experience
//! rides on newlines reaching the UI fast) or a 4KB fallback for long
//! single-line arguments, and `finish` flushes the tail and emits the
//! authoritative `ToolCalls` list via the existing boundary in chat.rs.

use crate::chat::ToolCall;
use crate::events::ProviderEvent;

/// Flush threshold when a batch carries no line break (a long single-line
/// argument must not buffer until stream end) — ZCode's 4KB fallback
const DELTA_FLUSH_CHARS: usize = 4096;

#[derive(Default)]
pub(crate) struct ToolCallStream {
    /// Accumulated calls keyed by the content-block/output index; sparse
    /// defaults for non-tool blocks are filtered as nameless at finish.
    calls: Vec<ToolCall>,
    /// Whether ToolCallStart was sent per index (id+name were complete)
    started: Vec<bool>,
    /// Unflushed argument fragment per index (always break-free between
    /// pushes: a batch flushes on every line break it brings)
    buffers: Vec<String>,
    /// Whether the authoritative done item closed this call. Late fragments
    /// and repeated finalizes are ignored: the durable call and its replay
    /// must never change after the done boundary.
    finalized: Vec<bool>,
}

impl ToolCallStream {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn grow_to(&mut self, index: usize) {
        while self.calls.len() <= index {
            self.calls.push(ToolCall::default());
            self.started.push(false);
            self.buffers.push(String::new());
            self.finalized.push(false);
        }
    }

    /// One wire fragment: id/name/arguments may each be absent (Chat deltas
    /// repeat only what changed), or id+name arrive together (Anthropic
    /// content_block_start, Responses output_item.added). Sends ToolCallStart
    /// once id+name are complete, ToolCallDelta per flushed batch.
    pub(crate) async fn push(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
        tx: &tokio::sync::mpsc::Sender<ProviderEvent>,
    ) {
        self.grow_to(index);
        if self.finalized[index] {
            tracing::warn!(
                index,
                "ignoring a tool-call fragment that arrived after finalization"
            );
            return;
        }
        let call = &mut self.calls[index];
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            call.id.push_str(id);
        }
        if let Some(name) = name.filter(|s| !s.is_empty()) {
            call.name.push_str(name);
        }
        if !self.started[index] && !call.id.is_empty() && !call.name.is_empty() {
            self.started[index] = true;
            let _ = tx
                .send(ProviderEvent::ToolCallStart {
                    index,
                    id: call.id.clone(),
                    name: call.name.clone(),
                })
                .await;
        }
        if let Some(fragment) = arguments.filter(|s| !s.is_empty()) {
            // Authoritative accumulation (the final ToolCalls value) plus the
            // live-event batching copy: the buffer only decides when a Delta
            // event goes out, the call always carries the whole text
            self.calls[index].arguments.push_str(fragment);
            let buffer = &mut self.buffers[index];
            buffer.push_str(fragment);
            // The buffer holds no break between pushes (it flushes on every
            // break it brings), so only the new fragment needs scanning
            if has_line_break(fragment) || buffer.len() >= DELTA_FLUSH_CHARS {
                let _ = tx
                    .send(ProviderEvent::ToolCallDelta {
                        index,
                        delta: std::mem::take(buffer),
                    })
                    .await;
            }
        }
    }

    /// Authoritative reconciliation at `output_item.done` (Responses). The
    /// event order a live consumer saw is: Start, flushed batched deltas,
    /// possibly one unflushed tail. Finalization must therefore:
    /// 1. start the call if `output_item.added` was skipped,
    /// 2. flush the unflushed streamed tail (it is real live content, never
    ///    stale: it has not been sent yet),
    /// 3. use a non-empty authoritative value for execution/replay, emitting
    ///    only the suffix beyond the streamed prefix when the two reconcile,
    /// 4. keep the streamed value when the authoritative arguments are empty.
    ///
    /// Any later fragment or repeated finalize is ignored, so the durable call
    /// can never change after the done boundary.
    pub(crate) async fn finalize(
        &mut self,
        index: usize,
        call: ToolCall,
        tx: &tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> ToolCall {
        self.grow_to(index);
        if self.finalized[index] {
            tracing::warn!(index, "ignoring a repeated tool-call finalization");
            return self.calls[index].clone();
        }
        let (existing_id, existing_name, streamed) = {
            let existing = &self.calls[index];
            (
                existing.id.clone(),
                existing.name.clone(),
                existing.arguments.clone(),
            )
        };
        let mut effective = call;
        if effective.id.is_empty() {
            effective.id = existing_id;
        }
        if effective.name.trim().is_empty() {
            effective.name = existing_name;
        }
        if !self.started[index] && !effective.id.is_empty() && !effective.name.is_empty() {
            self.started[index] = true;
            let _ = tx
                .send(ProviderEvent::ToolCallStart {
                    index,
                    id: effective.id.clone(),
                    name: effective.name.clone(),
                })
                .await;
        }

        // The unflushed tail is part of the stream the UI is already showing.
        // Send it before the authoritative value so no live content is lost.
        let pending = std::mem::take(&mut self.buffers[index]);
        if self.started[index] && !pending.is_empty() {
            let _ = tx
                .send(ProviderEvent::ToolCallDelta {
                    index,
                    delta: pending,
                })
                .await;
        }

        if effective.arguments.trim().is_empty() {
            // Empty authoritative arguments never erase streamed input.
            effective.arguments = streamed;
        } else if !streamed.is_empty() && effective.arguments.starts_with(&streamed) {
            // The server repeated what we streamed and may append a final
            // suffix that never arrived as a delta. Emit exactly that suffix;
            // replaying the whole value would duplicate the live content.
            let suffix = &effective.arguments[streamed.len()..];
            if self.started[index] && !suffix.is_empty() {
                let _ = tx
                    .send(ProviderEvent::ToolCallDelta {
                        index,
                        delta: suffix.to_string(),
                    })
                    .await;
            }
        }
        // If the authoritative value does not extend the streamed prefix it
        // irreconcilably replaces the live view; execution and replay use the
        // authoritative call, while every streamed fragment was still sent.

        self.finalized[index] = true;
        self.calls[index] = effective.clone();
        effective
    }

    /// Flush the remaining unbreak batches as final deltas (the tail of the
    /// last argument line). Finalized calls never emit again.
    pub(crate) async fn flush(&mut self, tx: &tokio::sync::mpsc::Sender<ProviderEvent>) {
        for index in 0..self.buffers.len() {
            if self.finalized[index] {
                self.buffers[index].clear();
                continue;
            }
            if self.started[index] && !self.buffers[index].is_empty() {
                let _ = tx
                    .send(ProviderEvent::ToolCallDelta {
                        index,
                        delta: std::mem::take(&mut self.buffers[index]),
                    })
                    .await;
            }
        }
    }

    pub(crate) fn calls(&self) -> &[ToolCall] {
        &self.calls
    }

    /// Stream end: flush the tail, then the durable boundary (filter indexed
    /// placeholders, ToolCalls, Finished) from chat.rs
    pub(crate) async fn finish(
        &mut self,
        tx: &tokio::sync::mpsc::Sender<ProviderEvent>,
        reason: pig_protocol::FinishReason,
    ) {
        self.flush(tx).await;
        let calls = std::mem::take(&mut self.calls);
        crate::chat::finish(tx, calls.clone(), reason).await;
        self.calls = calls;
    }
}

/// A batch boundary: a real line break or its JSON-escaped form (arguments
/// stream as raw JSON text, where Write/Edit content newlines arrive as the
/// two characters `\` + `n`)
fn has_line_break(fragment: &str) -> bool {
    fragment.contains('\n')
        || fragment.contains('\r')
        || fragment.contains("\\n")
        || fragment.contains("\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! push {
        ($stream:expr, $($args:tt)*) => {
            ($stream).push($($args)*).await
        };
    }
    macro_rules! finalize {
        ($stream:expr, $($args:tt)*) => {
            ($stream).finalize($($args)*).await
        };
    }
    macro_rules! finish {
        ($stream:expr, $tx:expr, $reason:expr) => {
            ($stream).finish($tx, $reason).await
        };
    }

    fn collect(rx: &mut tokio::sync::mpsc::Receiver<ProviderEvent>) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    fn started(stream: &ToolCallStream) -> Vec<bool> {
        stream.started.clone()
    }

    /// Start fires once id+name are complete; argument fragments batch per
    /// line break (break-free fragments merge into the next break-bearing
    /// batch); finish flushes the tail and emits ToolCalls + Finished with the
    /// fully accumulated arguments
    #[tokio::test]
    async fn batches_on_line_break_and_flushes_tail() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        // First Chat-style chunk: id+name, no arguments yet
        push!(&mut stream, 0, Some("call_1"), Some("Write"), None, &tx);
        // Fragments of {"path":"a.txt","content":"l1\nl2"} serialized compact:
        // the first carries no break, so it merges into the second (which
        // ends with the escaped \n) as one flushed batch
        push!(
            &mut stream,
            0,
            None,
            None,
            Some(r#"{"path":"a.txt","content":"l1"#),
            &tx
        );
        push!(&mut stream, 0, None, None, Some(r#"\nl2"#), &tx);
        // Tail without a break stays buffered
        push!(&mut stream, 0, None, None, Some(r#""}"#), &tx);
        let events = collect(&mut rx);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolCallStart { index: 0, id, name } if id == "call_1" && name == "Write"
        ));
        assert!(matches!(
            &events[1],
            ProviderEvent::ToolCallDelta { index: 0, .. }
        ));
        finish!(&mut stream, &tx, pig_protocol::FinishReason::Stop);
        let events = collect(&mut rx);
        match &events[..] {
            [
                ProviderEvent::ToolCallDelta { delta, .. },
                ProviderEvent::ToolCalls(calls),
                ProviderEvent::Finished(_),
            ] => {
                assert_eq!(delta, r#""}"#);
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].arguments, r#"{"path":"a.txt","content":"l1\nl2"}"#);
            }
            other => panic!("unexpected finish sequence: {other:?}"),
        }
    }

    /// A break-free fragment at or past 4KB flushes without waiting for a
    /// newline (long single-line arguments)
    #[tokio::test]
    async fn size_fallback_flushes_long_single_line() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        push!(&mut stream, 0, Some("c"), Some("ExecCommand"), None, &tx);
        let long = "x".repeat(DELTA_FLUSH_CHARS);
        push!(&mut stream, 0, None, None, Some(&long), &tx);
        let events = collect(&mut rx);
        assert!(matches!(
            &events[..],
            [_, ProviderEvent::ToolCallDelta { delta, .. }] if delta.len() == DELTA_FLUSH_CHARS
        ));
    }

    /// A nameless call never starts (no live events) and finish filters it
    /// out of the authoritative list
    #[tokio::test]
    async fn nameless_never_starts_and_finish_filters() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        push!(
            &mut stream,
            0,
            Some("call_ok"),
            Some("Read"),
            Some("{}"),
            &tx
        );
        // Broken model output: a second call with an id but no name
        push!(&mut stream, 1, Some("call_bad"), None, Some("{}"), &tx);
        assert_eq!(started(&stream), vec![true, false]);
        finish!(&mut stream, &tx, pig_protocol::FinishReason::Stop);
        let events = collect(&mut rx);
        match &events[..] {
            [
                ProviderEvent::ToolCallStart { .. },
                ProviderEvent::ToolCallDelta { .. },
                ProviderEvent::ToolCalls(calls),
                ProviderEvent::Finished(_),
            ] => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_ok");
                assert_eq!(calls[0].arguments, "{}");
            }
            other => panic!("unexpected finish sequence: {other:?}"),
        }
    }

    fn delta_text(events: &[ProviderEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    }

    /// A Start still fires when added was skipped; the streamed prefix is
    /// flushed before the authoritative call replaces it.
    #[tokio::test]
    async fn finalize_starts_and_flushes_when_added_was_skipped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        push!(&mut stream, 0, None, None, Some(r#"{"path":"partial"#), &tx);
        assert!(collect(&mut rx).is_empty());
        let call = finalize!(
            &mut stream,
            0,
            ToolCall {
                id: "call_r".into(),
                name: "Read".into(),
                arguments: r#"{"path":"complete.txt"}"#.into(),
            },
            &tx,
        );
        assert_eq!(call.arguments, r#"{"path":"complete.txt"}"#);
        let events = collect(&mut rx);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolCallStart { id, name, .. } if id == "call_r" && name == "Read"
        ));
        assert_eq!(
            delta_text(&events),
            r#"{"path":"partial"#,
            "the streamed prefix is not lost when the done value does not extend it"
        );
    }

    /// The done item may repeat exactly what was streamed. The unflushed tail
    /// must be emitted (it was never sent), and no byte is repeated.
    #[tokio::test]
    async fn authoritative_done_reconciles_the_buffered_tail_exactly_once() {
        let full = r#"{"content":"line\n","path":"a.txt"}"#;
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        push!(
            &mut stream,
            0,
            Some("call_1"),
            Some("Write"),
            Some(r#"{"content":"line\n"#),
            &tx,
        );
        push!(
            &mut stream,
            0,
            None,
            None,
            Some(r#"","path":"a.txt"}"#),
            &tx
        );
        let call = finalize!(
            &mut stream,
            0,
            ToolCall {
                id: "call_1".into(),
                name: "Write".into(),
                arguments: full.into(),
            },
            &tx,
        );
        assert_eq!(call.arguments, full);
        finish!(&mut stream, &tx, pig_protocol::FinishReason::ToolCalls);
        let events = collect(&mut rx);
        assert_eq!(
            delta_text(&events),
            full,
            "streamed deltas plus the flushed tail equal the call exactly once: {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::Finished(
                pig_protocol::FinishReason::ToolCalls
            ))
        ));
    }

    /// Done-before-flush: a break-free streamed tail is buffered, then the done
    /// item supplies a value that extends it. The tail and the missing suffix
    /// are emitted once each.
    #[tokio::test]
    async fn authoritative_done_extends_streamed_prefix_with_the_missing_suffix() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        push!(
            &mut stream,
            0,
            Some("call_1"),
            Some("Read"),
            Some(r#"{"path":"a"#),
            &tx
        );
        let call = finalize!(
            &mut stream,
            0,
            ToolCall {
                id: "call_1".into(),
                name: "Read".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            },
            &tx,
        );
        assert_eq!(call.arguments, r#"{"path":"a.txt"}"#);
        finish!(&mut stream, &tx, pig_protocol::FinishReason::ToolCalls);
        let events = collect(&mut rx);
        assert_eq!(
            delta_text(&events),
            r#"{"path":"a.txt"}"#,
            "prefix + suffix reach the final value without duplication: {events:?}"
        );
        let calls_at = events
            .iter()
            .position(|event| matches!(event, ProviderEvent::ToolCalls(_)))
            .expect("tool calls");
        assert!(
            events[..calls_at]
                .iter()
                .filter(|event| matches!(event, ProviderEvent::ToolCallDelta { .. }))
                .count()
                == 2,
            "the buffered tail and suffix flush before the execution boundary: {events:?}"
        );
    }

    /// Flush-before-done: an already-flushed fragment must not be emitted a
    /// second time at finalization.
    #[tokio::test]
    async fn authoritative_done_after_a_flush_adds_nothing() {
        let full = r#"{"content":"line\n","path":"a.txt"}"#;
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        push!(
            &mut stream,
            0,
            Some("call_1"),
            Some("Write"),
            Some(full),
            &tx
        );
        let call = finalize!(
            &mut stream,
            0,
            ToolCall {
                id: "call_1".into(),
                name: "Write".into(),
                arguments: full.into(),
            },
            &tx,
        );
        assert_eq!(call.arguments, full);
        finish!(&mut stream, &tx, pig_protocol::FinishReason::ToolCalls);
        let events = collect(&mut rx);
        assert_eq!(delta_text(&events), full);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderEvent::ToolCallDelta { .. }))
                .count(),
            1,
            "the flushed fragment is sent once: {events:?}"
        );
        let calls_at = events
            .iter()
            .position(|event| matches!(event, ProviderEvent::ToolCalls(_)))
            .expect("tool calls");
        assert!(
            events[..calls_at]
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolCallDelta { .. })),
            "the already-flushed delta precedes the execution boundary: {events:?}"
        );
    }

    /// A done item with empty arguments must not erase streamed input: the
    /// effective call (and therefore the durable replay) keeps the accumulated
    /// arguments, and the pending tail is still a valid live delta.
    #[tokio::test]
    async fn empty_authoritative_done_arguments_keep_streamed_input() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        push!(
            &mut stream,
            0,
            Some("call_1"),
            Some("Read"),
            Some(r#"{"path":"a"}"#),
            &tx,
        );
        let call = finalize!(
            &mut stream,
            0,
            ToolCall {
                id: "call_1".into(),
                name: "Read".into(),
                arguments: String::new(),
            },
            &tx,
        );
        assert_eq!(call.arguments, r#"{"path":"a"}"#);
        assert_eq!(stream.calls()[0].arguments, r#"{"path":"a"}"#);
        finish!(&mut stream, &tx, pig_protocol::FinishReason::ToolCalls);
        let events = collect(&mut rx);
        assert_eq!(delta_text(&events), r#"{"path":"a"}"#);
        assert!(
            events.iter().any(
                |event| matches!(event, ProviderEvent::ToolCalls(calls) if calls[0].arguments == r#"{"path":"a"}"#)
            ),
            "the authoritative list keeps the input: {events:?}"
        );
    }

    /// A non-prefix authoritative value replaces execution/replay, but every
    /// streamed fragment is still delivered live. Late fragments and repeated
    /// finalizes cannot mutate the closed call.
    #[tokio::test]
    async fn non_prefix_done_and_late_fragments_never_mutate_the_closed_call() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        push!(
            &mut stream,
            0,
            Some("call_1"),
            Some("Read"),
            Some(r#"{"old":"#),
            &tx
        );
        let call = finalize!(
            &mut stream,
            0,
            ToolCall {
                id: "call_1".into(),
                name: "Read".into(),
                arguments: r#"{"new":true}"#.into(),
            },
            &tx,
        );
        assert_eq!(call.arguments, r#"{"new":true}"#);
        let after_finalize = collect(&mut rx);

        push!(
            &mut stream,
            0,
            Some("late"),
            Some("Wrong"),
            Some("LEAKED"),
            &tx
        );
        let repeated = finalize!(
            &mut stream,
            0,
            ToolCall {
                id: "call_2".into(),
                name: "Other".into(),
                arguments: r#"{"other":true}"#.into(),
            },
            &tx,
        );
        assert_eq!(repeated, call, "a repeated done cannot change the call");
        let late = collect(&mut rx);
        assert!(late.is_empty(), "late fragments leak no events: {late:?}");
        assert_eq!(delta_text(&after_finalize), r#"{"old":"#);
        assert_eq!(stream.calls()[0].arguments, r#"{"new":true}"#);
        assert_eq!(stream.calls()[0].id, "call_1");
        assert_eq!(stream.calls()[0].name, "Read");
        finish!(&mut stream, &tx, pig_protocol::FinishReason::ToolCalls);
        let events = collect(&mut rx);
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, ProviderEvent::ToolCallDelta { .. })),
            "no buffered or late tail after finalization: {events:?}"
        );
    }

    /// The Anthropic sparse-index shape: a non-tool block leaves a nameless
    /// default that never starts and is filtered at finish
    #[tokio::test]
    async fn sparse_block_indices_keep_mapping() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ToolCallStream::new();
        // Text block at index 0 is never pushed; tool_use starts at index 1
        push!(&mut stream, 1, Some("toolu_1"), Some("Read"), None, &tx);
        push!(&mut stream, 1, None, None, Some("{}"), &tx);
        finish!(&mut stream, &tx, pig_protocol::FinishReason::Stop);
        let events = collect(&mut rx);
        match &events[..] {
            [
                ProviderEvent::ToolCallStart { index, .. },
                ProviderEvent::ToolCallDelta { .. },
                ProviderEvent::ToolCalls(calls),
                ProviderEvent::Finished(_),
            ] => {
                assert_eq!(*index, 1);
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "toolu_1");
            }
            other => panic!("unexpected finish sequence: {other:?}"),
        }
    }
}
