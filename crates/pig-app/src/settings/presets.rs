use super::*;

/// Preset model entry: context/output limits/vision/structured output/reasoning
/// levels all carry the real values parsed from ZCode's built-in provider rules
/// (disabled/enabled are toggle-style reasoning levels)
pub(crate) struct PresetModel {
    pub(crate) id: &'static str,
    pub(crate) context: u64,
    pub(crate) max_output: u64,
    pub(crate) input_image: bool,
    pub(crate) cap_structured: bool,
    pub(crate) reasoning: &'static [&'static str],
}

/// Positional-argument constructor for PresetModel (for building models one by
/// one inside const arrays)
const fn pm(
    id: &'static str,
    context: u64,
    max_output: u64,
    input_image: bool,
    cap_structured: bool,
    reasoning: &'static [&'static str],
) -> PresetModel {
    PresetModel {
        id,
        context,
        max_output,
        input_image,
        cap_structured,
        reasoning,
    }
}

/// Display names for common reasoning levels (other levels show the id itself in
/// the UI); strings follow the UI language. Note: the display names are written
/// into config's reasoning_labels when "create a provider from a preset",
/// capturing the UI language at creation time; old values in stored configs
/// stay unchanged
pub(crate) fn reasoning_labels() -> [(&'static str, std::borrow::Cow<'static, str>); 9] {
    [
        ("none", rust_i18n::t!("settings.reasoning.none")),
        ("disabled", rust_i18n::t!("settings.reasoning.disabled")),
        ("enabled", rust_i18n::t!("settings.reasoning.enabled")),
        ("minimal", rust_i18n::t!("settings.reasoning.minimal")),
        ("low", rust_i18n::t!("settings.reasoning.low")),
        ("medium", rust_i18n::t!("settings.reasoning.medium")),
        ("high", rust_i18n::t!("settings.reasoning.high")),
        ("xhigh", rust_i18n::t!("settings.reasoning.xhigh")),
        ("max", rust_i18n::t!("settings.reasoning.max")),
    ]
}

/// Preset provider entry: base_url follows pig's concatenation convention (OpenAI
/// format appends /chat/completions, Anthropic format appends /v1/messages),
/// referencing ZCode's built-in provider catalog (config/provider/zcode-builtin.json)
pub(crate) struct PresetProvider {
    pub(crate) name: &'static str,
    pub(crate) base_url: &'static str,
    pub(crate) api_format: ApiFormat,
    /// Key management page (the "get key" entry)
    pub(crate) key_url: &'static str,
    /// Brand icon asset path (embedded via rust-embed, see assets/provider-icons/)
    pub(crate) icon: &'static str,
    /// Icon variant for dark themes (only OpenRouter has light/dark versions;
    /// None = universal)
    pub(crate) icon_dark: Option<&'static str>,
    /// Preset models (with real context/capability/reasoning-level values)
    pub(crate) models: &'static [PresetModel],
}

