use super::*;

impl Composer {
    /// 惰性构建问卷实体（InputState 需要 window，set_question 拿不到，故在 render 调用）。
    /// 每题：题号为 item 名、题干为标题、可选 header 为题注、选项 label 为 choice value
    /// （提交直接回 label，与协议一致）、每题一个「其他」自由文本输入；全部必答
    /// （对齐原「每题作答才可提交」门控），数字键快捷选中。
    pub(crate) fn ensure_questionnaire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.questionnaire.is_some() {
            return;
        }
        let Some(question) = &self.question else {
            return;
        };
        let mut items = Vec::with_capacity(question.questions.len());
        for (ix, q) in question.questions.iter().enumerate() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("说说你的想法…"));
            let choices: Vec<QuestionnaireChoiceDefinition> = dedup_question_options(q)
                .into_iter()
                .map(|option| {
                    let choice = QuestionnaireChoiceDefinition::new(
                        option.label.clone(),
                        option.label.clone(),
                    );
                    match &option.description {
                        Some(description) => choice.with_description(description.clone()),
                        None => choice,
                    }
                })
                .collect();
            let mut item = QuestionnaireItemDefinition::new(ix.to_string(), q.question.clone())
                .with_required(true)
                .with_multiple(q.multi_select)
                .with_choices(choices)
                .with_input(QuestionnaireInputDefinition::new(input, "其他"));
            if let Some(header) = &q.header {
                item = item.with_description(header.clone());
            }
            items.push(item);
        }
        let state = cx.new(|cx| {
            QuestionnaireState::new(items, cx)
                .map(|state| state.with_shortcuts(QuestionnaireShortcutMode::Numbers))
                .expect("item 名为题号、choice 已按 label 去重，schema 必然合法")
        });
        let sub = cx.subscribe_in(
            &state,
            window,
            |this, _state, event: &QuestionnaireEvent, window, cx| match event {
                // submit() 校验通过先 emit Completed 再 emit Submit：
                // finish 内部 take(question)，只处理先到的一个
                QuestionnaireEvent::Completed(submission)
                | QuestionnaireEvent::Submit(submission) => {
                    this.finish_question_submission(submission, window, cx);
                }
                // 翻页：镜像当前页题号
                QuestionnaireEvent::CurrentItemChanged { current, .. } => {
                    if let Some(ix) = current
                        .as_ref()
                        .and_then(|name| name.parse::<usize>().ok())
                    {
                        this.question_current = ix;
                    }
                    cx.notify();
                }
                // 选择/「其他」输入变化：重绘问题条刷新选中态与按钮显隐
                QuestionnaireEvent::AnswerChanged(_) => cx.notify(),
                _ => cx.notify(),
            },
        );
        self.question_current = 0;
        self.questionnaire = Some((state, sub));
    }


    /// 问卷提交：按题序收集答案（选中 label 按选项定义序 + 非空「其他」文本 trim 后
    /// 追加为一个 label），发 QuestionReply、清问题态与问卷实体、焦点还回输入框。
    pub(crate) fn finish_question_submission(
        &mut self,
        submission: &QuestionnaireSubmission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(question) = self.question.take() else {
            return;
        };
        self.questionnaire = None;
        self.question_current = 0;
        let mut answers: Vec<Vec<String>> = Vec::with_capacity(question.questions.len());
        for ix in 0..question.questions.len() {
            let mut labels: Vec<String> = Vec::new();
            if let Some(answer) = submission.answer(&ix.to_string()) {
                labels.extend(answer.choices().iter().map(ToString::to_string));
                let other = answer
                    .freeform()
                    .map(|text| text.trim().to_string())
                    .unwrap_or_default();
                if !other.is_empty() {
                    labels.push(other);
                }
            }
            answers.push(labels);
        }
        cx.emit(ComposerEvent::QuestionReply {
            request_id: question.request_id,
            answers: Some(answers),
        });
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }


    /// 放弃：回复 None（core 按「用户选择不回答」继续，不算错误）。
    pub(crate) fn skip_question(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(question) = self.question.take() {
            self.questionnaire = None;
            self.question_current = 0;
            cx.emit(ComposerEvent::QuestionReply {
                request_id: question.request_id,
                answers: None,
            });
            self.input.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }


    /// 自测用：问题条是否在显示（返回当前页题干）。
    pub fn debug_question(&self) -> Option<String> {
        let question = self.question.as_ref()?;
        self.questionnaire.as_ref()?;
        let qix = self
            .question_current
            .min(question.questions.len().saturating_sub(1));
        question.questions.get(qix).map(|q| q.question.clone())
    }


    /// 自测用：等价点「下一题」（受当前题已作答门控；go_next 需要 Window，取首窗口）。
    pub fn debug_next_question_page(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.questionnaire.as_ref().map(|(state, _)| state.clone()) else {
            return;
        };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        let _ = window.update(cx, |_, window, cx| {
            state.update(cx, |state, cx| {
                state.go_next(window, cx);
            });
        });
        cx.notify();
    }


    /// 自测用：选中某题某选项（等价点击选项按钮；不管焦点与「其他」输入）。
    pub fn debug_select_question_option(&mut self, qix: usize, oix: usize, cx: &mut Context<Self>) {
        let Some(label) = self
            .question
            .as_ref()
            .and_then(|q| q.questions.get(qix))
            .and_then(|q| q.options.get(oix))
            .map(|option| option.label.clone())
        else {
            return;
        };
        let Some(state) = self.questionnaire.as_ref().map(|(state, _)| state.clone()) else {
            return;
        };
        let name = qix.to_string();
        state.update(cx, |state, cx| {
            let _ = state.activate_choice(&name, &label, cx);
        });
        cx.notify();
    }


    /// 自测用：等价点「提交」（问卷校验全过才经事件订阅发 QuestionReply，门控与原实现一致）。
    pub fn debug_submit_question(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.questionnaire.as_ref().map(|(state, _)| state.clone()) else {
            return;
        };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        let _ = window.update(cx, |_, window, cx| {
            state.update(cx, |state, cx| {
                state.submit(window, cx);
            });
        });
        cx.notify();
    }


    /// 问题条（gpui-kit Questionnaire 官方组件，向导分页一次一题）：Progress（题号/总数）→
    /// 当前题 Item（Title 题干 / Description 放可选 header / Choices 选项卡 / Input「其他」/
    /// Error 校验提示）→ Actions（[上一题] [放弃 Esc] [下一题]/[提交]，按问卷导航态自动显隐）。
    /// 数字键选选项、⏎ 确认进下一题/提交由 Questionnaire 根的键盘路由承接；「放弃」协议是
    /// 整卷 answers: None（官方 Skip 是逐题跳过，表达不了），自绘按钮 + 外层 Esc 走 skip_question。
    pub(crate) fn render_question_bar(
        &self,
        question: &PendingQuestion,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((state, _)) = &self.questionnaire else {
            // ensure_questionnaire 先于本帧渲染执行，正常到不了这里
            return div().into_any_element();
        };
        let progress = state.read(cx).progress();
        let mut item_parts = Vec::with_capacity(question.questions.len());
        for (ix, q) in question.questions.iter().enumerate() {
            let name = ix.to_string();
            // 与 ensure_questionnaire 同一套 label 去重（渲染 id 按 value 生成）
            let choice_parts: Vec<QuestionnaireChoice> = dedup_question_options(q)
                .into_iter()
                .map(|option| {
                    // 选项卡两行（label+description）：官方默认把指示器/角标对齐
                    // 首行文本（items_start），整行垂直居中更顺眼
                    QuestionnaireChoice::new(state, name.clone(), option.label.clone())
                        .items_center()
                })
                .collect();
            // 非当前题的 part 自行渲染为空，全部挂上即可
            item_parts.push(
                QuestionnaireItem::new(state, name.clone())
                    .child(QuestionnaireTitle::new(state, name.clone()))
                    .child(QuestionnaireDescription::new(state, name.clone()))
                    .child(QuestionnaireChoices::new(state, name.clone()).children(choice_parts))
                    .child(QuestionnaireInput::new(state, name.clone()))
                    .child(QuestionnaireError::new(state, name.clone())),
            );
        }
        v_flex()
            .id("question-bar")
            .w_full()
            .gap_3()
            .p_2()
            .track_focus(&self.question_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Esc 放弃走协议层（answers: None），Questionnaire 无对应概念
                if event.keystroke.key.as_str() == "escape" {
                    this.skip_question(window, cx);
                }
            }))
            .child(
                Questionnaire::new(state)
                    // 输入区是紧凑条形：整体小一号贴现状（行距/题干字重沿用 part 默认）
                    .with_size(Size::Small)
                    .child(
                        QuestionnaireProgress::new(state)
                            .child(format!("{}/{}", progress.current(), progress.total())),
                    )
                    .children(item_parts)
                    .child(
                        QuestionnaireActions::new(state)
                            .child(QuestionnairePrevious::new(state).child("上一题"))
                            .child(
                                // 与官方问卷动作按钮同尺寸（Small）
                                Button::new("question-skip")
                                    .secondary()
                                    .small()
                                    .label("放弃  Esc")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.skip_question(window, cx);
                                    })),
                            )
                            .child(QuestionnaireNext::new(state).child("下一题  ⏎"))
                            .child(QuestionnaireSubmit::new(state).child("提交  ⏎")),
                    ),
            )
            .into_any_element()
    }

}
