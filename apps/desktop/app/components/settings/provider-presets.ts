import { t } from "../../i18n";
import type { MessageKey } from "../../i18n";
import type { ApiFormat, ModelConfig, ProviderConfig } from "../../protocol";
import presetJson from "../../presets/providers.json";

export interface PresetModel {
  id: string;
  icon?: string;
  context: number;
  max_output: number;
  input_image?: boolean;
  input_pdf?: boolean;
  cap_structured?: boolean;
  reasoning?: string[];
}

export interface PresetProvider {
  name: string;
  icon: string;
  base_url: string;
  requires_api_key?: boolean;
  api_format: ApiFormat;
  key_url: string;
  models: PresetModel[];
}

export const PROVIDER_PRESETS = presetJson as PresetProvider[];

export const SORTED_PROVIDER_PRESETS = [...PROVIDER_PRESETS].sort((a, b) => a.name.localeCompare(b.name));

export function presetMatchesQuery(preset: PresetProvider, query: string): boolean {
  const normalized = query.trim().toLowerCase();
  return !normalized || preset.name.toLowerCase().includes(normalized) ||
    preset.base_url.toLowerCase().includes(normalized) ||
    preset.models.some((model) => model.id.toLowerCase().includes(normalized));
}

export function nextProviderId(providers: ProviderConfig[]): string {
  let index = 1;
  while (providers.some((provider) => provider.id === `custom-${index}`)) index += 1;
  return `custom-${index}`;
}

export function defaultReasoningParams(levels: string[], format: ApiFormat): Record<string, unknown> {
  return Object.fromEntries(levels.map((level) => {
    const off = level === "none" || level === "disabled";
    const effort = off ? "none" : level === "enabled" ? "high" : level;
    const params = format === "OpenAiChat"
      ? {
          thinking: { type: off ? "disabled" : "enabled" },
          enable_thinking: !off,
          reasoning_effort: effort,
          reasoning: { effort },
        }
      : format === "AnthropicMessages"
        ? off
          ? { thinking: { type: "disabled" } }
          : { thinking: { type: "enabled" }, output_config: { effort } }
        : off
          ? {}
          : { reasoning: { effort, summary: "auto" } };
    return [level, params];
  }));
}

const REASONING_LABELS: Record<string, MessageKey> = {
  none: "settings.models.reasoning.none",
  disabled: "settings.models.reasoning.none",
  enabled: "settings.models.reasoning.enabled",
  minimal: "settings.models.reasoning.minimal",
  low: "settings.models.reasoning.low",
  medium: "settings.models.reasoning.medium",
  high: "settings.models.reasoning.high",
  xhigh: "settings.models.reasoning.xhigh",
  max: "settings.models.reasoning.max",
};

export function providerFromPreset(preset: PresetProvider, id: string): ProviderConfig {
  return {
    id,
    name: preset.name,
    base_url: preset.base_url,
    requires_api_key: preset.requires_api_key ?? true,
    api_format: preset.api_format,
    enabled: true,
    icon: preset.icon,
    key_url: preset.key_url,
    models: preset.models.map((model): ModelConfig => {
      const reasoningLevels = model.reasoning ?? [];
      return {
        id: model.id,
        name: null,
        icon: model.icon ?? null,
        enabled: true,
        context_window: model.context,
        max_output_tokens: model.max_output,
        input_image: model.input_image ?? false,
        input_pdf: model.input_pdf ?? false,
        cap_structured: model.cap_structured ?? false,
        cap_strict_tools: false,
        cap_web_search: false,
        web_search_tool: null,
        reasoning_levels: reasoningLevels,
        default_reasoning_level: null,
        reasoning_labels: Object.fromEntries(reasoningLevels.map((level) => [level, REASONING_LABELS[level] ? t(REASONING_LABELS[level]) : level])),
        reasoning_params: defaultReasoningParams(reasoningLevels, preset.api_format),
        parallel_tool_calls: true,
      };
    }),
  };
}