pub(crate) const PRESET_PROVIDERS: &[PresetProvider] = &[
    PresetProvider {
        name: "BigModel Coding Plan",
        icon: "provider-icons/bigmodel.svg",
        icon_dark: None,
        base_url: "https://open.bigmodel.cn/api/anthropic",
        api_format: ApiFormat::AnthropicMessages,
        key_url: "https://bigmodel.cn/coding-plan/personal/overview",
        models: &[
            pm(
                "GLM-5.3",
                1_000_000,
                128_000,
                false,
                false,
                &["low", "high", "max"],
            ),
            pm(
                "GLM-5.3-Flash",
                1_000_000,
                128_000,
                true,
                false,
                &["low", "high", "max"],
            ),
        ],
    },
    PresetProvider {
        name: "BigModel API",
        icon: "provider-icons/bigmodel.svg",
        icon_dark: None,
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        api_format: ApiFormat::OpenAiChat,
        key_url: "https://bigmodel.cn/usercenter/proj-mgmt/apikeys",
        models: &[
            pm(
                "GLM-5.3",
                1_000_000,
                128_000,
                false,
                false,
                &["low", "high", "max"],
            ),
            pm(
                "GLM-5.3-Flash",
                1_000_000,
                128_000,
                true,
                false,
                &["low", "high", "max"],
            ),
        ],
    },
    PresetProvider {
        name: "Z.ai Coding Plan",
        icon: "provider-icons/zai.png",
        icon_dark: None,
        base_url: "https://api.z.ai/api/anthropic",
        api_format: ApiFormat::AnthropicMessages,
        key_url: "https://z.ai/manage-apikey/apikey-list",
        models: &[
            pm(
                "GLM-5.3",
                1_000_000,
                128_000,
                false,
                false,
                &["low", "high", "max"],
            ),
            pm(
                "GLM-5.3-Flash",
                1_000_000,
                128_000,
                true,
                false,
                &["low", "high", "max"],
            ),
        ],
    },
    PresetProvider {
        name: "Z.ai API",
        icon: "provider-icons/zai.png",
        icon_dark: None,
        base_url: "https://api.z.ai/api/paas/v4",
        api_format: ApiFormat::OpenAiChat,
        key_url: "https://z.ai/manage-apikey/apikey-list",
        models: &[
            pm(
                "GLM-5.3",
                1_000_000,
                128_000,
                false,
                false,
                &["low", "high", "max"],
            ),
            pm(
                "GLM-5.3-Flash",
                1_000_000,
                128_000,
                true,
                false,
                &["low", "high", "max"],
            ),
        ],
    },
    PresetProvider {
        name: "Kimi",
        icon: "provider-icons/kimi.png",
        icon_dark: None,
        base_url: "https://api.moonshot.cn/anthropic",
        api_format: ApiFormat::AnthropicMessages,
        key_url: "https://platform.kimi.com/console/api-keys",
        models: &[
            pm(
                "kimi-k3",
                1_048_576,
                131_072,
                true,
                false,
                &["low", "high", "max"],
            ),
            pm("kimi-k2.7-code", 262_144, 98_304, true, false, &["enabled"]),
            pm(
                "kimi-k2.6",
                262_144,
                98_304,
                true,
                false,
                &["disabled", "enabled"],
            ),
        ],
    },
    PresetProvider {
        name: "MiniMax",
        icon: "provider-icons/minimax.png",
        icon_dark: None,
        base_url: "https://api.minimaxi.com/anthropic",
        api_format: ApiFormat::AnthropicMessages,
        key_url: "https://platform.minimaxi.com/console/access?tab=api-keys",
        models: &[
            pm(
                "MiniMax-M3",
                1_000_000,
                32_000,
                true,
                false,
                &["disabled", "enabled"],
            ),
            pm(
                "MiniMax-M2.7",
                204_800,
                32_000,
                false,
                false,
                &["disabled", "enabled"],
            ),
        ],
    },
    PresetProvider {
        name: "DeepSeek",
        icon: "provider-icons/deepseek.png",
        icon_dark: None,
        base_url: "https://api.deepseek.com/anthropic",
        api_format: ApiFormat::AnthropicMessages,
        key_url: "https://platform.deepseek.com/api_keys",
        models: &[
            pm(
                "deepseek-flash",
                1_000_000,
                384_000,
                true,
                false,
                &["disabled", "low", "high", "max"],
            ),
            pm(
                "deepseek-v4-pro",
                1_000_000,
                384_000,
                false,
                false,
                &["disabled", "low", "high", "max"],
            ),
        ],
    },
    PresetProvider {
        name: "Alibaba Cloud Bailian (China)",
        icon: "provider-icons/alibaba.png",
        icon_dark: None,
        base_url: "https://dashscope.aliyuncs.com/apps/anthropic",
        api_format: ApiFormat::AnthropicMessages,
        key_url: "https://bailian.console.aliyun.com/cn-beijing?tab=model",
        models: &[
            pm(
                "qwen3.8-max",
                1_000_000,
                131_072,
                true,
                true,
                &["low", "medium", "xhigh"],
            ),
            pm(
                "qwen3.8-flash",
                1_000_000,
                131_072,
                true,
                true,
                &["low", "medium", "xhigh"],
            ),
        ],
    },
    PresetProvider {
        name: "Alibaba Cloud Bailian (International)",
        icon: "provider-icons/alibaba.png",
        icon_dark: None,
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        api_format: ApiFormat::OpenAiChat,
        key_url: "https://modelstudio.console.aliyun.com/ap-southeast-1?tab=dashboard",
        models: &[
            pm(
                "qwen3.8-max",
                1_000_000,
                131_072,
                true,
                true,
                &["low", "medium", "xhigh"],
            ),
            pm(
                "qwen3.8-flash",
                1_000_000,
                131_072,
                true,
                true,
                &["low", "medium", "xhigh"],
            ),
        ],
    },
    PresetProvider {
        name: "Xiaomi MiMo",
        icon: "provider-icons/mimo.png",
        icon_dark: None,
        base_url: "https://api.xiaomimimo.com/anthropic",
        api_format: ApiFormat::AnthropicMessages,
        key_url: "https://platform.xiaomimimo.com/",
        models: &[
            pm(
                "mimo-v2.5-pro",
                1_000_000,
                131_072,
                false,
                false,
                &["disabled", "enabled"],
            ),
            pm(
                "mimo-v2.5",
                1_000_000,
                131_072,
                true,
                false,
                &["disabled", "enabled"],
            ),
        ],
    },
    PresetProvider {
        name: "Anthropic",
        icon: "provider-icons/anthropic.png",
        icon_dark: None,
        // pig appends /v1/messages for the Anthropic format; the official base
        // has no /v1 suffix
        base_url: "https://api.anthropic.com",
        api_format: ApiFormat::AnthropicMessages,
        key_url: "https://console.anthropic.com/settings/keys",
        models: &[
            pm(
                "claude-fable-5-1",
                1_000_000,
                128_000,
                true,
                true,
                &["low", "medium", "high", "xhigh", "max"],
            ),
            pm(
                "claude-fable-5",
                1_000_000,
                128_000,
                true,
                true,
                &["low", "medium", "high", "xhigh", "max"],
            ),
            pm(
                "claude-opus-5",
                1_000_000,
                128_000,
                true,
                true,
                &["low", "medium", "high", "xhigh", "max"],
            ),
            pm(
                "claude-sonnet-5",
                1_000_000,
                128_000,
                true,
                true,
                &["low", "medium", "high", "xhigh", "max"],
            ),
            pm(
                "claude-haiku-4-5-20251001",
                200_000,
                64_000,
                true,
                true,
                &["disabled", "enabled"],
            ),
        ],
    },
    PresetProvider {
        name: "OpenAI",
        icon: "provider-icons/openai.png",
        icon_dark: None,
        base_url: "https://api.openai.com/v1",
        api_format: ApiFormat::OpenAiChat,
        key_url: "https://platform.openai.com/api-keys",
        models: &[
            pm(
                "gpt-6-astra",
                1_050_000,
                128_000,
                true,
                true,
                &["low", "medium", "high", "xhigh", "max"],
            ),
            pm(
                "gpt-5.6-sol",
                1_050_000,
                128_000,
                true,
                true,
                &["none", "low", "medium", "high", "xhigh", "max"],
            ),
        ],
    },
    PresetProvider {
        name: "xAI",
        icon: "provider-icons/xai.png",
        icon_dark: None,
        base_url: "https://api.x.ai/v1",
        api_format: ApiFormat::OpenAiChat,
        key_url: "https://console.x.ai",
        models: &[
            pm(
                "grok-4.6",
                500_000,
                128_000,
                true,
                true,
                &["low", "medium", "high", "xhigh"],
            ),
            pm(
                "grok-build-0.1",
                256_000,
                128_000,
                true,
                true,
                &["disabled", "enabled"],
            ),
        ],
    },
    PresetProvider {
        name: "OpenRouter",
        icon: "provider-icons/openrouter-light.svg",
        icon_dark: Some("provider-icons/openrouter-dark.svg"),
        base_url: "https://openrouter.ai/api",
        api_format: ApiFormat::AnthropicMessages,
        key_url: "https://openrouter.ai/keys",
        models: &[
            pm(
                "z-ai/glm-5.3",
                1_000_000,
                128_000,
                false,
                false,
                &["low", "high", "max"],
            ),
            pm(
                "anthropic/claude-fable-5.1",
                1_000_000,
                128_000,
                true,
                true,
                &["low", "medium", "high", "xhigh", "max"],
            ),
            pm(
                "openai/gpt-6-astra",
                1_050_000,
                128_000,
                true,
                true,
                &["low", "medium", "high", "xhigh", "max"],
            ),
            pm(
                "moonshotai/kimi-k3",
                1_048_576,
                131_072,
                true,
                false,
                &["low", "high", "max"],
            ),
            pm(
                "deepseek/deepseek-v4-pro",
                1_000_000,
                384_000,
                false,
                false,
                &["disabled", "low", "high", "max"],
            ),
            pm(
                "qwen/qwen3.8-max",
                1_000_000,
                131_072,
                true,
                true,
                &["low", "medium", "xhigh"],
            ),
        ],
    },
];

