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
            cap_web_search: model.cap_web_search,
            cap_system_msg: model.cap_system_msg,
            enabled: model.enabled,
            reasoning_levels: model.reasoning_levels.clone(),
            reasoning_labels: model.reasoning_labels.clone(),
            // 已被删出等级表的默认档不算数
            default_level: model
                .default_reasoning_level
                .clone()
                .filter(|lv| model.reasoning_levels.contains(lv)),
            new_level: cx.new(|cx| InputState::new(window, cx).placeholder("等级名，如 high")),
            new_label: cx
                .new(|cx| InputState::new(window, cx).placeholder("显示名（可选），如 最高")),
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
            snapshot,
            looked_up_id: None,
            lookup_state: LookupState::Idle,
            lookup_overwrite: false,
        };
        // 模型 ID 输入完成（回车/失焦）→ 查 models.dev 自动填充。
        // 对话框每次重开都新建输入框，旧订阅靠 entity id 排除
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
                    // 输入变化：清掉上一次查询的状态提示
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
        self.format_popup = false;
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
        let params: Result<
            std::collections::HashMap<String, serde_json::Value>,
            serde_json::Error,
        > = serde_json::from_str(params_raw.trim());

        let error = if id.is_empty() {
            Some("模型 ID 不能为空".to_string())
        } else if context_window.is_err() || context_window.as_ref().ok() == Some(&0) {
            Some("上下文窗口必须是正整数".to_string())
        } else if max_tokens.is_err() || max_tokens.as_ref().ok() == Some(&0) {
            Some("最大输出 Token 必须是正整数".to_string())
        } else if let Err(e) = &params {
            Some(format!("推理参数映射不是合法 JSON 对象: {e}"))
        } else {
            None
        };
        if let Some(error) = error {
            let mut dialog = dialog;
            dialog.params_error = Some(error);
            self.format_popup = false;
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
            cap_web_search: dialog.cap_web_search,
            // 无设置 UI：编辑时保留手配的原值，新建为 None
            web_search_tool: dialog
                .snapshot
                .as_ref()
                .and_then(|m| m.web_search_tool.clone()),
            cap_system_msg: dialog.cap_system_msg,
            reasoning_levels: dialog.reasoning_levels.clone(),
            // 默认档必须仍在等级表内
            default_reasoning_level: dialog
                .default_level
                .clone()
                .filter(|lv| dialog.reasoning_levels.contains(lv)),
            // 显示名只保留仍存在的等级 id（防御chip外路径改列表）
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
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_provider(ix, cx);
            }))
            .child(div().size_2().rounded_full().bg(dot_color))
            .child(div().text_sm().flex_1().child(provider.name.clone()))
            .into_any_element()
    }

    /// 模型 ID 输入完成：非空且与上次查询不同才发起 models.dev 查询。
    /// overwrite=true 为「重置表单」语义（查询结果完全覆盖 + 缺字段回落默认值）
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

    /// models.dev 查询结果回填弹窗。只在事件对应弹窗当前编辑的 ID 时应用。
    /// 重置触发的查询（lookup_overwrite）：字段 = 数据源值 ?? 新建默认值，参数 JSON 无条件重生成；
    /// 回车/失焦触发的查询：温和填充——数据源有才覆盖，缺字段不动用户值，手配参数 JSON 保留
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
            // None = 网络失败或数据源未收录：解锁该 ID，回车可重试
            // （core 侧有 10 分钟节流，重试不会连打网络）
            dialog.looked_up_id = None;
            dialog.lookup_state = LookupState::NotFound;
            cx.notify();
            return;
        };
        dialog.lookup_state = LookupState::Idle;
        // 上下文窗口 / 最大输出：重置语义缺字段回落新建默认值；回车语义缺则不动
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
            // 常用等级配中文显示名，其余显示 id 本身
            for (level, label) in [
                ("none", "无"),
                ("minimal", "极简"),
                ("low", "低"),
                ("medium", "中"),
                ("high", "高"),
                ("xhigh", "超高"),
                ("max", "最高"),
            ] {
                if info.reasoning_levels.iter().any(|l| l == level) {
                    dialog.reasoning_labels.insert(level.into(), label.into());
                }
            }
            dialog.reasoning_levels = info.reasoning_levels.clone();
            // 等级表变了：指向已删等级的默认档作废
            dialog.default_level = dialog
                .default_level
                .take()
                .filter(|lv| info.reasoning_levels.contains(lv));
        }
        // 推理参数 JSON：重置语义无条件按等级 + API 格式重新生成；
        // 回车语义只在未手配（空对象）时生成建议值
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
        // 输入模态：重置语义缺数据按全 false；回车语义数据源未给则不动勾选
        if overwrite || !info.input_modalities.is_empty() {
            let has = |m: &str| info.input_modalities.iter().any(|v| v == m);
            dialog.input_image = has("image");
            dialog.input_video = has("video");
            dialog.input_pdf = has("pdf");
        }
        if overwrite || info.structured_output.is_some() {
            dialog.cap_structured = info.structured_output.unwrap_or(false);
        }
        cx.notify();
    }

    pub(crate) fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(p_ix) = self.selected else {
            return div()
                .flex_1()
                .child("选择或添加一个供应商")
                .into_any_element();
        };
        let provider = self.config.providers[p_ix].clone();
        let provider_id = provider.id.clone();
        let enabled = provider.enabled;
        let is_anthropic = provider.api_format == ApiFormat::AnthropicMessages;
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
                                "确认删除？"
                            } else {
                                "删除"
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
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("名称"),
                    )
                    .child(Input::new(&self.name_input)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
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
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("API 格式"),
                    )
                    .child(
                        div()
                            .on_prepaint({
                                let cell = self.format_btn_bounds.clone();
                                move |bounds, _, _| cell.set(bounds)
                            })
                            .child(
                                Button::new("api-format")
                                    .outline()
                                    .w_full()
                                    .label(if is_anthropic {
                                        "Anthropic Messages (/v1/messages)"
                                    } else {
                                        "OpenAI Chat Completions (/v1/chat/completions)"
                                    })
                                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                        // 弹层打开时点按钮：按下先触发弹层的 outside-close
                                        // （记录按下位置），紧随的 click 按同一位置吞掉，
                                        // 避免收起又马上弹开（main.rs 右侧面板菜单同款处理）
                                        let down_pos = match event {
                                            ClickEvent::Mouse(e) => Some(e.down.position),
                                            _ => None,
                                        };
                                        if this
                                            .format_outside_close
                                            .take()
                                            .is_some_and(|pos| Some(pos) == down_pos)
                                        {
                                            return;
                                        }
                                        this.format_popup = !this.format_popup;
                                        cx.notify();
                                    })),
                            )
                            .when(self.format_popup, |this| {
                                this.child(self.render_format_popup(p_ix, cx))
                            }),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("API Key"),
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
            .when_some(test_result, |this, (ok, message)| {
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
                    .child(div().text_sm().font_semibold().flex_1().child("模型列表"))
                    .child(
                        Button::new("add-model")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .label("添加模型")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_model_dialog(None, window, cx);
                            })),
                    ),
            )
            .children(provider.models.iter().enumerate().map(|(m_ix, model)| {
                let provider_id = provider_id.clone();
                h_flex()
                    .id(("model", m_ix))
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
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
                                .child("视觉"),
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
    }
}

impl SettingsView {
    /// API 格式下拉：deferred 到窗口层绘制，`Positioner::side(Bottom)` 锚定按钮正下方
    /// （与 main.rs 标签页 "+" 菜单同一模式）。详情列在 overflow_y_scroll 容器内，
    /// absolute 弹层会被滚动区裁剪，且后续表单兄弟（API Key 输入框等带背景元素）
    /// 按文档序画在其上，看起来就是弹层没有背景。
    pub(crate) fn render_format_popup(&self, p_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        deferred(
            Positioner::side(self.format_btn_bounds.get())
                .placement(Placement::Bottom)
                .align(Align::Start)
                .offset(px(4.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("format-popup")
                        .w(px(360.))
                        .py_1()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().popover)
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.format_popup = false;
                            this.format_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .children(
                            [
                                (
                                    "OpenAI Chat Completions (/v1/chat/completions)",
                                    ApiFormat::OpenAiChat,
                                ),
                                (
                                    "Anthropic Messages (/v1/messages)",
                                    ApiFormat::AnthropicMessages,
                                ),
                            ]
                            .map(|(label, format)| {
                                div()
                                    .id(gpui_kit::SharedString::from(label.to_string()))
                                    .px_3()
                                    .py_1()
                                    .text_sm()
                                    .cursor_pointer()
                                    .hover(|this| this.bg(cx.theme().accent))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.config.providers[p_ix].api_format = format;
                                        this.format_popup = false;
                                        cx.emit(SettingsEvent::Save(this.config.clone()));
                                        cx.notify();
                                    }))
                                    .child(label)
                            }),
                        ),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

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
                    .overflow_y_scroll()
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
                            .child(if snapshot_exists { "编辑模型配置" } else { "添加模型" }),
                    )
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
                                            .child("模型 ID"),
                                    )
                                    .child(match dialog.lookup_state {
                                        LookupState::Pending => h_flex()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(Spinner::new().small())
                                            .child("正在查询 models.dev…")
                                            .into_any_element(),
                                        LookupState::NotFound => div()
                                            .text_xs()
                                            .text_color(cx.theme().warning)
                                            .child("models.dev 未收录该 ID，回车可重试")
                                            .into_any_element(),
                                        LookupState::Idle => div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .opacity(0.7)
                                            .child("输入后回车，自动填充上下文/输出/推理等级（models.dev）")
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
                                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("上下文窗口"))
                                    .child(Input::new(&dialog.context_window)),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("最大输出 Token"))
                                    .child(Input::new(&dialog.max_tokens)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().flex_1().child("启用该模型"))
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
                                    .child(div().text_sm().child("高级配置")),
                            )
                            .when(dialog.advanced_open, |this| {
                                this.child(
                                    v_flex()
                                        .gap_2()
                                        .child(
                                            h_flex()
                                                .gap_3()
                                                .child(Checkbox::new("in-image").label("图片").checked(dialog.input_image).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.input_image = *v; } cx.notify(); })))
                                                .child(Checkbox::new("in-video").label("视频").checked(dialog.input_video).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.input_video = *v; } cx.notify(); })))
                                                .child(Checkbox::new("in-pdf").label("PDF").checked(dialog.input_pdf).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.input_pdf = *v; } cx.notify(); }))),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_3()
                                                .child(Checkbox::new("cap-struct").label("结构化输出").checked(dialog.cap_structured).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_structured = *v; } cx.notify(); })))
                                                .child(Checkbox::new("cap-web").label("原生联网搜索").checked(dialog.cap_web_search).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_web_search = *v; } cx.notify(); })))
                                                .child(Checkbox::new("cap-sys").label("对话中系统消息").checked(dialog.cap_system_msg).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_system_msg = *v; } cx.notify(); }))),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child("推理等级"))
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
                                                                // 点 chip 文本：回填到输入行编辑（从列表移除，点 + 重新加入）
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
                                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child("默认思考等级（新会话与切换模型的初始档）"))
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .children(
                                                            // 首项「不设置」= None，其余为等级表各档
                                                            [None]
                                                                .into_iter()
                                                                .chain(dialog.reasoning_levels.iter().cloned().map(Some))
                                                                .enumerate()
                                                                .map(|(ix, opt)| {
                                                                    let selected = dialog.default_level == opt;
                                                                    let label = opt.clone()
                                                                        .map(|lv| dialog.reasoning_labels.get(&lv).cloned().filter(|s| !s.is_empty()).unwrap_or(lv))
                                                                        .unwrap_or_else(|| "不设置".to_string());
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
                                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child("推理参数映射（JSON：等级 → 请求体合并参数）"))
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
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("reset-dialog")
                                    .ghost()
                                    .small()
                                    .label("重置表单")
                                    .disabled(!snapshot_exists)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let snapshot =
                                            this.model_dialog.as_ref().and_then(|d| d.snapshot.clone());
                                        let editing = this.model_dialog.as_ref().and_then(|d| d.editing);
                                        if snapshot.is_some() {
                                            this.open_model_dialog(editing, window, cx);
                                            // 恢复快照后按当前模型 ID 重新走 models.dev
                                            // 自动填充（重置语义：完全覆盖 + 缺字段回落默认）
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
                                    .label("取消")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.model_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("save-dialog")
                                    .primary()
                                    .small()
                                    .label("保存")
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
