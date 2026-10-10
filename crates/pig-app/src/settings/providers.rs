use super::*;

impl SettingsView {
    pub(crate) fn open_model_dialog(
        &mut self,
        editing: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = editing.and_then(|ix| {
            self.selected_provider()
                .and_then(|p| p.models.get(ix))
                .cloned()
        });
        let snapshot = model.clone();
        let model =
            model.unwrap_or_else(|| ModelConfig::new("", NEW_MODEL_CONTEXT, NEW_MODEL_MAX_OUTPUT));
        // Gray placeholder = the JSON actually injected by default for this
        // provider's format (chat-completions has no default: the Zhipu-style
        // example stands in, empty there means no injection)
        let web_search_placeholder = match self
            .selected_provider()
            .map(|p| p.api_format)
            .unwrap_or(pig_protocol::ApiFormat::OpenAiChat)
        {
            pig_protocol::ApiFormat::AnthropicMessages => {
                r#"{"type": "web_search_20250305", "name": "web_search"}"#.to_string()
            }
            pig_protocol::ApiFormat::OpenAiResponses => r#"{"type": "web_search"}"#.to_string(),
            pig_protocol::ApiFormat::OpenAiChat => {
                r#"{"type": "web_search", "web_search": {"enable": true}}"#.to_string()
            }
        };
        let dialog = ModelDialog {
            editing,
            id: cx.new(|cx| InputState::new(window, cx).default_value(model.id.clone())),
            context_window: cx.new(|cx| {
                InputState::new(window, cx).default_value(model.context_window.to_string())
            }),
            max_tokens: cx.new(|cx| {
                InputState::new(window, cx).default_value(model.max_output_tokens.to_string())
            }),
            advanced_open: false,
            input_image: model.input_image,
            input_video: model.input_video,
            input_pdf: model.input_pdf,
            cap_structured: model.cap_structured,
            cap_strict_tools: model.cap_strict_tools,
            cap_web_search: model.cap_web_search,
            cap_system_msg: model.cap_system_msg,
            enabled: model.enabled,
            reasoning_levels: model.reasoning_levels.clone(),
            reasoning_labels: model.reasoning_labels.clone(),
            // A default level already removed from the level list doesn't count
            default_level: model
                .default_reasoning_level
                .clone()
                .filter(|lv| model.reasoning_levels.contains(lv)),
            new_level: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(rust_i18n::t!("settings.models.new_level_placeholder"))
            }),
            new_label: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(rust_i18n::t!("settings.models.new_label_placeholder"))
            }),
            params_json: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(3, 8)
                    .default_value(if model.reasoning_params.is_empty() {
                        "{}".to_string()
                    } else {
                        serde_json::to_string_pretty(&model.reasoning_params).unwrap_or_default()
                    })
            }),
            params_error: None,
            web_search_json: cx.new(|cx| {
                let state = TextareaState::new(window, cx).auto_grow(2, 6);
                if let Some(tool) = &model.web_search_tool {
                    state.default_value(serde_json::to_string_pretty(tool).unwrap_or_default())
                } else {
                    state.placeholder(web_search_placeholder)
                }
            }),
            web_search_error: None,
            snapshot,
            looked_up_id: None,
            lookup_state: LookupState::Idle,
            lookup_overwrite: false,
            body_scroll: ScrollHandle::new(),
        };
        // Model ID input confirmed (Enter/blur) → query models.dev for
        // auto-fill. Each dialog reopen creates a fresh input; stale
        // subscriptions are excluded by entity id
        let id_input = dialog.id.clone();
        self._subscriptions.push(cx.subscribe_in(
            &id_input,
            window,
            |this, emitter, event: &gpui_kit::component::input::InputEvent, _, cx| {
                let Some(dialog) = &this.model_dialog else {
                    return;
                };
                if dialog.id.entity_id() != emitter.entity_id() {
                    return;
                }
                match event {
                    gpui_kit::component::input::InputEvent::PressEnter { .. }
                    | gpui_kit::component::input::InputEvent::Blur => {
                        this.maybe_lookup_model(false, cx);
                    }
                    // Input changed: clear the previous lookup's status hint
                    gpui_kit::component::input::InputEvent::Change => {
                        let dirty = this
                            .model_dialog
                            .as_mut()
                            .is_some_and(|d| d.lookup_state != LookupState::Idle);
                        if dirty {
                            if let Some(d) = this.model_dialog.as_mut() {
                                d.lookup_state = LookupState::Idle;
                            }
                            cx.notify();
                        }
                    }
                    _ => {}
                }
            },
        ));
        self.model_dialog = Some(dialog);
        cx.notify();
    }

    pub(crate) fn save_model_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.model_dialog.take() else {
            return;
        };
        let Some(p_ix) = self.selected else { return };

        let id = dialog.id.read(cx).value().trim().to_string();
        let context_window = dialog.context_window.read(cx).value().trim().parse::<u64>();
        let max_tokens = dialog.max_tokens.read(cx).value().trim().parse::<u64>();
        let params_raw = dialog.params_json.read(cx).value().to_string();
        let web_search_raw = dialog.web_search_json.read(cx).value().trim().to_string();
        let web_search_tool: Result<Option<serde_json::Value>, serde_json::Error> =
            if web_search_raw.is_empty() {
                Ok(None)
            } else {
                serde_json::from_str(&web_search_raw).map(Some)
            };
        let params: Result<
            std::collections::HashMap<String, serde_json::Value>,
            serde_json::Error,
        > = serde_json::from_str(params_raw.trim());

        let error = if id.is_empty() {
            Some(rust_i18n::t!("settings.models.err_id_empty").to_string())
        } else if context_window.is_err() || context_window.as_ref().ok() == Some(&0) {
            Some(rust_i18n::t!("settings.models.err_context").to_string())
        } else if max_tokens.is_err() || max_tokens.as_ref().ok() == Some(&0) {
            Some(rust_i18n::t!("settings.models.err_max_tokens").to_string())
        } else if let Err(e) = &params {
            Some(rust_i18n::t!("settings.models.err_params_json", error = e).to_string())
        } else if let Err(e) = &web_search_tool {
            Some(rust_i18n::t!("settings.models.err_web_search_json", error = e).to_string())
        } else {
            None
        };
        if let Some(error) = error {
            let params_failed = params.is_err();
            let mut dialog = dialog;
            dialog.params_error = params_failed.then_some(error.clone());
            dialog.web_search_error = (!params_failed && web_search_tool.is_err()).then_some(error);
            self.model_dialog = Some(dialog);
            cx.notify();
            return;
        }

        let model = ModelConfig {
            id,
            enabled: dialog.enabled,
            context_window: context_window.unwrap(),
            max_output_tokens: max_tokens.unwrap(),
            input_image: dialog.input_image,
            input_video: dialog.input_video,
            input_pdf: dialog.input_pdf,
            cap_structured: dialog.cap_structured,
            cap_strict_tools: dialog.cap_strict_tools,
            cap_web_search: dialog.cap_web_search,
            web_search_tool: web_search_tool.unwrap_or(None),
            cap_system_msg: dialog.cap_system_msg,
            reasoning_levels: dialog.reasoning_levels.clone(),
            // The default level must still be in the level list
            default_reasoning_level: dialog
                .default_level
                .clone()
                .filter(|lv| dialog.reasoning_levels.contains(lv)),
            // Display names only keep level ids that still exist (guards
            // against list edits outside the chips)
            reasoning_labels: dialog
                .reasoning_labels
                .iter()
                .filter(|(id, label)| dialog.reasoning_levels.contains(*id) && !label.is_empty())
                .map(|(id, label)| (id.clone(), label.clone()))
                .collect(),
            reasoning_params: params.unwrap(),
        };
        let provider = &mut self.config.providers[p_ix];
        match dialog.editing {
            Some(ix) => provider.models[ix] = model,
            None => provider.models.push(model),
        }
        cx.emit(SettingsEvent::Save(self.config.clone()));
        cx.notify();
    }
}
impl SettingsView {
    /// Left column provider row: bordered row card; selected gets a primary
    /// border, hover gets accent at half opacity
    pub(crate) fn render_provider_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let provider = &self.config.providers[ix];
        let selected = self.selected == Some(ix);
        let dot_color = if provider.enabled {
            cx.theme().success
        } else {
            cx.theme().muted_foreground
        };
        h_flex()
            .id(("provider", ix))
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(if selected {
                cx.theme().primary
            } else {
                cx.theme().border
            })
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_provider(ix, cx);
            }))
            .child(div().size_2().flex_none().rounded_full().bg(dot_color))
            .child(
                div()
                    .text_sm()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(provider.name.clone()),
            )
            .into_any_element()
    }

    /// Model ID input confirmed: only query models.dev when non-empty and
    /// different from the last lookup. overwrite=true means "reset form"
    /// semantics (query results fully overwrite plus missing fields fall back
    /// to defaults)
    pub(crate) fn maybe_lookup_model(&mut self, overwrite: bool, cx: &mut Context<Self>) {
        let Some(dialog) = &self.model_dialog else {
            return;
        };
        let id = dialog.id.read(cx).value().trim().to_string();
        if id.is_empty() || dialog.looked_up_id.as_deref() == Some(id.as_str()) {
            return;
        }
        self.model_dialog
            .as_mut()
            .expect("dialog checked above")
            .looked_up_id = Some(id.clone());
        let dialog = self.model_dialog.as_mut().expect("dialog checked above");
        dialog.lookup_state = LookupState::Pending;
        dialog.lookup_overwrite = overwrite;
        cx.emit(SettingsEvent::LookupModel(id));
    }

    /// Backfill the dialog from a models.dev lookup result. Only applied when
    /// the event matches the ID currently being edited in the dialog.
    /// Reset-triggered lookups (lookup_overwrite): field = source value ??
    /// new-model default, params JSON regenerated unconditionally;
    /// Enter/blur-triggered lookups: gentle filling — overwrite only what the
    /// source has, leave user values for missing fields, keep the hand-tuned
    /// params JSON
    pub fn apply_model_info(
        &mut self,
        id: &str,
        info: Option<pig_protocol::ModelRegistryInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let overwrite = self
            .model_dialog
            .as_ref()
            .is_some_and(|d| d.lookup_overwrite);
        let Some(dialog) = &mut self.model_dialog else {
            return;
        };
        if dialog.id.read(cx).value().trim() != id {
            return;
        }
        let Some(info) = info else {
            // None = network failure or not listed in the source: unlock this ID
            // so Enter can retry (core throttles for 10 minutes, so retries
            // don't hammer the network)
            dialog.looked_up_id = None;
            dialog.lookup_state = LookupState::NotFound;
            cx.notify();
            return;
        };
        dialog.lookup_state = LookupState::Idle;
        // Context window / max output: reset semantics fall back to new-model
        // defaults when the source lacks them; Enter semantics leave them
        // untouched when missing
        let context_src = info.context.or(info.input);
        if overwrite || context_src.is_some() {
            let context = context_src.unwrap_or(NEW_MODEL_CONTEXT);
            dialog
                .context_window
                .update(cx, |i, cx| i.set_value(context.to_string(), window, cx));
        }
        if overwrite || info.output.is_some() {
            let output = info.output.unwrap_or(NEW_MODEL_MAX_OUTPUT);
            dialog
                .max_tokens
                .update(cx, |i, cx| i.set_value(output.to_string(), window, cx));
        }
        if overwrite || !info.reasoning_levels.is_empty() {
            dialog
                .reasoning_labels
                .retain(|k, _| info.reasoning_levels.contains(k));
            // Common levels get localized display names; the rest show the id
            // itself
            for (level, label) in [
                ("none", rust_i18n::t!("settings.reasoning.none")),
                ("minimal", rust_i18n::t!("settings.reasoning.minimal")),
                ("low", rust_i18n::t!("settings.reasoning.low")),
                ("medium", rust_i18n::t!("settings.reasoning.medium")),
                ("high", rust_i18n::t!("settings.reasoning.high")),
                ("xhigh", rust_i18n::t!("settings.reasoning.xhigh")),
                ("max", rust_i18n::t!("settings.reasoning.max")),
            ] {
                if info.reasoning_levels.iter().any(|l| l == level) {
                    dialog.reasoning_labels.insert(level.into(), label.into());
                }
            }
            dialog.reasoning_levels = info.reasoning_levels.clone();
            // The level list changed: a default level pointing at a removed
            // level is voided
            dialog.default_level = dialog
                .default_level
                .take()
                .filter(|lv| info.reasoning_levels.contains(lv));
        }
        // Reasoning params JSON: reset semantics regenerate unconditionally from
        // levels plus API format; Enter semantics only generate suggestions when
        // not hand-tuned (empty object)
        let params_current = dialog.params_json.read(cx).value().trim().to_string();
        let untouched = serde_json::from_str::<serde_json::Value>(&params_current)
            .map(|v| v.as_object().is_some_and(|m| m.is_empty()))
            .unwrap_or(true);
        if overwrite || untouched {
            let api_format = self
                .selected
                .and_then(|ix| self.config.providers.get(ix))
                .map(|p| p.api_format)
                .unwrap_or(ApiFormat::OpenAiChat);
            let params = default_reasoning_params(&info.reasoning_levels, api_format);
            let has_params = !params.is_empty();
            let raw = serde_json::to_string_pretty(&serde_json::Value::Object(params))
                .unwrap_or_default();
            dialog
                .params_json
                .update(cx, |t, cx| t.set_value(raw, window, cx));
            if has_params {
                dialog.advanced_open = true;
            }
        }
        // Input modalities: reset semantics default everything to false when
        // the source lacks data; Enter semantics leave checkboxes untouched
        // when the source doesn't say
        if overwrite || !info.input_modalities.is_empty() {
            let has = |m: &str| info.input_modalities.iter().any(|v| v == m);
            dialog.input_image = has("image");
            dialog.input_video = has("video");
            dialog.input_pdf = has("pdf");
        }
        if overwrite || info.structured_output.is_some() {
            dialog.cap_structured = info.structured_output.unwrap_or(false);
        }
        // Provider-identity rule (pi's capability table): only lit for
        // OpenAI/DeepSeek/Z.ai picks; everything else leaves the checkbox
        // alone so manual overrides survive
        if overwrite || info.strict_tools.is_some() {
            dialog.cap_strict_tools = info.strict_tools.unwrap_or(false);
        }
        cx.notify();
    }

    pub(crate) fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(p_ix) = self.selected else {
            // Empty state (aligned with ZCode's PresetProviderPlaceholderCard:
            // centered muted hint)
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(IconName::Bot).size_8())
                .child(
                    div()
                        .text_sm()
                        .child(rust_i18n::t!("settings.models.select_or_add").to_string()),
                )
                .into_any_element();
        };
        let provider = self.config.providers[p_ix].clone();
        let provider_id = provider.id.clone();
        let enabled = provider.enabled;
        let test_result = self.test_results.get(&provider_id).cloned();

        v_flex()
            .flex_1()
            .gap_3()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .flex_1()
                            .child(provider.name.clone()),
                    )
                    .child(
                        Switch::new("provider-enabled")
                            .checked(enabled)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.config.providers[p_ix].enabled = *checked;
                                cx.emit(SettingsEvent::Save(this.config.clone()));
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("delete-provider")
                            .ghost()
                            .small()
                            .label(if self.delete_armed {
                                rust_i18n::t!("settings.common.confirm_delete")
                            } else {
                                rust_i18n::t!("common.delete")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.delete_armed {
                                    this.config.providers.remove(p_ix);
                                    this.selected = None;
                                    this.delete_armed = false;
                                    this.form_dirty = true;
                                    cx.emit(SettingsEvent::Save(this.config.clone()));
                                } else {
                                    this.delete_armed = true;
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.common.field_name").to_string()),
                    )
                    .child(Input::new(&self.name_input)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Base URL"),
                    )
                    .child(Input::new(&self.base_url_input)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.models.api_format").to_string()),
                    )
                    // Select with the same trigger as the inputs (Button's
                    // default 16px centering clashes with the form)
                    .child(
                        Select::new(&self.format_select)
                            .w_full()
                            .menu_width(px(360.)),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("API Key"),
                            )
                            // Key management page brought in by the preset
                            // provider (custom providers have no such entry)
                            .when_some(
                                provider.key_url.clone().filter(|u| !u.is_empty()),
                                |this, url| {
                                    this.child(div().flex_1()).child(
                                        // Text and icon laid out horizontally
                                        // (a block-level div would push the
                                        // icon to the next line)
                                        h_flex()
                                            .id("get-api-key")
                                            .gap_0p5()
                                            .text_xs()
                                            .text_color(cx.theme().primary)
                                            .cursor_pointer()
                                            .hover(|this| this.underline())
                                            .child(
                                                rust_i18n::t!("settings.models.get_api_key")
                                                    .to_string(),
                                            )
                                            .child(Icon::new(IconName::ExternalLink).size_3())
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                cx.open_url(&url);
                                            })),
                                    )
                                },
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(div().flex_1().child(Input::new(&self.api_key_input)))
                            .child(
                                Button::new("toggle-mask")
                                    .ghost()
                                    .small()
                                    .icon(if self.api_key_masked {
                                        IconName::EyeOff
                                    } else {
                                        IconName::Eye
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.api_key_masked = !this.api_key_masked;
                                        let masked = this.api_key_masked;
                                        this.api_key_input.update(cx, |i, cx| {
                                            i.set_masked(masked, window, cx);
                                        });
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when_some(test_result, |this, result| {
                // Localized at draw time: failures pass the upstream English
                // detail through verbatim
                let ok = matches!(result, pig_protocol::ConnTestResult::Connected { .. });
                let message = match result {
                    pig_protocol::ConnTestResult::Connected { status } => {
                        rust_i18n::t!("settings.models.test_ok", status = status).to_string()
                    }
                    pig_protocol::ConnTestResult::Timeout { .. } => {
                        rust_i18n::t!("settings.models.test_timeout").to_string()
                    }
                    pig_protocol::ConnTestResult::Failed { detail } => detail,
                };
                this.child(
                    div()
                        .text_xs()
                        .text_color(if ok {
                            cx.theme().success
                        } else {
                            cx.theme().danger
                        })
                        .child(message),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .mt_2()
                    .mb_1()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.models.models").to_string()),
                    )
                    .child(
                        Button::new("add-model")
                            .secondary()
                            .small()
                            .icon(IconName::Plus)
                            .label(rust_i18n::t!("settings.models.add_model"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_model_dialog(None, window, cx);
                            })),
                    ),
            )
            // Model list (aligned with ZCode: one rounded container with
            // multiple rows, divider lines between rows, dashed box for the
            // empty state)
            .child(if provider.models.is_empty() {
                h_flex()
                    .h_12()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .border_dashed()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(IconName::Info).size_4())
                    .child(div().child(rust_i18n::t!("settings.models.no_models").to_string()))
                    .into_any_element()
            } else {
                v_flex()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    // Same color band as the inputs (official
                    // input_background), avoiding a background color break
                    .bg(cx.theme().input_background())
                    .children(provider.models.iter().enumerate().map(|(m_ix, model)| {
                        let provider_id = provider_id.clone();
                        h_flex()
                            .id(("model", m_ix))
                            .gap_2()
                            .px_3()
                            .py_2()
                            .when(m_ix > 0, |this| {
                                this.border_t_1().border_color(cx.theme().border)
                            })
                            .child(div().text_sm().child(model.id.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(cx.theme().accent)
                                    .child(Self::format_ctx(model.context_window)),
                            )
                            .when(model.input_image, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .px_1()
                                        .rounded_sm()
                                        .bg(cx.theme().accent)
                                        .child(rust_i18n::t!("settings.models.vision").to_string()),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                Button::new(("test", m_ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Globe)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(SettingsEvent::TestProvider(provider_id.clone()));
                                    })),
                            )
                            .child(
                                Button::new(("edit-model", m_ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Settings2)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_model_dialog(Some(m_ix), window, cx);
                                    })),
                            )
                            .child(
                                Button::new(("del-model", m_ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Delete)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.config.providers[p_ix].models.remove(m_ix);
                                        cx.emit(SettingsEvent::Save(this.config.clone()));
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Switch::new(("model-enabled", m_ix))
                                    .checked(model.enabled)
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        this.config.providers[p_ix].models[m_ix].enabled = *checked;
                                        cx.emit(SettingsEvent::Save(this.config.clone()));
                                        cx.notify();
                                    })),
                            )
                    }))
                    .into_any_element()
            })
            .into_any_element()
    }
}