impl SettingsView {
    /// Create a provider from a preset: prefill name/endpoint/format/key page;
    /// models carry the preset's real context/output/vision/structured/reasoning
    /// values (the params mapping is generated per API format)
    pub(crate) fn add_provider_from_preset(
        &mut self,
        preset: &PresetProvider,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_provider_id();
        let ix = self.config.providers.len();
        self.config.providers.push(ProviderConfig {
            id,
            name: preset.name.into(),
            base_url: preset.base_url.into(),
            api_key: String::new(),
            api_format: preset.api_format,
            enabled: true,
            models: preset
                .models
                .iter()
                .map(|m| {
                    let mut model = ModelConfig::new(m.id, m.context, m.max_output);
                    model.input_image = m.input_image;
                    model.cap_structured = m.cap_structured;
                    let levels: Vec<String> = m.reasoning.iter().map(|s| s.to_string()).collect();
                    if !levels.is_empty() {
                        for (id, label) in reasoning_labels() {
                            if levels.iter().any(|l| l.as_str() == id) {
                                model.reasoning_labels.insert(id.into(), label.into());
                            }
                        }
                        // The helper returns a serde_json::Map; convert to HashMap
                        // per the field's type
                        model.reasoning_params =
                            default_reasoning_params(&levels, preset.api_format)
                                .into_iter()
                                .collect();
                        model.reasoning_levels = levels;
                    }
                    model
                })
                .collect(),
            key_url: Some(preset.key_url.into()),
        });
        self.selected = Some(ix);
        self.preset_picker_open = false;
        self.form_dirty = true;
        cx.emit(SettingsEvent::Save(self.config.clone()));
        cx.notify();
    }

