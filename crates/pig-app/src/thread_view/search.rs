use super::*;

impl ThreadView {
    /// Lazily create the search input (on first open). Subscription: rerun the
    /// search on input change; Enter=next, Shift+Enter=previous (the
    /// single-line input emits PressEnter for both Enter/Shift+Enter and inserts
    /// no newline; the action is consumed at the input layer and never bubbles
    /// up to the composer as a send — same PressEnter-send path as the composer)
    pub(crate) fn ensure_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rust_i18n::t!("thread.search_placeholder"))
        });
        let subscription = cx.subscribe(&input, |this, _, event, cx| match event {
            InputEvent::Change => this.run_search(cx),
            InputEvent::PressEnter { shift, .. } => this.goto_match(!shift, cx),
            _ => {}
        });
        self.search_input = Some((input, subscription));
    }

    /// The search input entity (exists only after first open)
    pub(crate) fn search_input(&self) -> Option<&Entity<InputState>> {
        self.search_input.as_ref().map(|(input, _)| input)
    }

    /// Open the search bar and focus the input; rerun once when a query exists
    /// (content may have streamed on)
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_search_input(window, cx);
        self.search_open = true;
        if let Some(input) = self.search_input() {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        if !self.search_query.is_empty() {
            self.run_search(cx);
        }
        cx.notify();
    }

    /// Close the search bar: clear the query, all segment highlights, matches,
    /// and reset the active index. set_value emits no Change (upstream
    /// emit_events=false), so everything is reset explicitly here
    pub fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = false;
        self.clear_search_highlights(cx);
        if let Some(input) = self.search_input() {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.search_query.clear();
        self.search_matches.clear();
        self.search_cache.clear();
        self.active_match = 0;
        cx.notify();
    }

    /// In-session search: case-insensitively match all Markdown segments
    /// (rendered_text and query are both lowercased, then byte-level
    /// match_indices). Lowercasing is the identity for caseless scripts (CJK
    /// etc.), so ranges are byte offsets into the original text; the rare
    /// characters whose lowercasing changes byte length (Turkish İ for example)
    /// skew ranges, and that whole segment is rejected by
    /// set_range_highlights (Err ignored), degrading to no highlight without
    /// affecting other segments
    pub(crate) fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self
            .search_input()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let query_lc = query.to_lowercase();
        // Reuse condition: query unchanged and the segment revision unchanged; a
        // changed query rescans all segments
        let same_query = self.search_query.to_lowercase() == query_lc;
        self.search_query = query;
        // Segments that matched last round: clear stale highlights when they no
        // longer match
        let mut touched: HashSet<EntityId> = self
            .search_cache
            .iter()
            .filter(|(_, cache)| !cache.ranges.is_empty())
            .map(|(id, _)| *id)
            .collect();
        let mut matches = Vec::new();
        let mut new_cache: HashMap<EntityId, SearchSegmentCache> = HashMap::new();
        if !query_lc.is_empty() {
            for (msg_ix, message) in self.messages.iter().enumerate() {
                for (seg_ix, segment) in message.segments.iter().enumerate() {
                    let Segment::Markdown { state, .. } = segment else {
                        continue;
                    };
                    let id = state.entity_id();
                    let text = state.read(cx).rendered_text();
                    let ranges = match self.search_cache.get(&id) {
                        Some(cache) if same_query && cache.snapshot == text => cache.ranges.clone(),
                        _ => text
                            .as_str()
                            .to_lowercase()
                            .match_indices(&query_lc)
                            .map(|(start, _)| start..start + query_lc.len())
                            .collect(),
                    };
                    if !ranges.is_empty() {
                        touched.insert(id);
                        matches.extend(ranges.iter().map(|range| SearchMatch {
                            msg_ix,
                            seg_ix,
                            range: range.clone(),
                        }));
                    }
                    new_cache.insert(
                        id,
                        SearchSegmentCache {
                            snapshot: text,
                            ranges,
                        },
                    );
                }
            }
        }
        self.search_cache = new_cache;
        self.search_matches = matches;
        if self.active_match >= self.search_matches.len() {
            self.active_match = 0;
        }
        for (msg_ix, message) in self.messages.iter().enumerate() {
            for (seg_ix, segment) in message.segments.iter().enumerate() {
                let Segment::Markdown { state, .. } = segment else {
                    continue;
                };
                if touched.contains(&state.entity_id()) {
                    self.highlight_segment(msg_ix, seg_ix, cx);
                }
            }
        }
        cx.notify();
    }

    /// Re-apply a segment's search highlights: the active match gets the deeper
    /// accent 0.5, the rest 0.25; cleared when there are none.
    /// set_range_highlights validates ranges as a batch and rejects the whole
    /// batch on any invalid one — Err is ignored and the segment degrades to no
    /// highlight (range coordinate system explained in run_search)
    pub(crate) fn highlight_segment(&self, msg_ix: usize, seg_ix: usize, cx: &mut Context<Self>) {
        let Some(Segment::Markdown { state, .. }) = self
            .messages
            .get(msg_ix)
            .and_then(|message| message.segments.get(seg_ix))
        else {
            return;
        };
        let active_bg = cx.theme().accent.opacity(0.5);
        let normal_bg = cx.theme().accent.opacity(0.25);
        let highlights: Vec<RangeHighlight> = self
            .search_matches
            .iter()
            .enumerate()
            .filter(|(_, m)| m.msg_ix == msg_ix && m.seg_ix == seg_ix)
            .map(|(ix, m)| {
                RangeHighlight::new(
                    m.range.clone(),
                    if ix == self.active_match {
                        active_bg
                    } else {
                        normal_bg
                    },
                )
            })
            .collect();
        state.update(cx, |state, cx| {
            if highlights.is_empty() {
                state.clear_range_highlights(cx);
            } else {
                let _ = state.set_range_highlights(highlights, cx);
            }
        });
    }

    /// Clear search highlights on all Markdown segments (upstream is a no-op
    /// when the segment has none)
    pub(crate) fn clear_search_highlights(&mut self, cx: &mut Context<Self>) {
        for message in &self.messages {
            for segment in &message.segments {
                if let Segment::Markdown { state, .. } = segment {
                    state.update(cx, |state, cx| state.clear_range_highlights(cx));
                }
            }
        }
    }

    /// Jump to the next/previous match (wrapping): re-apply highlights on the
    /// old/new active match's segments for the color change, and reveal the
    /// target line into view. Jumping = leaving the bottom, so following pauses;
    /// the reveal's scroll only takes effect on later frames, and nav_jump
    /// suppresses one "back at bottom auto-resumes following" (same timing as
    /// nav-bar jumps, see the resume logic in render)
    pub(crate) fn goto_match(&mut self, next: bool, cx: &mut Context<Self>) {
        if self.search_matches.is_empty() {
            return;
        }
        let total = self.search_matches.len();
        let previous = self.active_match;
        self.active_match = if next {
            (previous + 1) % total
        } else {
            (previous + total - 1) % total
        };
        for ix in [previous, self.active_match] {
            let m = &self.search_matches[ix];
            self.highlight_segment(m.msg_ix, m.seg_ix, cx);
        }
        let target = self.search_matches[self.active_match].clone();
        if let Some(Segment::Markdown { state, .. }) = self
            .messages
            .get(target.msg_ix)
            .and_then(|message| message.segments.get(target.seg_ix))
        {
            self.follow_bottom = false;
            self.nav_jump = true;
            state.update(cx, |state, cx| {
                let _ = state.reveal_range(target.range.clone(), cx);
            });
        }
        cx.notify();
    }

    /// In-session search bar: input + match counter + prev/next + close (a
    /// pinned row above the message list). key_context("thread-search") lets
    /// the Esc → CloseThreadSearch binding fire (the input's Escape action
    /// defaults to cx.propagate() and releases into that context, see the
    /// main.rs key bindings); closing is forwarded back to close_search in
    /// main.rs
    pub(crate) fn render_search_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let input = self.search_input()?;
        let total = self.search_matches.len();
        // No matches shows 0/0
        let counter = if total == 0 {
            "0/0".to_string()
        } else {
            format!("{}/{}", self.active_match + 1, total)
        };
        Some(
            h_flex()
                .key_context("thread-search")
                .w_full()
                .max_w(px(860.))
                .mx_auto()
                .px_4()
                .py_2()
                .gap_2()
                .items_center()
                .child(div().flex_1().child(Input::new(input).small()))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(counter),
                )
                .child(
                    Button::new("thread-search-prev")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronUp)
                        .on_click(cx.listener(|this, _, _, cx| this.goto_match(false, cx))),
                )
                .child(
                    Button::new("thread-search-next")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronDown)
                        .on_click(cx.listener(|this, _, _, cx| this.goto_match(true, cx))),
                )
                .child(
                    Button::new("thread-search-close")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx))),
                )
                .into_any_element(),
        )
    }
}
