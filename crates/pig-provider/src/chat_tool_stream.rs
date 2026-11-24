use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use pig_protocol::CoreError;

use crate::chat::ToolCall;
use crate::events::ProviderEvent;

const DELTA_FLUSH_BYTES: usize = 4096;
static NEXT_GENERATED_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub(crate) struct ChatToolStream {
    calls: Vec<TrackedCall>,
    by_id: HashMap<String, Vec<usize>>,
    by_index: HashMap<usize, Vec<usize>>,
    used_ids: HashSet<String>,
}

struct TrackedCall {
    call: ToolCall,
    wire_index: Option<usize>,
    sequence: usize,
    argument_state: ArgumentState,
    started: bool,
    deferred_deltas: Vec<String>,
    delta_buffer: String,
}

impl TrackedCall {
    fn new(id: String, wire_index: Option<usize>, sequence: usize, name: Option<&str>) -> Self {
        Self {
            call: ToolCall {
                id,
                name: name.unwrap_or_default().to_string(),
                arguments: String::new(),
            },
            wire_index,
            sequence,
            argument_state: ArgumentState::default(),
            started: false,
            deferred_deltas: Vec::new(),
            delta_buffer: String::new(),
        }
    }

    fn has_name(&self) -> bool {
        !self.call.name.is_empty()
    }
}

#[derive(Default)]
struct ArgumentState {
    structure: ArgumentStructure,
}

