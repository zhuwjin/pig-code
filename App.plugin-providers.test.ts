import { expect, test } from "nsh/test";
import { pluginProviderConfigs } from "./app/plugins/providers";
import type { PluginPackageInfo, PluginRuntimeContributions } from "./app/protocol";

function packageInfo(
  id: string,
  digest: string,
  providers: PluginPackageInfo["contributions"]["providers"],
  overrides: Partial<PluginPackageInfo> = {},
): PluginPackageInfo {
  return {
    id,
    name: id,
    version: "1.0.0",
    digest,
    root: `/plugins/${id}`,
    source: "user",
    enabled: true,
    capabilities: [],
    contributions: {
      providers,
      credentials: [{ kind: "owned", id: "acme-key", label: "Acme key" }],
    },
    ...overrides,
  };
}

const acme = {
  id: "acme",
  name: "Acme",
  base_url: "https://acme.example/v1",
  api_format: "OpenAiChat" as const,
  credential: "acme-key",
  models: [
    {
      id: "acme-large",
      name: "Acme Large",
      context_window: 128_000,
      max_output_tokens: 8_192,
      reasoning_levels: ["low", "high"],
      default_reasoning_level: "high",
      cap_structured: true,
    },
  ],
};

test("plugin providers are namespaced and mapped to host model configs", () => {
  const providers = pluginProviderConfigs([packageInfo("demo", "sha256:a", [acme])], {});
  expect(providers).toHaveLength(1);
  const provider = providers[0];
  expect(provider.id).toBe("plugin:demo:acme");
  expect(provider.name).toBe("Acme");
  expect(provider.base_url).toBe("https://acme.example/v1");
  expect(provider.api_format).toBe("OpenAiChat");
  expect(provider.requires_api_key).toBe(true);
  expect(provider.models[0].id).toBe("acme-large");
  expect(provider.models[0].context_window).toBe(128_000);
  expect(provider.models[0].cap_structured).toBe(true);
  expect(provider.models[0].default_reasoning_level).toBe("high");
});

test("a runtime provider replaces the manifest entry of the same id", () => {
  const runtime: PluginRuntimeContributions = {
    "alpha:sha256:a": {
      providers: [{ ...acme, name: "Runtime Acme" }],
    },
  };
  const providers = pluginProviderConfigs(
    [packageInfo("alpha", "sha256:a", [{ ...acme, name: "Manifest Acme" }])],
    runtime,
  );
  expect(providers).toHaveLength(1);
  expect(providers[0].name).toBe("Runtime Acme");
});

test("the first package by id wins a duplicate provider id", () => {
  const providers = pluginProviderConfigs(
    [
      packageInfo("zeta", "sha256:z", [{ ...acme, id: "shared", name: "Zeta" }]),
      packageInfo("alpha", "sha256:a", [{ ...acme, id: "shared", name: "Alpha" }]),
    ],
    {},
  );
  expect(providers).toHaveLength(1);
  expect(providers[0].id).toBe("plugin:alpha:shared");
  expect(providers[0].name).toBe("Alpha");
});

test("disabled and incompatible packages contribute no providers", () => {
  const providers = pluginProviderConfigs(
    [
      packageInfo("disabled", "sha256:d", [acme], { enabled: false }),
      packageInfo("old", "sha256:o", [acme], { incompatible: "requires host 2.0.0" }),
    ],
    {},
  );
  expect(providers).toEqual([]);
});
