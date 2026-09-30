use super::*;

impl ThreadView {
    /// 惰性创建搜索输入框（首开时）。订阅：输入变化重跑搜索；
    /// Enter=下一个、Shift+Enter=上一个（单行输入框的 Enter/Shift+Enter
    /// 都发 PressEnter、不插换行，action 在输入框层就被消费，不会冒泡到
    /// 输入区成发送——同 composer 用 PressEnter 发送的路径）
    pub(crate) fn ensure_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_input.is_some() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("在会话中搜索…"));
        let subscription = cx.subscribe(&input, |this, _, event, cx| match event {
            InputEvent::Change => this.run_search(cx),
            InputEvent::PressEnter { shift, .. } => this.goto_match(!shift, cx),
            _ => {}
        });
        self.search_input = Some((input, subscription));
    }

    /// 搜索输入框实体（仅首开以后存在）
    pub(crate) fn search_input(&self) -> Option<&Entity<InputState>> {
        self.search_input.as_ref().map(|(input, _)| input)
    }

    /// 打开搜索条并聚焦输入框；已有 query 时重跑一次（内容可能已流式更新）
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

    /// 关闭搜索条：清 query、清所有段高亮、清命中、复位活动下标。
    /// set_value 不发 Change（上游 emit_events=false），这里全部显式复位
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

    /// 会话内搜索：大小写不敏感地命中所有 Markdown 段（rendered_text 与
    /// query 都小写化后做字节级 match_indices）。无大小写的文字（中文等）
    /// 小写化是恒等，区间即原文字节偏移；极少数小写化后字节数变化的字符
    /// （如土耳其语 İ）区间会偏，该段整批被 set_range_highlights 拒绝
    /// （Err 忽略）降级为无高亮，不影响其他段
    pub(crate) fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self
            .search_input()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let query_lc = query.to_lowercase();
        // 复用条件：query 没变且段 revision 没变；query 变了全段重搜
        let same_query = self.search_query.to_lowercase() == query_lc;
        self.search_query = query;
        // 上轮有命中的段：本轮不再命中时要清掉旧高亮
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

    /// 重打某段的搜索高亮：活动命中更深的 accent 0.5，其余 0.25；无命中则清。
    /// set_range_highlights 整批校验区间、任一非法整批拒绝——Err 忽略，
    /// 该段降级为不高亮（区间坐标系说明见 run_search）
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

    /// 清掉所有 Markdown 段的搜索高亮（段上没有高亮时上游是 no-op）
    pub(crate) fn clear_search_highlights(&mut self, cx: &mut Context<Self>) {
        for message in &self.messages {
            for segment in &message.segments {
                if let Segment::Markdown { state, .. } = segment {
                    state.update(cx, |state, cx| state.clear_range_highlights(cx));
                }
            }
        }
    }

    /// 跳到下一个/上一个命中（回绕）：旧/新活动命中所在段重打高亮换色，
    /// 目标行 reveal 进可视区。跳转 = 离开底部，暂停跟随；reveal 的滚动
    /// 后续帧才生效，nav_jump 抑制一次「回到底部自动恢复跟随」（同导航条
    /// 跳转的时序，见 render 里的恢复逻辑）
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

    /// 会话内搜索条：输入框 + 命中计数 + 上/下一个 + 关闭（消息列表之上的
    /// 固定行）。key_context("thread-search") 让 Esc → CloseThreadSearch
    /// 绑定生效（输入框的 Escape action 默认 cx.propagate() 放行到该上下文，
    /// 见 main.rs 键绑定）；关闭走 main.rs 转发回 close_search
    pub(crate) fn render_search_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let input = self.search_input()?;
        let total = self.search_matches.len();
        // 无命中显示 0/0
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
