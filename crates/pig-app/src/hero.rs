use super::*;

impl AppView {
    /// Hero state: no current session, or the current session has no messages
    pub(crate) fn is_hero(&self, cx: &App) -> bool {
        match &self.current {
            None => true,
            Some(sid) => self
                .views
                .get(sid)
                .map(|views| views.thread.read(cx).is_empty())
                .unwrap_or(true),
        }
    }

    pub(crate) fn push_hero_info(&self, cx: &mut Context<Self>) {
        let label = self
            .hero_cwd
            .as_ref()
            .map(|cwd| {
                cwd.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| cwd.display().to_string())
            })
            .unwrap_or_else(|| rust_i18n::t!("hero.select_workspace").to_string());
        let mut cwds: Vec<String> = self
            .metas
            .iter()
            .map(|m| m.cwd.display().to_string())
            .collect();
        if let Some(cwd) = &self.hero_cwd {
            let current = cwd.display().to_string();
            if !cwds.contains(&current) {
                cwds.insert(0, current);
            }
        }
        let mut seen = std::collections::HashSet::new();
        cwds.retain(|c| seen.insert(c.clone()));
        let (branch, branches, is_git) = (
            self.hero_branch.clone(),
            self.hero_branches.clone(),
            self.hero_is_git,
        );
        let cwd = self.hero_cwd.as_ref().map(|c| c.display().to_string());
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_info(cwd, label, cwds, branch, branches, is_git, cx);
        });
    }

    /// Hero defaults: spread the workspace's most recently active session's
    /// model/mode/reasoning level onto the composer as the defaults for the next
    /// new session (the user can still change them; hero_send creates with the
    /// current selection). With no seed (a new workspace) nothing changes,
    /// keeping the app defaults/last selection.
    pub(crate) fn apply_hero_defaults(&mut self, cx: &mut Context<Self>) {
        let cwd = self.hero_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let Some(seed) = self
            .metas
            .iter()
            .filter(|m| !m.archived && m.cwd == cwd)
            .max_by_key(|m| m.updated_at)
            .cloned()
        else {
            return;
        };
        self.exec_mode = seed.exec_mode;
        self.reasoning_level = seed.reasoning_level.clone();
        // Model display name: look up the provider name in config by
        // provider_id, falling back to model_id when not found. Do not overwrite
        // when the user has explicitly picked a model in hero state (mode and
        // reasoning level still get seeded)
        let label = match (&seed.provider_id, &seed.model_id) {
            (Some(p), Some(m)) if !self.hero_model_dirty => {
                self.current_model = Some((p.clone(), m.clone()));
                Some(self.model_display_label(p, m))
            }
            // User explicitly picked a model: leave the model display alone, only seed mode/reasoning level
            _ if self.hero_model_dirty => None,
            // Seed has no model choice: show the "no model configured" placeholder (no leftover label from the previous session;
            // the empty sentinel localizes at the composer's draw time)
            _ => Some(String::new()),
        };
        self.composer.update(cx, |composer, cx| {
            composer.set_exec_mode(seed.exec_mode, cx);
            composer.set_reasoning_level(seed.reasoning_level.clone(), cx);
            composer.set_fs_access(seed.fs_read_outside, seed.fs_write_outside, cx);
            if let Some(label) = label {
                composer.set_model_name(label, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn enter_hero(&mut self, cx: &mut Context<Self>) {
        self.current = None;
        self.git_branch = None;
        self.title_branches = vec![];
        self.title_branch_menu_open = false;
        self.hero_error = None;
        // A new hero cycle: reset the explicit-model-choice flag (the workspace seed takes effect again)
        self.hero_model_dirty = false;
        self.composer.update(cx, |composer, cx| {
            composer.clear_context_usage(cx);
            // Progress/task/change chips are also previous-session state; clear them together (the setters collapse their popovers)
            composer.set_todos(vec![], cx);
            composer.set_tasks(vec![], cx);
            composer.set_changes(0, 0, vec![], cx);
        });
        if let Some(cwd) = self.hero_cwd.clone() {
            self.agent.git_info(cwd);
        } else {
            self.hero_branch = None;
            self.hero_branches = vec![];
            self.hero_is_git = false;
        }
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_mode(true, cx);
            composer.set_streaming(false, cx);
        });
        self.push_hero_info(cx);
        self.refresh_sidebar(cx);
        self.apply_hero_defaults(cx);
        cx.notify();
    }

    pub(crate) fn hero_send(
        &mut self,
        text: String,
        files: Vec<String>,
        images: Vec<pig_protocol::PendingImage>,
        mode: ExecMode,
        cx: &mut Context<Self>,
    ) {
        self.pending_first_send = Some((text, files, images, mode));
        let cwd = self.hero_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        // Carry the UI's current selections: the new session initializes with
        // them (not the workspace seed), so a returning SessionConfigured cannot
        // overwrite the mode/reasoning level the user just picked
        let (provider_id, model_id) = match self.current_model.clone() {
            Some((p, m)) => (Some(p), Some(m)),
            None => (None, None),
        };
        tracing::info!(
            "hero_send new session: cwd={} model={:?} thinking={:?}",
            cwd.display(),
            provider_id.as_deref().zip(model_id.as_deref()),
            self.reasoning_level
        );
        self.agent.new_session(
            cwd,
            provider_id,
            model_id,
            self.reasoning_level.clone(),
            Some(mode),
            // The plan toggle carries the composer's draft state (checking "plan" on hero starts the new session with it on)
            Some(self.plan_enabled),
        );
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_mode(false, cx);
        });
        cx.notify();
    }

    /// Close the Yolo confirm dialog and refocus the input
    pub(crate) fn close_yolo_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.yolo_confirm_open = false;
        self.composer.update(cx, |composer, cx| {
            composer.focus_input(window, cx);
        });
        cx.notify();
    }

    /// The Yolo confirm dialog's "enable unrestricted mode": apply the mode and close
    pub(crate) fn confirm_yolo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_yolo_confirm(window, cx);
        self.apply_exec_mode(ExecMode::Yolo, cx);
    }

    /// The "enable unrestricted mode?" confirm dialog (a ModelDialog-style
    /// overlay: mask plus centered card). Cancel/mask click/Esc do not apply;
    /// only confirm switches to Yolo. It pops up on every switch and does not
    /// remember the choice.
    pub(crate) fn render_yolo_confirm(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("yolo-confirm-overlay")
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_yolo_confirm(window, cx);
            }))
            .child(
                v_flex()
                    .id("yolo-confirm")
                    .track_focus(&self.yolo_confirm_focus)
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close_yolo_confirm(window, cx);
                        }
                    }))
                    .on_click(|_, _, cx| cx.stop_propagation()) // clicking the card must not trigger the mask cancel
                    .w(px(420.))
                    .gap_3()
                    .p_4()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .text_color(cx.theme().danger)
                            .child(rust_i18n::t!("hero.yolo_title")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("hero.yolo_body"))
                            .child(rust_i18n::t!("hero.yolo_note")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                // Same size as the questionnaire/approval-bar buttons (Small) so confirm-dialog button sizes stay consistent
                                Button::new("yolo-cancel")
                                    .label(rust_i18n::t!("common.cancel"))
                                    .outline()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_yolo_confirm(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("yolo-confirm")
                                    .label(rust_i18n::t!("hero.yolo_confirm"))
                                    .danger()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_yolo(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn sync_hero_mode(&mut self, cx: &mut Context<Self>) {
        let hero = self.is_hero(cx) && self.pending_first_send.is_none();
        self.composer
            .update(cx, |composer, cx| composer.set_hero_mode(hero, cx));
        if hero {
            self.push_hero_info(cx);
        }
    }

    /// For self-tests.
    pub fn debug_is_hero(&self, cx: &App) -> bool {
        self.is_hero(cx)
    }

    pub(crate) fn render_hero(&self, cx: &mut Context<Self>) -> AnyElement {
        let hour = time::OffsetDateTime::now_local()
            .map(|t| t.hour() as i64)
            .unwrap_or_else(|_| {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0) as i64;
                (secs / 3600 + 8) % 24
            });
        let greeting = match hour {
            5..=11 => rust_i18n::t!("hero.greeting_morning"),
            12..=17 => rust_i18n::t!("hero.greeting_afternoon"),
            _ => rust_i18n::t!("hero.greeting_evening"),
        };

        let chips: Vec<(String, String)> = vec![
            (
                rust_i18n::t!("hero.chip_summarize").to_string(),
                rust_i18n::t!("hero.chip_summarize_prompt").to_string(),
            ),
            (
                rust_i18n::t!("hero.chip_fix").to_string(),
                rust_i18n::t!("hero.chip_fix_prompt").to_string(),
            ),
            (
                rust_i18n::t!("hero.chip_test").to_string(),
                rust_i18n::t!("hero.chip_test_prompt").to_string(),
            ),
        ];

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_5()
            .child(div().text_xl().font_semibold().child(greeting))
            .child(
                div()
                    .w_full()
                    .max_w(px(720.))
                    .px_4()
                    .child(self.composer.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .children(chips.into_iter().map(|(label, fill)| {
                        let composer = self.composer.clone();
                        h_flex()
                            .id(("chip", label.len()))
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .border_1()
                            .border_color(cx.theme().border)
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .cursor_pointer()
                            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                            .child(label)
                            .on_click(move |_, window, cx| {
                                composer.update(cx, |composer, cx| {
                                    composer.fill_text(&fill, window, cx);
                                });
                            })
                    })),
            )
            .when_some(self.hero_error.clone(), |this, error| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(crate::errors::core_error_text(&error)),
                )
            })
            .into_any_element()
    }
}