    /// Preset picker dialog (modeled on ZCode's ProviderTemplatePicker: all
    /// presets tiled one card per cell plus a custom-endpoint entry in the first
    /// cell; clicking creates and closes)
    pub(crate) fn render_preset_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let card = |ix: usize| {
            let preset = &PRESET_PROVIDERS[ix];
            // Prefer the dedicated variant in dark themes (OpenRouter's
            // light/dark glyphs differ in contrast)
            let icon_path = preset
                .icon_dark
                .filter(|_| cx.theme().mode.is_dark())
                .unwrap_or(preset.icon);
            let badge = div()
                .size_8()
                .flex_none()
                .rounded_lg()
                // Square brand icons are uniformly clipped to the component's
                // corner radius; transparent-background glyphs are unaffected
                // either way
                .overflow_hidden()
                .child(img(icon_path).size_full());
            h_flex()
                .id(("preset-card", ix))
                .w(px(250.))
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .cursor_pointer()
                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let preset = &PRESET_PROVIDERS[ix];
                    this.add_provider_from_preset(preset, cx);
                }))
                .child(badge)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_medium().truncate().child(preset.name))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(preset.base_url),
                        ),
                )
                .child(
                    Icon::new(IconName::ChevronRight)
                        .size_4()
                        .text_color(cx.theme().muted_foreground),
                )
                .into_any_element()
        };
        let cards: Vec<AnyElement> = PRESET_PROVIDERS
            .iter()
            .enumerate()
            .map(|(ix, _)| card(ix))
            .collect();
        let custom_card = h_flex()
            .id("preset-custom")
            .w(px(250.))
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .border_dashed()
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.preset_picker_open = false;
                this.add_provider(cx);
            }))
            .child(
                div()
                    .size_8()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_lg()
                    .bg(cx.theme().accent)
                    .child(Icon::new(IconName::Plus).size_4()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(rust_i18n::t!("settings.models.custom_provider").to_string()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                rust_i18n::t!("settings.models.custom_provider_hint").to_string(),
                            ),
                    ),
            )
            .into_any_element();

        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("preset-picker")
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
                                div().text_lg().font_semibold().child(
                                    rust_i18n::t!("settings.models.add_provider").to_string(),
                                ),
                            )
                            .child(
                                div()
                                    .id("preset-picker-close")
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
                                        this.preset_picker_open = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    // Scrollable body: the cap sits on the scroll element itself
                    // and the right 16px lane keeps the overlay scrollbar off the
                    // cards (same layout as the edit dialogs)
                    .child(
                        div()
                            .relative()
                            .child(
                                v_flex()
                                    .id("preset-picker-body")
                                    .gap_4()
                                    .max_h(px(528.))
                                    .overflow_y_scroll()
                                    .track_scroll(&self.preset_picker_scroll)
                                    .pl_4()
                                    .pr(px(crate::code_view::CODE_SCROLLBAR_LANE))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(
                                                rust_i18n::t!("settings.models.picker_description")
                                                    .to_string(),
                                            ),
                                    )
                                    .child(
                                        h_flex()
                                            .flex_wrap()
                                            .gap_2()
                                            .child(custom_card)
                                            .children(cards),
                                    ),
                            )
                            .child(gpui_kit::base::Scrollbar::vertical(
                                &self.preset_picker_scroll,
                            )),
                    )
                    .child(
                        h_flex().p_4().pt_3().justify_end().child(
                            Button::new("cancel-preset-picker")
                                .outline()
                                .small()
                                .label(rust_i18n::t!("common.cancel"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.preset_picker_open = false;
                                    cx.notify();
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }
}
