import { expect, open, test } from "nsh/test";
import { remoteDocumentRejection } from "./app/plugins/ui-protocol";
import { uiPlan } from "./app/plugins/ui-coordinator";
import type { PluginPackageInfo, PluginRuntimeState, RemoteDocument } from "./app/protocol";
import { pluginInstance } from "./app/plugins/ui-coordinator";

function statsPackage(): PluginPackageInfo {
  const targets = ["message.turnStats", "status.activity", "status.usage", "status.context"];
  return {
    id: "com.pigcodets.plugins.tokenstats",
    name: "Token Stats",
    version: "1.0.0",
    digest: "sha256:stats",
    root: "/plugins/stats",
    source: "user",
    enabled: true,
    capabilities: [
      { name: "ui.patch.structure", targets },
      { name: "session.read", targets: ["*"] },
    ],
    contributions: {
      ui: targets.map((target, index) => ({
        id: `stats-${index}`,
        kind: "replace" as const,
        target,
        export: `Stats:${index}`,
        preserve: [],
      })),
    },
  };
}

test("accepts remote popover documents", () => {
  const pkg = statsPackage();
  const instance = pluginInstance(pkg);
  const document: RemoteDocument = {
    instance,
    epoch: 1,
    generation: 1,
    sequence: 1,
    baseRevision: 0,
    nodes: [{
      id: "stats",
      kind: "popover",
      text: null,
      exportName: "SessionActivity",
      testId: null,
      press: false,
      children: [],
      style: {},
      events: ["openChange"],
      props: { open: false, placement: "top", align: "center" },
      accessibility: {},
      source: null,
    }],
    roots: ["stats"],
    warnings: [],
  };
  expect(remoteDocumentRejection(document, {
    protocol: 1,
    kind: "ui.commit",
    instance,
    epoch: 1,
    stream: "ui",
    sequence: 1,
    generation: 1,
    body: {},
  })).toBe(null);
});

test("exposes turn and session statistic mounts to plugins", () => {
  const pkg = statsPackage();
  const runtime = {
    [pluginInstance(pkg)]: { state: "active" } as PluginRuntimeState,
  };
  for (const target of ["message.turnStats", "status.activity", "status.usage", "status.context"]) {
    const plan = uiPlan([pkg], target, undefined, runtime);
    expect(plan.entries.length).toBe(1);
    expect(plan.rejected).toEqual([]);
  }
});

test("shows no native turn or session statistics without the plugin", async () => {
  const app = await open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  expect(app.count("id:stats-summary-TurnStats:0:speed")).toBe(0);
  expect(app.count("id:stats-summary-TurnStats:0:usage")).toBe(0);
  expect(app.count("id:stats-summary-SessionActivity")).toBe(0);
  expect(app.count("id:stats-summary-SessionUsage")).toBe(0);
  expect(app.count("id:stats-summary-SessionContext")).toBe(0);
});
