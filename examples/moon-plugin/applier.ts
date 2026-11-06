import { PluginError, type PluginContext } from "../../sdk/pig-plugin/mod";

interface MoonEchoArgs {
  text: string;
}

interface MoonEchoResult {
  echo: string;
  engine: "moon";
}

// The SDK owns the envelope protocol: instance, epoch, generation, per-stream
// sequences, request ids, stale-message filtering and lifecycle handshakes.
// This backend only states what it offers.
//
// Kept apart from `backend.ts` so tests can import the applier without the bare
// `pig/plugin` specifier (the `nsh test` work directory has no alias for it).
export const moonPlugin = (ctx: PluginContext) => {
  ctx.tools.register<MoonEchoArgs, MoonEchoResult>({
    name: "MoonEcho",
    description: "Returns the supplied text through the plugin backend",
    parameters: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
      additionalProperties: false,
    },
    outputSchema: {
      type: "object",
      properties: {
        echo: { type: "string" },
        engine: { type: "string" },
      },
      required: ["echo", "engine"],
      additionalProperties: false,
    },
    readOnly: true,
    idempotent: true,
    timeoutMs: 30_000,
    handler: ({ text }) => {
      if (typeof text !== "string") {
        throw new PluginError("invalid_input", "MoonEcho needs a text string");
      }
      return { echo: text, engine: "moon" };
    },
  });

  ctx.commands.register(
    {
      id: "moon-workspace",
      title: {
        en: "Open workspace with Moon",
        "zh-CN": "通过月亮打开工作区",
      },
      order: 10,
    },
    async () => {
      await ctx.services.request("navigation_open", { pageId: "main" });
    },
  );

  ctx.events.on("locale.changed", (event) => {
    ctx.log.info("locale.changed", { locale: String(event.locale ?? "") });
  });
};