impl SettingsView {
    pub(crate) fn render_model_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.model_dialog else {
            return div().into_any_element();
        };
        let snapshot_exists = dialog.snapshot.is_some();

        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("model-dialog")
                    .w(px(560.))
                    .max_h(px(640.))
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    // Fixed header: title left, close button top-right
                    .child(
                        h_flex()
                            .p_4()
                            .pb_2()
                            .justify_between()
                            .child(
                                div()
                                    .text_lg()
                                    .font_semibold()
                                    .child(if snapshot_exists {
                                        rust_i18n::t!("settings.models.edit_title").to_string()
                                    } else {
                                        rust_i18n::t!("settings.models.add_model").to_string()
                                    }),
                            )
                            .child(
                                div()
                                    .id("model-dialog-close")
                                    .test_support()
                                    .p_1()
                                    .rounded(cx.theme().radius)
                                    .cursor_pointer()
                                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                                    .child(
                                        Icon::new(IconName::Close)
                                            .size_4()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.model_dialog = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    // Form body scrolls under the fixed header and footer. The
                    // cap must sit on the scroll element itself (same
                    // max_h + overflow pattern as the MCP/skill dialogs) —
                    // the shell only has a max-height, so a flex_1 child
                    // never gets a definite height to overflow against
                    .child(
                        div()
                            .relative()
                            .child(
                                v_flex()
                                    .id("model-dialog-body")
                                    .gap_3()
                                    .max_h(px(528.))
                                    .overflow_y_scroll()
                                    .track_scroll(&dialog.body_scroll)
                                    .pl_4()
                                    // Reserve the scrollbar's own 16px lane on
                                    // the right: the overlay Scrollbar lays out
                                    // absolute against this wrapper's content
                                    // box, so without the right padding it sits
                                    // on top of the form fields (same lane
                                    // reservation as the code-view cards)
                                    .pr(px(crate::code_view::CODE_SCROLLBAR_LANE))
                                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(rust_i18n::t!("settings.models.model_id").to_string()),
                                    )
                                    .child(match dialog.lookup_state {
                                        LookupState::Pending => h_flex()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(Spinner::new().small())
                                            .child(rust_i18n::t!("settings.models.lookup_pending").to_string())
                                            .into_any_element(),
                                        LookupState::NotFound => div()
                                            .text_xs()
                                            .text_color(cx.theme().warning)
                                            .child(rust_i18n::t!("settings.models.lookup_not_found").to_string())
                                            .into_any_element(),
                                        LookupState::Idle => div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .opacity(0.7)
                                            .child(rust_i18n::t!("settings.models.lookup_hint").to_string())
                                            .into_any_element(),
                                    }),
                            )
                            .child(Input::new(&dialog.id)),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(rust_i18n::t!("settings.models.context_window").to_string()))
                                    .child(Input::new(&dialog.context_window)),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(rust_i18n::t!("settings.models.max_output").to_string()))
                                    .child(Input::new(&dialog.max_tokens)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().flex_1().child(rust_i18n::t!("settings.models.enable_model").to_string()))
                            .child(
                                Switch::new("model-enabled-dialog")
                                    .checked(dialog.enabled)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        if let Some(dialog) = &mut this.model_dialog {
                                            dialog.enabled = *checked;
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .id("advanced-toggle")
                                    .gap_2()
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(dialog) = &mut this.model_dialog {
                                            dialog.advanced_open = !dialog.advanced_open;
                                        }
                                        cx.notify();
                                    }))
                                    .child(
                                        Icon::new(if dialog.advanced_open {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .size_4()
                                        .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(div().text_sm().child(rust_i18n::t!("settings.models.advanced").to_string())),
                            )
                            .when(dialog.advanced_open, |this| {
                                this.child(
                                    v_flex()
                                        .gap_2()
                                        .child(
                                            h_flex()
                                                .gap_3()
                                                .child(Checkbox::new("in-image").label(rust_i18n::t!("settings.models.input_image").as_ref()).checked(dialog.input_image).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.input_image = *v; } cx.notify(); })))
                                                .child(Checkbox::new("in-video").label(rust_i18n::t!("settings.models.input_video").as_ref()).checked(dialog.input_video).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.input_video = *v; } cx.notify(); })))
                                                .child(Checkbox::new("in-pdf").label("PDF").checked(dialog.input_pdf).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.input_pdf = *v; } cx.notify(); }))),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_3()
                                                .child(Checkbox::new("cap-struct").label(rust_i18n::t!("settings.models.cap_structured").as_ref()).checked(dialog.cap_structured).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_structured = *v; } cx.notify(); })))
                                                .child(Checkbox::new("cap-strict").label(rust_i18n::t!("settings.models.cap_strict_tools").as_ref()).checked(dialog.cap_strict_tools).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_strict_tools = *v; } cx.notify(); })))
                                                .child(
                                                    div()
                                                        .id("cap-strict-hint")
                                                        .flex_shrink_0()
                                                        .tooltip(move |window, cx| {
                                                            gpui_kit::component::tooltip::Tooltip::element(
                                                                move |_window, _cx| {
                                                                    div()
                                                                        .max_w(gpui_kit::px(320.))
                                                                        .child(rust_i18n::t!("settings.models.cap_strict_tools_hint").to_string())
                                                                },
                                                            )
                                                            .build(window, cx)
                                                        })
                                                        .child(
                                                            Icon::new(IconName::Info)
                                                                .size_3p5()
                                                                .text_color(cx.theme().muted_foreground),
                                                        ),
                                                )
                                                )
                                                .child(
                                                    h_flex()
                                                        .gap_3()
                                                .child(Checkbox::new("cap-web").label(rust_i18n::t!("settings.models.cap_web_search").as_ref()).checked(dialog.cap_web_search).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_web_search = *v; } cx.notify(); })))
                                                .child(Checkbox::new("cap-sys").label(rust_i18n::t!("settings.models.cap_system_msg").as_ref()).checked(dialog.cap_system_msg).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_system_msg = *v; } cx.notify(); }))),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .items_center()
                                                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(rust_i18n::t!("settings.models.web_search_tool").to_string()))
                                                        .child(
                                                            div()
                                                                .id("web-search-tool-hint")
                                                                .flex_shrink_0()
                                                                .tooltip(move |window, cx| {
                                                                    gpui_kit::component::tooltip::Tooltip::element(
                                                                        move |_window, _cx| {
                                                                            div()
                                                                                .max_w(gpui_kit::px(320.))
                                                                                .child(rust_i18n::t!("settings.models.web_search_tool_hint").to_string())
                                                                        },
                                                                    )
                                                                    .build(window, cx)
                                                                })
                                                                .child(
                                                                    Icon::new(IconName::Info)
                                                                        .size_3p5()
                                                                        .text_color(cx.theme().muted_foreground),
                                                                ),
                                                        ),
                                                )
                                                .child(Textarea::new(&dialog.web_search_json))
                                                .when_some(dialog.web_search_error.clone(), |this, error| {
                                                    this.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(cx.theme().danger)
                                                            .child(error),
                                                    )
                                                }),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(rust_i18n::t!("settings.models.reasoning_levels").to_string()))
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .children(dialog.reasoning_levels.iter().enumerate().map(|(ix, level)| {
                                                            let label = dialog.reasoning_labels.get(level).cloned();
                                                            let level_owned = level.clone();
                                                            h_flex()
                                                                .gap_1()
                                                                .px_2()
                                                                .py_0p5()
                                                                .rounded_full()
                                                                .bg(cx.theme().accent)
                                                                // Clicking the
                                                                // chip text:
                                                                // backfills the
                                                                // input row for
                                                                // editing
                                                                // (removed from
                                                                // the list;
                                                                // click + to add
                                                                // it back)
                                                                .child(
                                                                    div()
                                                                        .id(("edit-level", ix))
                                                                        .cursor_pointer()
                                                                        .on_click(cx.listener(move |this, _, window, cx| {
                                                                            if let Some(d) = &mut this.model_dialog {
                                                                                let label = d.reasoning_labels.remove(&level_owned).unwrap_or_default();
                                                                                d.reasoning_levels.remove(ix);
                                                                                if d.default_level.as_deref() == Some(level_owned.as_str()) {
                                                                                    d.default_level = None;
                                                                                }
                                                                                d.new_level.update(cx, |i, cx| i.set_value(level_owned.clone(), window, cx));
                                                                                d.new_label.update(cx, |i, cx| i.set_value(label, window, cx));
                                                                            }
                                                                            cx.notify();
                                                                        }))
                                                                        .child(
                                                                            h_flex()
                                                                                .gap_1()
                                                                                .child(div().text_xs().child(level.clone()))
                                                                                .when_some(label, |this, label| {
                                                                                    this.child(
                                                                                        div()
                                                                                            .text_xs()
                                                                                            .text_color(cx.theme().muted_foreground)
                                                                                            .child(format!("· {label}")),
                                                                                    )
                                                                                }),
                                                                        ),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .id(("del-level", ix))
                                                                        .cursor_pointer()
                                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                                            if let Some(d) = &mut this.model_dialog {
                                                                                let level = d.reasoning_levels.remove(ix);
                                                                                d.reasoning_labels.remove(&level);
                                                                                if d.default_level.as_deref() == Some(&level) {
                                                                                    d.default_level = None;
                                                                                }
                                                                            }
                                                                            cx.notify();
                                                                        }))
                                                                        .child(Icon::new(IconName::Close).size_3()),
                                                                )
                                                        }))
                                                        .child(
                                                            h_flex()
                                                                .gap_1()
                                                                .child(div().w(px(90.)).child(Input::new(&dialog.new_level).small()))
                                                                .child(div().w(px(110.)).child(Input::new(&dialog.new_label).small()))
                                                                .child(
                                                                    Button::new("add-level")
                                                                        .ghost()
                                                                        .xsmall()
                                                                        .icon(IconName::Plus)
                                                                        .on_click(cx.listener(|this, _, window, cx| {
                                                                            let (level, label) = this.model_dialog.as_ref().map(|d| {
                                                                                (
                                                                                    d.new_level.read(cx).value().trim().to_string(),
                                                                                    d.new_label.read(cx).value().trim().to_string(),
                                                                                )
                                                                            }).unwrap_or_default();
                                                                            if level.is_empty() { return; }
                                                                            if let Some(d) = &mut this.model_dialog {
                                                                                if !d.reasoning_levels.contains(&level) {
                                                                                    d.reasoning_levels.push(level.clone());
                                                                                }
                                                                                if label.is_empty() {
                                                                                    d.reasoning_labels.remove(&level);
                                                                                } else {
                                                                                    d.reasoning_labels.insert(level, label);
                                                                                }
                                                                                d.new_level.update(cx, |i, cx| i.set_value("", window, cx));
                                                                                d.new_label.update(cx, |i, cx| i.set_value("", window, cx));
                                                                            }
                                                                            cx.notify();
                                                                        })),
                                                                ),
                                                        ),
                                                ),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(rust_i18n::t!("settings.models.default_level").to_string()))
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .children(
                                                            // First entry
                                                            // "unset" = None;
                                                            // the rest are
                                                            // the level list's
                                                            // levels
                                                            [None]
                                                                .into_iter()
                                                                .chain(dialog.reasoning_levels.iter().cloned().map(Some))
                                                                .enumerate()
                                                                .map(|(ix, opt)| {
                                                                    let selected = dialog.default_level == opt;
                                                                    let label = opt.clone()
                                                                        .map(|lv| dialog.reasoning_labels.get(&lv).cloned().filter(|s| !s.is_empty()).unwrap_or(lv))
                                                                        .unwrap_or_else(|| rust_i18n::t!("settings.models.level_unset").to_string());
                                                                    div()
                                                                        .id(("default-level", ix))
                                                                        .px_2()
                                                                        .py_0p5()
                                                                        .rounded_full()
                                                                        .cursor_pointer()
                                                                        .when(selected, |this| this.bg(cx.theme().accent))
                                                                        .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                                            if let Some(d) = &mut this.model_dialog {
                                                                                d.default_level = opt.clone();
                                                                            }
                                                                            cx.notify();
                                                                        }))
                                                                        .child(div().text_xs().child(label))
                                                                })
                                                                .collect::<Vec<_>>(),
                                                        ),
                                                ),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(rust_i18n::t!("settings.models.params_mapping").to_string()))
                                                .child(Textarea::new(&dialog.params_json))
                                                .when_some(dialog.params_error.clone(), |this, error| {
                                                    this.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(cx.theme().danger)
                                                            .child(error),
                                                    )
                                                }),
                                        ),
                                )
                            }),
                            )
                            )
                            .child(gpui_kit::base::Scrollbar::vertical(&dialog.body_scroll)),
                    )
                    .child(
                        h_flex()
                            .p_4()
                            .pt_3()
                            .gap_2()
                            .child(
                                Button::new("reset-dialog")
                                    .ghost()
                                    .small()
                                    .label(rust_i18n::t!("settings.models.reset_form"))
                                    .disabled(!snapshot_exists)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let snapshot =
                                            this.model_dialog.as_ref().and_then(|d| d.snapshot.clone());
                                        let editing = this.model_dialog.as_ref().and_then(|d| d.editing);
                                        if snapshot.is_some() {
                                            this.open_model_dialog(editing, window, cx);
                                            // After restoring the snapshot, run
                                            // the models.dev auto-fill again for
                                            // the current model ID (reset
                                            // semantics: full overwrite plus
                                            // missing fields fall back to
                                            // defaults)
                                            this.maybe_lookup_model(true, cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("cancel-dialog")
                                    .outline()
                                    .small()
                                    .label(rust_i18n::t!("common.cancel"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.model_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("save-dialog")
                                    .primary()
                                    .small()
                                    .label(rust_i18n::t!("settings.common.save"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_model_dialog(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl SettingsView {}
