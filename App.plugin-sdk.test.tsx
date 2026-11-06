import { expect, test } from "nsh/test";
import { moonPlugin } from "./examples/moon-plugin/applier";
import { createTestHost, PluginError, type PluginEnvelope } from "./sdk/pig-plugin/mod";

/** What `check` rejects with, or a failure when it resolves. */
async function rejected(check: () => Promise<unknown>): Promise<unknown> {
  try {
    await check();
  } catch (error) {
    return error;
  }
  throw new Error("the promise resolved instead of rejecting");
}

test("the SDK serves a real backend's tools, commands and events", async () => {
  const host = await createTestHost({ applier: moonPlugin }).start();
  expect(host.ready).toBe(true);
  expect(host.envelopes[0].kind).toBe("lifecycle.hello");
  expect(host.tools()).toContain("MoonEcho");
  expect(host.commands()).toContain("moon-workspace");

  const echoed = await host.tool<{ echo: string; engine: string }>("MoonEcho", { text: "hi" });
  expect(echoed).toEqual({ echo: "hi", engine: "moon" });

  await host.command("moon-workspace");
  const navigation = host.serviceRequests.find((request) => request.service === "navigation_open");
  expect(navigation).toBeDefined();
  expect(navigation?.payload).toEqual({ pageId: "main" });
  host.answerService(navigation!.requestId, null);

  await host.emitEvent("locale.changed", { locale: "en" });
  await host.health();

  const tools = host.envelopes
    .filter((envelope) => envelope.stream === "tools")
    .map((envelope) => envelope.sequence);
  expect(tools.length).toBeGreaterThan(0);
  expect(tools).toEqual([...tools].sort((left, right) => left - right));
  expect(new Set(tools).size).toBe(tools.length);
  for (const envelope of host.envelopes) {
    expect(envelope.instance).toBe(host.identity.instance);
    expect(envelope.epoch).toBe(host.identity.epoch);
    expect(envelope.generation).toBe(host.identity.generation);
  }

  host.stop();
  expect(host.disposed()).toBe(true);
  expect(host.errors).toEqual([]);
});

test("handler failures cross as structured errors", async () => {
  const host = await createTestHost({
    applier: (ctx) => {
      ctx.tools.register({
        name: "Failing",
        description: "Always fails",
        parameters: { type: "object" },
        handler: () => {
          throw new PluginError("invalid_input", "bad arguments");
        },
      });
      ctx.tools.register({
        name: "PlainFailure",
        description: "Throws a plain error",
        parameters: { type: "object" },
        handler: () => {
          throw new Error("plain");
        },
      });
    },
  }).start();

  const structured = (await rejected(() => host.tool("Failing"))) as PluginError;
  expect(structured.message).toBe("bad arguments");
  expect(structured.code).toBe("invalid_input");

  const plain = (await rejected(() => host.tool("PlainFailure"))) as PluginError;
  expect(plain.code).toBe("internal");
  expect(plain.message).toBe("plain");

  const missing = (await rejected(() => host.tool("Missing"))) as PluginError;
  expect(missing.code).toBe("not_found");

  host.stop();
});

test("session projections and hook contributions flow through the test host", async () => {
  const host = await createTestHost({
    applier: (ctx) => {
      ctx.hooks.register({ event: "post_tool_use", command: "audit.sh" });
      ctx.events.on("session.check", async () => {
        const session = await ctx.services.session.read<{ id: string }>();
        ctx.log.info("session.read", { id: session?.id ?? "" });
      });
    },
  }).start();

  host.setSession({ id: "s-42", cwd: "/workspace", stats: {}, turns: [] });
  await host.emitEvent("session.check");
  const request = host.serviceRequests.find((entry) => entry.service === "session_read");
  expect(request).toBeDefined();
  const log = host.envelopes.find(
    (envelope) => envelope.kind === "log.event" && (envelope.body as { message?: string }).message === "session.read",
  );
  expect(log).toBeDefined();
  expect((log?.body as { id?: string }).id).toBe("s-42");
  expect(host.hooks()).toHaveLength(1);
  host.stop();
});

test("stale envelopes are ignored and hook contributions are collected", async () => {
  const host = await createTestHost({
    applier: (ctx) => {
      ctx.hooks.register({
        event: "pre_tool_use",
        command: "guard.sh",
        matcher: "Write|Edit",
        timeoutMs: 5000,
      });
      ctx.tools.register({
        name: "Dynamic",
        description: "Dynamically selectable",
        availability: "plugin",
        parameters: { type: "object" },
        handler: () => "dynamic",
      });
    },
  }).start();

  expect(host.hooks()).toHaveLength(1);
  expect(host.hooks()[0].event).toBe("pre_tool_use");
  expect(host.hooks()[0].command).toBe("guard.sh");
  expect(await host.select()).toEqual(["Dynamic"]);

  // A stop from another generation must not dispose this instance.
  const stale: PluginEnvelope = {
    protocol: 1,
    kind: "lifecycle.stop",
    instance: host.identity.instance,
    epoch: host.identity.epoch,
    stream: "lifecycle",
    sequence: 999,
    generation: host.identity.generation + 1,
    body: null,
  };
  host.sendRaw(stale);
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(host.disposed()).toBe(false);

  const foreign: PluginEnvelope = { ...stale, epoch: host.identity.epoch + 1, generation: host.identity.generation };
  host.sendRaw(foreign);
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(host.disposed()).toBe(false);

  host.stop();
  expect(host.disposed()).toBe(true);
});
