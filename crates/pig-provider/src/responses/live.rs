use crate::chat::{AssistantReplayItem, TextCitation};

#[derive(Default)]
pub(crate) struct LiveResponsesText {
    segments: Vec<LiveSegment>,
    next_sequence: usize,
}

struct LiveSegment {
    output_index: Option<usize>,
    item_id: Option<String>,
    sequence: usize,
    done: bool,
    kind: LiveSegmentKind,
}

enum LiveSegmentKind {
    Reasoning {
        summary: String,
        content: String,
    },
    Text {
        text: String,
        citations: Vec<TextCitation>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveKind {
    Reasoning,
    Text,
    Other,
}

impl LiveSegment {
    fn kind(&self) -> LiveKind {
        match self.kind {
            LiveSegmentKind::Reasoning { .. } => LiveKind::Reasoning,
            LiveSegmentKind::Text { .. } => LiveKind::Text,
        }
    }

    fn matches(&self, output_index: Option<usize>, item_id: Option<&str>, kind: LiveKind) -> bool {
        if self.kind() != kind {
            return false;
        }
        match output_index {
            Some(index) => self.output_index == Some(index),
            None => match item_id {
                Some(id) => self.item_id.as_deref() == Some(id),
                None => self.output_index.is_none() && self.item_id.is_none(),
            },
        }
    }
}

impl LiveResponsesText {
    fn take_sequence(&mut self) -> usize {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        sequence
    }

    fn segment_mut(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        kind: LiveKind,
    ) -> &mut LiveSegment {
        if let Some(index) = self
            .segments
            .iter_mut()
            .position(|segment| segment.matches(output_index, item_id, kind))
        {
            return &mut self.segments[index];
        }
        let sequence = self.take_sequence();
        let kind = match kind {
            LiveKind::Reasoning => LiveSegmentKind::Reasoning {
                summary: String::new(),
                content: String::new(),
            },
            LiveKind::Text => LiveSegmentKind::Text {
                text: String::new(),
                citations: Vec::new(),
            },
            LiveKind::Other => unreachable!(),
        };
        self.segments.push(LiveSegment {
            output_index,
            item_id: item_id.map(str::to_string),
            sequence,
            done: false,
            kind,
        });
        self.segments.last_mut().expect("segment inserted")
    }

    pub(crate) fn reasoning_summary_delta(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        delta: &str,
    ) {
        let segment = self.segment_mut(output_index, item_id, LiveKind::Reasoning);
        let LiveSegmentKind::Reasoning { summary, .. } = &mut segment.kind else {
            return;
        };
        summary.push_str(delta);
    }

    pub(crate) fn reasoning_content_delta(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        delta: &str,
    ) {
        let segment = self.segment_mut(output_index, item_id, LiveKind::Reasoning);
        let LiveSegmentKind::Reasoning { content, .. } = &mut segment.kind else {
            return;
        };
        content.push_str(delta);
    }

    pub(crate) fn text_delta(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        delta: &str,
    ) {
        let segment = self.segment_mut(output_index, item_id, LiveKind::Text);
        let LiveSegmentKind::Text { text, .. } = &mut segment.kind else {
            return;
        };
        text.push_str(delta);
    }

    pub(crate) fn annotation_added(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        citation: TextCitation,
    ) {
        let segment = self.segment_mut(output_index, item_id, LiveKind::Text);
        let LiveSegmentKind::Text { citations, .. } = &mut segment.kind else {
            return;
        };
        if !citations.iter().any(|existing| {
            existing.url == citation.url
                && existing.start_index == citation.start_index
                && existing.end_index == citation.end_index
        }) {
            citations.push(citation);
        }
    }

    /// Authoritative done items win over streamed annotations; this drains the
    /// live fallback copy so a done item is never duplicated.
    pub(crate) fn take_citations(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
    ) -> Vec<TextCitation> {
        let position = self.segments.iter_mut().position(|segment| {
            segment.kind() == LiveKind::Text
                && segment.matches(output_index, item_id, LiveKind::Text)
        });
        match position {
            Some(index) => match &mut self.segments[index].kind {
                LiveSegmentKind::Text { citations, .. } => std::mem::take(citations),
                _ => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    pub(crate) fn note_done(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        kind: LiveKind,
    ) -> usize {
        let order_index = output_index.unwrap_or_else(|| self.take_sequence());
        if output_index.is_some() {
            self.next_sequence += 1;
        }
        let item_id = item_id.filter(|id| !id.is_empty());
        let mut matched = false;
        for segment in &mut self.segments {
            let index_match = output_index
                .is_some_and(|index| segment.output_index == Some(index) && segment.kind() == kind);
            let id_match = item_id
                .is_some_and(|id| segment.item_id.as_deref() == Some(id) && segment.kind() == kind);
            if index_match || id_match {
                segment.done = true;
                matched = true;
            }
        }
        if matched {
            return order_index;
        }
        // A done item that carries an id/index but whose deltas were anonymous
        // (a common compatible-gateway shape) claims the single anonymous
        // segment of the same kind. With multiple candidates any guess would
        // duplicate or misorder text, so leave them to the fallback.
        let candidates: Vec<usize> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, segment)| {
                segment.kind() == kind
                    && !segment.done
                    && segment.output_index.is_none()
                    && segment.item_id.is_none()
            })
            .map(|(index, _)| index)
            .collect();
        if candidates.len() == 1 && (output_index.is_some() || item_id.is_some()) {
            let segment = &mut self.segments[candidates[0]];
            if let Some(index) = output_index {
                segment.output_index = Some(index);
            }
            if let Some(id) = item_id {
                segment.item_id = Some(id.to_string());
            }
            segment.done = true;
        } else if output_index.is_none() && item_id.is_none() && candidates.len() == 1 {
            self.segments[candidates[0]].done = true;
        }
        order_index
    }

    pub(crate) fn apply_fallback(self, replay_items: &mut Vec<(usize, AssistantReplayItem)>) {
        for segment in self.segments {
            if segment.done {
                continue;
            }
            let index = segment.output_index.unwrap_or(segment.sequence);
            match segment.kind {
                LiveSegmentKind::Reasoning { summary, content }
                    if !summary.is_empty() || !content.is_empty() =>
                {
                    replay_items.push((
                        index,
                        AssistantReplayItem::ResponsesReasoning {
                            id: None,
                            summary: (!summary.is_empty())
                                .then_some(summary)
                                .into_iter()
                                .collect(),
                            content: (!content.is_empty())
                                .then_some(content)
                                .into_iter()
                                .collect(),
                            encrypted_content: None,
                        },
                    ));
                }
                LiveSegmentKind::Text { text, citations } if !text.is_empty() => {
                    replay_items.push((index, AssistantReplayItem::Text { text, citations }));
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anonymous_text(text: &str, sequence: usize) -> LiveSegment {
        LiveSegment {
            output_index: None,
            item_id: None,
            sequence,
            done: false,
            kind: LiveSegmentKind::Text {
                text: text.to_string(),
                citations: Vec::new(),
            },
        }
    }

    #[test]
    fn multiple_anonymous_candidates_are_never_guessed() {
        let mut live = LiveResponsesText {
            segments: vec![anonymous_text("first", 0), anonymous_text("second", 1)],
            next_sequence: 2,
        };
        live.note_done(Some(5), Some("msg_1"), LiveKind::Text);
        assert!(
            live.segments.iter().all(|segment| !segment.done
                && segment.output_index.is_none()
                && segment.item_id.is_none()),
            "with more than one candidate the done item must not claim any: {:?}",
            live.segments.len()
        );
        let mut items = Vec::new();
        live.apply_fallback(&mut items);
        assert_eq!(items.len(), 2, "both candidates remain fallback text");
    }

    #[test]
    fn a_single_anonymous_candidate_is_claimed_and_backfilled() {
        let mut live = LiveResponsesText {
            segments: vec![anonymous_text("only", 0)],
            next_sequence: 1,
        };
        live.note_done(Some(7), Some("msg_1"), LiveKind::Text);
        let segment = &live.segments[0];
        assert!(segment.done);
        assert_eq!(segment.output_index, Some(7));
        assert_eq!(segment.item_id.as_deref(), Some("msg_1"));
        let mut items = Vec::new();
        live.apply_fallback(&mut items);
        assert!(
            items.is_empty(),
            "the claimed segment is not replayed twice"
        );
    }
}