impl ArgumentState {
    fn append(&mut self, delta: &str) {
        for character in delta.chars() {
            match &mut self.structure {
                ArgumentStructure::Undetermined => {
                    if character.is_whitespace() {
                        continue;
                    }
                    if character == '{' || character == '[' {
                        self.structure = ArgumentStructure::Structured(StructuredState {
                            stack: vec![character],
                            in_string: false,
                            escaped: false,
                            complete: false,
                        });
                    } else {
                        self.structure = ArgumentStructure::Other;
                    }
                }
                ArgumentStructure::Other => {}
                ArgumentStructure::Structured(state) => {
                    if state.complete {
                        continue;
                    }
                    if state.in_string {
                        if state.escaped {
                            state.escaped = false;
                        } else if character == '\\' {
                            state.escaped = true;
                        } else if character == '"' {
                            state.in_string = false;
                        }
                        continue;
                    }
                    match character {
                        '"' => state.in_string = true,
                        '{' | '[' => state.stack.push(character),
                        '}' | ']' => {
                            let expected = if character == '}' { '{' } else { '[' };
                            if state.stack.last().copied() != Some(expected) {
                                self.structure = ArgumentStructure::Other;
                            } else {
                                state.stack.pop();
                                state.complete = state.stack.is_empty();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn has_complete_structured_value(&self) -> bool {
        matches!(
            &self.structure,
            ArgumentStructure::Structured(StructuredState { complete: true, .. })
        )
    }
}

#[derive(Default)]
enum ArgumentStructure {
    #[default]
    Undetermined,
    Other,
    Structured(StructuredState),
}

struct StructuredState {
    stack: Vec<char>,
    in_string: bool,
    escaped: bool,
    complete: bool,
}

#[derive(Clone, Copy)]
enum Resolution {
    Existing(usize),
    New,
    Ambiguous,
}

impl ChatToolStream {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn push(
        &mut self,
        wire_index: Option<usize>,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
        tx: &tokio::sync::mpsc::Sender<ProviderEvent>,
    ) {
        let wire_id = non_blank(id);
        let has_blank_name = name.is_some_and(|value| value.trim().is_empty());
        let name = non_blank(name);
        let empty_arguments = arguments.is_none_or(|value| value.trim().is_empty());
        let explicit_start =
            name.is_some() && (starts_with_structured_value(arguments) || empty_arguments);
        let resolution = self.resolve(wire_id, wire_index, name, explicit_start, empty_arguments);

        let slot = match resolution {
            Resolution::Existing(slot) => {
                if let Some(wire_id) = wire_id {
                    self.associate_id(slot, wire_id);
                }
                slot
            }
            Resolution::New => {
                let has_pending_data =
                    wire_id.is_some() || arguments.is_some_and(|fragment| !fragment.is_empty());
                let complete_blank_arguments = has_blank_name
                    && arguments.is_some_and(|fragment| {
                        let mut state = ArgumentState::default();
                        state.append(fragment);
                        state.has_complete_structured_value()
                    });
                if (has_blank_name && (!has_pending_data || complete_blank_arguments))
                    || (name.is_none() && !has_pending_data)
                {
                    return;
                }
                self.create_call(wire_id, wire_index, name)
            }
            Resolution::Ambiguous => {
                tracing::warn!(
                    wire_index = ?wire_index,
                    wire_id = ?wire_id,
                    name = ?name,
                    "Dropping ambiguous Chat Completions tool-call fragment"
                );
                return;
            }
        };

        if self.calls[slot].call.name.is_empty()
            && let Some(name) = name
        {
            self.calls[slot].call.name = name.to_string();
        }
        if !self.calls[slot].started && self.calls[slot].has_name() {
            self.start(slot, tx).await;
        }
        if let Some(index) = wire_index {
            self.associate_index(slot, index);
        }
        self.append_arguments(slot, arguments, tx).await;
    }

    pub(crate) async fn finish(
        mut self,
        tx: &tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<Vec<ToolCall>, CoreError> {
        let all_indexed = self.calls.iter().all(|call| call.wire_index.is_some());
        let mut order: Vec<usize> = (0..self.calls.len()).collect();
        if all_indexed {
            order.sort_by_key(|slot| {
                let call = &self.calls[*slot];
                (call.wire_index, call.sequence)
            });
        }

        for slot in order {
            let call = &mut self.calls[slot];
            if call.started && !call.delta_buffer.is_empty() {
                let _ = tx
                    .send(ProviderEvent::ToolCallDelta {
                        index: slot,
                        delta: std::mem::take(&mut call.delta_buffer),
                    })
                    .await;
            }
        }

        if let Some(slot) = self
            .calls
            .iter()
            .position(|call| call.call.name.trim().is_empty())
        {
            return Err(CoreError::Internal {
                detail: format!(
                    "Chat Completions tool call at logical slot {slot} has pending id or argument data but no non-blank function name"
                ),
            });
        }

        if all_indexed {
            self.calls
                .sort_by_key(|call| (call.wire_index, call.sequence));
        }
        Ok(self.calls.into_iter().map(|tracked| tracked.call).collect())
    }

    fn create_call(
        &mut self,
        wire_id: Option<&str>,
        wire_index: Option<usize>,
        name: Option<&str>,
    ) -> usize {
        let id = self.create_id(wire_id);
        let sequence = self.calls.len();
        self.calls
            .push(TrackedCall::new(id, wire_index, sequence, name));
        if let Some(wire_id) = wire_id {
            self.associate_id(sequence, wire_id);
        }
        sequence
    }

    fn create_id(&mut self, wire_id: Option<&str>) -> String {
        if let Some(id) = wire_id {
            // The provider's id is authoritative for wire/tool_result pairing,
            // even when it repeats within one response. Internal uniqueness
            // (UI item ids, approval request ids) is owned by the session and
            // does not rewrite the provider's id; only calls without any id
            // receive a generated one.
            self.used_ids.insert(id.to_string());
            return id.to_string();
        }

        loop {
            let suffix = NEXT_GENERATED_ID.fetch_add(1, Ordering::Relaxed);
            let id = format!("chat-tool-call-{suffix}");
            if self.used_ids.insert(id.clone()) {
                return id;
            }
        }
    }

    fn associate_id(&mut self, slot: usize, wire_id: &str) {
        let slots = self.by_id.entry(wire_id.to_string()).or_default();
        if !slots.contains(&slot) {
            slots.push(slot);
        }
    }

    fn associate_index(&mut self, slot: usize, index: usize) {
        if self.calls[slot].wire_index.is_none() {
            self.calls[slot].wire_index = Some(index);
        }
        let slots = self.by_index.entry(index).or_default();
        if !slots.contains(&slot) {
            slots.push(slot);
        }
    }

    async fn start(&mut self, slot: usize, tx: &tokio::sync::mpsc::Sender<ProviderEvent>) {
        let call = &mut self.calls[slot];
        call.started = true;
        let _ = tx
            .send(ProviderEvent::ToolCallStart {
                index: slot,
                id: call.call.id.clone(),
                name: call.call.name.clone(),
            })
            .await;
        let deferred = std::mem::take(&mut call.deferred_deltas);
        for fragment in deferred {
            self.push_live_delta(slot, &fragment, tx).await;
        }
    }

    async fn append_arguments(
        &mut self,
        slot: usize,
        arguments: Option<&str>,
        tx: &tokio::sync::mpsc::Sender<ProviderEvent>,
    ) {
        let Some(fragment) = arguments.filter(|fragment| !fragment.is_empty()) else {
            return;
        };
        // A closed top-level JSON value ends this call's input: deliver the
        // tail now instead of at stream end, so a read-only call can start
        // before the message is complete.
        let (complete, started) = {
            let call = &mut self.calls[slot];
            call.call.arguments.push_str(fragment);
            call.argument_state.append(fragment);
            (
                call.argument_state.has_complete_structured_value(),
                call.started,
            )
        };
        if started {
            self.push_live_delta(slot, fragment, tx).await;
            if complete {
                self.flush_slot(slot, tx).await;
            }
        } else {
            self.calls[slot].deferred_deltas.push(fragment.to_string());
        }
    }

    /// Deliver one call's unflushed argument tail.
    async fn flush_slot(&mut self, slot: usize, tx: &tokio::sync::mpsc::Sender<ProviderEvent>) {
        let buffer = &mut self.calls[slot].delta_buffer;
        if !buffer.is_empty() {
            let _ = tx
                .send(ProviderEvent::ToolCallDelta {
                    index: slot,
                    delta: std::mem::take(buffer),
                })
                .await;
        }
    }

    async fn push_live_delta(
        &mut self,
        slot: usize,
        fragment: &str,
        tx: &tokio::sync::mpsc::Sender<ProviderEvent>,
    ) {
        let buffer = &mut self.calls[slot].delta_buffer;
        buffer.push_str(fragment);
        if has_line_break(fragment) || buffer.len() >= DELTA_FLUSH_BYTES {
            let _ = tx
                .send(ProviderEvent::ToolCallDelta {
                    index: slot,
                    delta: std::mem::take(buffer),
                })
                .await;
        }
    }

    fn resolve(
        &self,
        wire_id: Option<&str>,
        wire_index: Option<usize>,
        name: Option<&str>,
        explicit_start: bool,
        empty_arguments: bool,
    ) -> Resolution {
        let indexed = wire_index.and_then(|index| self.by_index.get(&index));
        let matching_indexed = indexed
            .map(|slots| self.filter_by_name(slots, name))
            .unwrap_or_default();

        if let Some(wire_id) = wire_id {
            if let Some(with_id) = self.by_id.get(wire_id) {
                if wire_index.is_some() {
                    let matching: Vec<usize> = matching_indexed
                        .iter()
                        .copied()
                        .filter(|slot| with_id.contains(slot))
                        .collect();
                    let resolution = self.resolve_matching(&matching, explicit_start);
                    if !matches!(resolution, Resolution::New) {
                        return resolution;
                    }
                    if name.is_some() {
                        return Resolution::New;
                    }
                    if indexed.is_some() {
                        return Resolution::Ambiguous;
                    }
                    return self.resolve_matching(with_id, false);
                }

                if name.is_some() {
                    let matching: Vec<usize> = with_id
                        .iter()
                        .copied()
                        .filter(|slot| self.name_matches(*slot, name))
                        .collect();
                    return self.resolve_matching(&matching, explicit_start);
                }
                return self.resolve_matching(with_id, false);
            }

            if !matching_indexed.is_empty() {
                if matching_indexed
                    .iter()
                    .all(|slot| !self.calls[*slot].has_name())
                {
                    return self.resolve_matching(&matching_indexed, false);
                }
                if explicit_start {
                    return Resolution::New;
                }
                // A pending nameless call can still receive its late name after
                // its arguments complete; empty-ID-change splits need a named call.
                let named_empty_start = name.is_some()
                    && empty_arguments
                    && matching_indexed
                        .iter()
                        .all(|slot| self.calls[*slot].has_name());
                return self.resolve_matching(&matching_indexed, named_empty_start);
            }
            return Resolution::New;
        }

        if indexed.is_some() {
            return self.resolve_matching(&matching_indexed, explicit_start);
        }
        if name.is_some() {
            return Resolution::New;
        }
        let unfinished: Vec<usize> = (0..self.calls.len()).collect();
        self.resolve_matching(&unfinished, false)
    }

    fn filter_by_name(&self, slots: &[usize], name: Option<&str>) -> Vec<usize> {
        slots
            .iter()
            .copied()
            .filter(|slot| self.name_matches(*slot, name))
            .collect()
    }

    fn name_matches(&self, slot: usize, name: Option<&str>) -> bool {
        name.is_none_or(|name| {
            let call = &self.calls[slot];
            !call.has_name() || call.call.name == name
        })
    }

    fn resolve_matching(&self, candidates: &[usize], explicit_start: bool) -> Resolution {
        if candidates.is_empty() {
            return Resolution::New;
        }
        if candidates.len() == 1 && !self.calls[candidates[0]].has_name() {
            return Resolution::Existing(candidates[0]);
        }
        let continuable: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|slot| {
                !self.calls[*slot]
                    .argument_state
                    .has_complete_structured_value()
            })
            .collect();

        if !explicit_start && candidates.len() == 1 {
            return Resolution::Existing(candidates[0]);
        }
        if continuable.len() == 1 {
            return Resolution::Existing(continuable[0]);
        }
        if continuable.len() > 1 || !explicit_start {
            return Resolution::Ambiguous;
        }
        Resolution::New
    }
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

fn starts_with_structured_value(value: Option<&str>) -> bool {
    value
        .and_then(|value| value.trim_start().chars().next())
        .is_some_and(|character| character == '{' || character == '[')
}

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

    fn collect(rx: &mut tokio::sync::mpsc::Receiver<ProviderEvent>) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn starts(events: &[ProviderEvent]) -> Vec<(usize, String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallStart { index, id, name } => {
                    Some((*index, id.clone(), name.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn deltas(events: &[ProviderEvent]) -> Vec<(usize, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallDelta { index, delta } => Some((*index, delta.clone())),
                _ => None,
            })
            .collect()
    }

    fn names_and_arguments(calls: &[ToolCall]) -> Vec<(&str, &str)> {
        calls
            .iter()
            .map(|call| (call.name.as_str(), call.arguments.as_str()))
            .collect()
    }

    #[tokio::test]
    async fn repeated_id_and_late_name_start_once_and_reassemble() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(3),
            Some("dup"),
            None,
            Some(r#"{"path":"a"#),
            &tx
        );
        push!(
            &mut stream,
            Some(3),
            Some("dup"),
            None,
            Some(r#".txt""#),
            &tx
        );
        assert!(collect(&mut rx).is_empty());

        push!(
            &mut stream,
            Some(3),
            Some("dup"),
            Some("Write"),
            Some("}"),
            &tx
        );
        let events = collect(&mut rx);
        assert_eq!(starts(&events), vec![(0, "dup".into(), "Write".into())]);
        // The name unlocked the deferred fragments and the closing brace
        // completed the input: its whole text is delivered with the start,
        // not held back until the finish boundary.
        assert_eq!(deltas(&events), vec![(0, r#"{"path":"a.txt"}"#.into())]);
        assert_eq!(events.len(), 2, "one start and one delta");

        let calls = stream.finish(&tx).await.expect("named call finishes");
        assert!(collect(&mut rx).is_empty(), "nothing is left to flush");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "dup");
        assert_eq!(calls[0].arguments, r#"{"path":"a.txt"}"#);
    }

    /// A closed top-level JSON value ends a call's input: the unflushed tail
    /// goes out at once, so a read-only call can start before the message is
    /// complete. Nothing is re-sent at the finish boundary.
    #[tokio::test]
    async fn a_closed_json_value_delivers_its_tail_before_the_finish() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(0),
            Some("call_1"),
            Some("Read"),
            Some(r#"{"path":"a"#),
            &tx
        );
        let events = collect(&mut rx);
        assert_eq!(starts(&events), vec![(0, "call_1".into(), "Read".into())]);
        assert!(deltas(&events).is_empty(), "an open value is still batching");

        push!(
            &mut stream,
            Some(0),
            None,
            None,
            Some(r#".txt"}"#),
            &tx
        );
        let events = collect(&mut rx);
        assert_eq!(deltas(&events), vec![(0, r#"{"path":"a.txt"}"#.into())]);

        let calls = stream.finish(&tx).await.expect("complete call finishes");
        assert!(collect(&mut rx).is_empty(), "the tail was already delivered");
        assert_eq!(calls[0].arguments, r#"{"path":"a.txt"}"#);
    }

    /// Two calls in one response: each delivers its own tail when its input
    /// closes, so the first is whole while the second is still streaming.
    #[tokio::test]
    async fn each_call_delivers_its_own_tail_as_it_closes() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(&mut stream, Some(0), Some("c1"), Some("Read"), Some(r#"{"path":"one"}"#), &tx);
        let events = collect(&mut rx);
        assert_eq!(deltas(&events), vec![(0, r#"{"path":"one"}"#.into())]);

        push!(&mut stream, Some(1), Some("c2"), Some("Grep"), Some(r#"{"pattern":"x"#), &tx);
        let events = collect(&mut rx);
        assert!(deltas(&events).is_empty(), "the second call is still open");
        push!(&mut stream, Some(1), None, None, Some("\"}"), &tx);
        let events = collect(&mut rx);
        assert_eq!(deltas(&events), vec![(1, r#"{"pattern":"x"}"#.into())]);

        let calls = stream.finish(&tx).await.expect("both calls finish");
        assert!(collect(&mut rx).is_empty());
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, r#"{"path":"one"}"#);
        assert_eq!(calls[1].arguments, r#"{"pattern":"x"}"#);
    }

    #[tokio::test]
    async fn missing_id_gets_stable_canonical_id_and_late_wire_id_is_alias() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(0),
            None,
            Some("Read"),
            Some(r#"{"pa"#),
            &tx
        );
        let events = collect(&mut rx);
        let canonical = starts(&events)[0].1.clone();
        assert!(!canonical.is_empty());

        push!(
            &mut stream,
            Some(0),
            Some("late-wire-id"),
            None,
            Some(r#"th":"a"}"#),
            &tx,
        );
        let calls = stream.finish(&tx).await.expect("named call finishes");
        assert_eq!(calls[0].id, canonical);
        assert_ne!(calls[0].id, "late-wire-id");
        assert_eq!(calls[0].arguments, r#"{"path":"a"}"#);
    }

    #[tokio::test]
    async fn same_raw_index_separates_distinct_and_reused_ids() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(0),
            Some("dup"),
            Some("first"),
            Some(r#"{"value":"#),
            &tx,
        );
        push!(
            &mut stream,
            Some(0),
            Some("other"),
            Some("second"),
            Some(r#"{"value":2}"#),
            &tx,
        );
        push!(&mut stream, Some(0), Some("dup"), None, Some("1}"), &tx);
        push!(
            &mut stream,
            Some(0),
            Some("dup"),
            Some("first"),
            Some(r#"{"value":3}"#),
            &tx,
        );

        let calls = stream.finish(&tx).await.expect("all calls are named");
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].id, "dup");
        assert_eq!(calls[1].id, "other");
        assert_eq!(
            calls[2].id, "dup",
            "the provider's reused id is preserved for wire pairing"
        );
        assert_eq!(calls[0].arguments, r#"{"value":1}"#);
        assert_eq!(calls[1].arguments, r#"{"value":2}"#);
        assert_eq!(calls[2].arguments, r#"{"value":3}"#);
        let start_slots: Vec<usize> = starts(&collect(&mut rx))
            .into_iter()
            .map(|(slot, _, _)| slot)
            .collect();
        assert_eq!(start_slots, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn complete_same_name_structured_start_creates_new_call() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(0),
            None,
            Some("same_tool"),
            Some(r#"{"value":1}"#),
            &tx,
        );
        push!(
            &mut stream,
            Some(0),
            None,
            Some("same_tool"),
            Some(r#"{"value":2}"#),
            &tx,
        );
        let calls = stream.finish(&tx).await.expect("both calls are named");

        assert_eq!(
            names_and_arguments(&calls),
            vec![
                ("same_tool", r#"{"value":1}"#),
                ("same_tool", r#"{"value":2}"#),
            ]
        );
        assert_ne!(calls[0].id, calls[1].id);
        assert_eq!(starts(&collect(&mut rx)).len(), 2);
    }

    #[tokio::test]
    async fn incomplete_nested_structured_start_is_continuation() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(0),
            Some("call_1"),
            Some("calculate"),
            Some(r#"{"value":"#),
            &tx,
        );
        push!(
            &mut stream,
            Some(0),
            Some("call_1"),
            Some("calculate"),
            Some(r#"{"nested":true}}"#),
            &tx,
        );
        let calls = stream.finish(&tx).await.expect("call is named");

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, r#"{"value":{"nested":true}}"#);
        assert_eq!(starts(&collect(&mut rx)).len(), 1);
    }

    #[tokio::test]
    async fn same_index_interleaved_arguments_route_to_their_own_slots() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(9),
            Some("left"),
            Some("same"),
            Some(r#"{"left":"#),
            &tx,
        );
        push!(
            &mut stream,
            Some(9),
            Some("right"),
            Some("same"),
            Some(r#"{"right":"#),
            &tx,
        );
        push!(&mut stream, Some(9), Some("left"), None, Some("1}\n"), &tx);
        push!(&mut stream, Some(9), Some("right"), None, Some("2}\n"), &tx);
        let events = collect(&mut rx);

        assert_eq!(
            deltas(&events),
            vec![(0, "{\"left\":1}\n".into()), (1, "{\"right\":2}\n".into()),]
        );
        let calls = stream.finish(&tx).await.expect("calls are named");
        assert_eq!(calls[0].arguments, "{\"left\":1}\n");
        assert_eq!(calls[1].arguments, "{\"right\":2}\n");
    }

    #[tokio::test]
    async fn permanently_missing_name_is_an_internal_error() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(&mut stream, Some(0), Some("call_1"), None, Some("{}"), &tx);
        let error = stream
            .finish(&tx)
            .await
            .expect_err("pending name must fail");

        assert!(matches!(
            error,
            CoreError::Internal { detail } if detail.contains("no non-blank function name")
        ));
        assert!(collect(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn empty_nameless_delta_is_not_actual_pending_data() {
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(&mut stream, Some(0), None, None, Some(""), &tx);
        let calls = stream
            .finish(&tx)
            .await
            .expect("empty delta creates no call");
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn ambiguous_unlabeled_fragment_does_not_pollute_calls() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(0),
            Some("call_1"),
            Some("first"),
            Some(r#"{"a":"#),
            &tx,
        );
        push!(
            &mut stream,
            Some(1),
            Some("call_2"),
            Some("second"),
            Some(r#"{"b":"#),
            &tx,
        );
        push!(
            &mut stream,
            None,
            None,
            None,
            Some(r#"{"polluted":true}"#),
            &tx
        );
        let calls = stream.finish(&tx).await.expect("existing calls are named");

        assert_eq!(
            names_and_arguments(&calls),
            vec![("first", r#"{"a":"#), ("second", r#"{"b":"#)]
        );
        assert!(
            deltas(&collect(&mut rx))
                .iter()
                .all(|(_, delta)| !delta.contains("polluted"))
        );
    }

    #[tokio::test]
    async fn fully_indexed_calls_finish_in_wire_index_and_sequence_order() {
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(1),
            Some("b"),
            Some("second"),
            Some("{}"),
            &tx
        );
        push!(
            &mut stream,
            Some(0),
            Some("a"),
            Some("first"),
            Some("{}"),
            &tx
        );
        push!(
            &mut stream,
            Some(1),
            Some("c"),
            Some("third"),
            Some("{}"),
            &tx
        );
        let calls = stream.finish(&tx).await.expect("calls are named");
        let names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[tokio::test]
    async fn mixed_indexed_and_unindexed_calls_keep_insertion_order() {
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(&mut stream, None, Some("a"), Some("first"), Some("{}"), &tx);
        push!(
            &mut stream,
            Some(0),
            Some("b"),
            Some("second"),
            Some("{}"),
            &tx
        );
        let calls = stream.finish(&tx).await.expect("calls are named");
        let names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[tokio::test]
    async fn blank_name_continues_but_cannot_start_and_nonblank_values_are_preserved() {
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(0),
            Some(" spaced "),
            Some(" same_tool "),
            Some(r#"{"value":"#),
            &tx,
        );
        push!(
            &mut stream,
            Some(0),
            Some(" \n"),
            Some("   "),
            Some("0}"),
            &tx
        );
        push!(
            &mut stream,
            Some(1),
            Some("ignored"),
            Some(""),
            Some("{}"),
            &tx
        );
        let calls = stream.finish(&tx).await.expect("blank new name is ignored");

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, " spaced ");
        assert_eq!(calls[0].name, " same_tool ");
        assert_eq!(calls[0].arguments, r#"{"value":0}"#);
    }

    #[tokio::test]
    async fn late_name_replays_pending_arguments_with_line_and_size_flush_rules() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        let long = "x".repeat(DELTA_FLUSH_BYTES);
        push!(
            &mut stream,
            Some(0),
            Some("call_1"),
            None,
            Some(r#"{"content":"l1"#),
            &tx,
        );
        push!(&mut stream, Some(0), None, None, Some(r#"\nl2"#), &tx);
        push!(&mut stream, Some(0), None, None, Some(&long), &tx);
        assert!(collect(&mut rx).is_empty());

        push!(&mut stream, Some(0), None, Some("Write"), None, &tx);
        let events = collect(&mut rx);
        assert_eq!(starts(&events).len(), 1);
        assert_eq!(
            deltas(&events),
            vec![(0, r#"{"content":"l1\nl2"#.into()), (0, long.clone())]
        );

        push!(&mut stream, Some(0), None, None, Some("\"}"), &tx);
        let calls = stream.finish(&tx).await.expect("late name completes call");
        assert_eq!(deltas(&collect(&mut rx)), vec![(0, "\"}".into())]);
        assert_eq!(
            calls[0].arguments,
            format!(r#"{{"content":"l1\nl2{long}"}}"#)
        );
    }

    #[tokio::test]
    async fn argument_state_tracks_strings_escapes_and_mismatches() {
        let mut state = ArgumentState::default();
        state.append(r#"{"value":"braces: } ] { [ and escaped quote: \"#);
        assert!(!state.has_complete_structured_value());
        state.append(r#" still in string"}"#);
        assert!(state.has_complete_structured_value());

        let mut mismatch = ArgumentState::default();
        mismatch.append(r#"{"value":]"#);
        mismatch.append("}");
        assert!(!mismatch.has_complete_structured_value());
    }

    #[tokio::test]
    async fn late_id_and_name_can_complete_an_already_complete_pending_call() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(&mut stream, Some(0), None, None, Some("{}"), &tx);
        push!(&mut stream, Some(0), Some("late"), Some("Read"), None, &tx);
        let events = collect(&mut rx);
        let canonical = starts(&events)[0].1.clone();
        assert_ne!(canonical, "late");
        let calls = stream
            .finish(&tx)
            .await
            .expect("late labels name pending call");

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, canonical);
        assert_eq!(calls[0].name, "Read");
        assert_eq!(calls[0].arguments, "{}");
    }

    #[tokio::test]
    async fn repeated_named_empty_start_after_complete_creates_a_new_call() {
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(&mut stream, Some(0), None, Some("Write"), None, &tx);
        push!(
            &mut stream,
            Some(0),
            None,
            None,
            Some(r#"{"path":"a"}"#),
            &tx
        );
        push!(&mut stream, Some(0), None, Some("Write"), None, &tx);
        push!(
            &mut stream,
            Some(0),
            None,
            None,
            Some(r#"{"path":"b"}"#),
            &tx
        );
        let calls = stream.finish(&tx).await.expect("both calls are named");

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, r#"{"path":"a"}"#);
        assert_eq!(calls[1].arguments, r#"{"path":"b"}"#);
        assert_ne!(calls[0].id, calls[1].id);
    }

    #[tokio::test]
    async fn blank_name_prefix_is_buffered_until_a_real_name_arrives() {
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            Some(0),
            Some("call_1"),
            Some(""),
            Some(r#"{"pa"#),
            &tx
        );
        push!(
            &mut stream,
            Some(0),
            Some("call_1"),
            Some("Read"),
            Some(r#"th":"a"}"#),
            &tx,
        );
        let calls = stream.finish(&tx).await.expect("late name completes call");

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Read");
        assert_eq!(calls[0].arguments, r#"{"path":"a"}"#);
    }

    #[tokio::test]
    async fn late_wire_index_participates_in_final_sorting() {
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let mut stream = ChatToolStream::new();
        push!(
            &mut stream,
            None,
            Some("b"),
            Some("second"),
            Some("{}"),
            &tx
        );
        push!(
            &mut stream,
            Some(0),
            Some("a"),
            Some("first"),
            Some("{}"),
            &tx
        );
        push!(&mut stream, Some(1), Some("b"), None, None, &tx);
        let calls = stream.finish(&tx).await.expect("calls are named");

        let ids: Vec<&str> = calls.iter().map(|call| call.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }
}
