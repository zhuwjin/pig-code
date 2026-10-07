use super::*;

impl Composer {
    /// Lazily build the questionnaire entity (InputState needs a window, which
    /// set_question cannot get, hence the call in render).
    /// Per question: the question index is the item name, the question text is the
    /// title, the optional header is the caption, and the option label is the choice
    /// value (submission sends labels directly, matching the protocol); each
    /// question gets an "other" free-text input; all questions are required (matching
    /// the original "every question answered before submit" gating), with number-key
    /// shortcuts for selection.
    pub(crate) fn ensure_questionnaire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.questionnaire.is_some() {
            return;
        }
        let Some(question) = &self.question else {
            return;
        };
        let mut items = Vec::with_capacity(question.questions.len());
        for (ix, q) in question.questions.iter().enumerate() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(rust_i18n::t!("composer.question_placeholder"))
            });
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
                .with_input(QuestionnaireInputDefinition::new(
                    input,
                    rust_i18n::t!("composer.question_other"),
                ));
            if let Some(header) = &q.header {
                item = item.with_description(header.clone());
            }
            items.push(item);
        }
        let state = cx.new(|cx| {
            QuestionnaireState::new(items, cx)
                .map(|state| state.with_shortcuts(QuestionnaireShortcutMode::Numbers))
                .expect("items are named by question number and choices are deduped by label, so the schema must be valid")
        });
        let sub = cx.subscribe_in(
            &state,
            window,
            |this, _state, event: &QuestionnaireEvent, window, cx| match event {
                // submit() emits Completed first, then Submit once validation passes:
                // finish takes the question internally, so only the first one is handled
                QuestionnaireEvent::Completed(submission)
                | QuestionnaireEvent::Submit(submission) => {
                    this.finish_question_submission(submission, window, cx);
                }
                // Page turn: mirror the current page's question index
                QuestionnaireEvent::CurrentItemChanged { current, .. } => {
                    if let Some(ix) = current.as_ref().and_then(|name| name.parse::<usize>().ok()) {
                        this.question_current = ix;
                    }
                    cx.notify();
                }
                // Selection/"other" input change: redraw the question bar to refresh
                // the selection state and button visibility
                QuestionnaireEvent::AnswerChanged(_) => cx.notify(),
                _ => cx.notify(),
            },
        );
        self.question_current = 0;
        self.questionnaire = Some((state, sub));
    }

    /// Questionnaire submission: collect answers in question order (selected labels
    /// in option-definition order plus the trimmed non-empty "other" text appended as
    /// one label), emit QuestionReply, clear the question state and questionnaire
    /// entity, and return focus to the composer.
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

    /// Abandon: reply None (core continues as "user chose not to answer", not an error).
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

    /// For self-test: whether the question bar is showing (returns the current page's
    /// question text).
    pub fn debug_question(&self) -> Option<String> {
        let question = self.question.as_ref()?;
        self.questionnaire.as_ref()?;
        let qix = self
            .question_current
            .min(question.questions.len().saturating_sub(1));
        question.questions.get(qix).map(|q| q.question.clone())
    }

    /// For self-test: equivalent to clicking "next question" (gated on the current
    /// question being answered; go_next needs a Window, take the first one).
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

    /// For self-test: select an option of a question (equivalent to clicking the
    /// option button; ignores focus and the "other" input).
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

    /// For self-test: equivalent to clicking "submit" (QuestionReply is sent through
    /// the event subscription only after all questionnaire validation passes, same
    /// gating as the original implementation).
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

    /// Question bar (gpui-kit's official Questionnaire component, wizard paging one
    /// question at a time): Progress (question index/total) → the current question's
    /// Item (Title for question text / Description for the optional header / Choices
    /// for option cards / Input for "other" / Error for validation hints) → Actions
    /// ([previous] [abandon Esc] [next]/[submit], auto-shown per the questionnaire
    /// navigation state). Number-key option selection and ⏎ to advance/submit are
    /// handled by the Questionnaire root's keyboard routing; the "abandon" protocol
    /// is a whole-questionnaire answers: None (the official Skip skips per question
    /// and cannot express this), so a custom button plus the outer Esc route to
    /// skip_question.
    pub(crate) fn render_question_bar(
        &self,
        question: &PendingQuestion,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((state, _)) = &self.questionnaire else {
            // ensure_questionnaire runs before this frame's rendering; normally
            // unreachable
            return div().into_any_element();
        };
        let progress = state.read(cx).progress();
        let mut item_parts = Vec::with_capacity(question.questions.len());
        for (ix, q) in question.questions.iter().enumerate() {
            let name = ix.to_string();
            // Same label dedup as ensure_questionnaire (render ids derive from value)
            let choice_parts: Vec<QuestionnaireChoice> = dedup_question_options(q)
                .into_iter()
                .map(|option| {
                    // Option cards have two lines (label + description): by default
                    // the official component aligns the indicator/badge with the
                    // first line's text (items_start); centering the whole row
                    // vertically looks better
                    QuestionnaireChoice::new(state, name.clone(), option.label.clone())
                        .items_center()
                })
                .collect();
            // Parts of non-current questions render empty on their own; just mount
            // them all
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
                // Esc abandons at the protocol level (answers: None); Questionnaire
                // has no such concept
                if event.keystroke.key.as_str() == "escape" {
                    this.skip_question(window, cx);
                }
            }))
            .child(
                Questionnaire::new(state)
                    // The input area is a compact bar: one size smaller overall to
                    // match the current look (line height/title weight keep the part
                    // defaults)
                    .with_size(Size::Small)
                    .child(QuestionnaireProgress::new(state).child(format!(
                        "{}/{}",
                        progress.current(),
                        progress.total()
                    )))
                    .children(item_parts)
                    .child(
                        QuestionnaireActions::new(state)
                            .child(
                                QuestionnairePrevious::new(state)
                                    .child(rust_i18n::t!("composer.question_prev")),
                            )
                            .child(
                                // Same size as the official questionnaire action
                                // buttons (Small)
                                Button::new("question-skip")
                                    .secondary()
                                    .small()
                                    .label(format!(
                                        "{}  Esc",
                                        rust_i18n::t!("composer.question_skip")
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.skip_question(window, cx);
                                    })),
                            )
                            .child(
                                QuestionnaireNext::new(state).child(format!(
                                    "{}  ⏎",
                                    rust_i18n::t!("composer.question_next")
                                )),
                            )
                            .child(QuestionnaireSubmit::new(state).child(format!(
                                "{}  ⏎",
                                rust_i18n::t!("composer.question_submit")
                            ))),
                    ),
            )
            .into_any_element()
    }
}
