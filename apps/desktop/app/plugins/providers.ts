import type {
  PluginPackageInfo,
  PluginProviderContribution,
  PluginRuntimeContributions,
  ProviderConfig,
} from "../protocol";

/**
 * The effective model-provider presets of enabled packages. This mirrors the
 * host's rules, so the picker and the backend resolve the same providers:
 * packages are ordered by id, a runtime provider replaces the manifest entry of
 * the same id, the first package wins a duplicate raw id, and the effective
 * provider id is namespaced so it can never shadow a user provider.
 */
export function pluginProviderConfigs(
  packages: readonly PluginPackageInfo[],
  runtime: PluginRuntimeContributions,
): ProviderConfig[] {
  const seen = new Set<string>();
  const providers: ProviderConfig[] = [];
  const ordered = [...packages]
    .filter((pkg) => pkg.enabled && !pkg.incompatible)
    // Byte order, matching the host's `BTreeMap` package order, so both sides
    // choose the same winner for a duplicate raw provider id.
    .sort((left, right) => (left.id < right.id ? -1 : left.id > right.id ? 1 : 0));
  for (const pkg of ordered) {
    const dynamic = runtime[`${pkg.id}:${pkg.digest}`]?.providers ?? [];
    const merged = new Map<string, PluginProviderContribution>();
    for (const provider of pkg.contributions.providers ?? []) merged.set(provider.id, provider);
    for (const provider of dynamic) merged.set(provider.id, provider);
    for (const provider of merged.values()) {
      if (seen.has(provider.id)) continue;
      seen.add(provider.id);
      providers.push({
        id: `plugin:${pkg.id}:${provider.id}`,
        name: provider.name,
        base_url: provider.base_url,
        api_format: provider.api_format,
        enabled: true,
        requires_api_key: Boolean(provider.credential),
        icon: provider.icon ?? null,
        key_url: null,
        models: provider.models.map((model) => ({
          id: model.id,
          name: model.name ?? null,
          enabled: true,
          context_window: model.context_window,
          max_output_tokens: model.max_output_tokens,
          input_image: model.input_image ?? false,
          input_pdf: model.input_pdf ?? false,
          cap_structured: model.cap_structured ?? false,
          cap_strict_tools: model.cap_strict_tools ?? false,
          cap_web_search: model.cap_web_search ?? false,
          reasoning_levels: model.reasoning_levels ?? [],
          default_reasoning_level: model.default_reasoning_level ?? null,
          reasoning_labels: model.reasoning_labels ?? {},
          reasoning_params: model.reasoning_params ?? {},
          parallel_tool_calls: model.parallel_tool_calls ?? true,
        })),
      });
    }
  }
  return providers;
}
