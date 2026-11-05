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
        kind: "surface" as const,
        target,
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

function openStatsApp() {
  return open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
}

async function realStatsPlatform(app: Awaited<ReturnType<typeof openStatsApp>>): Promise<boolean> {
  return (await app.screenshot().catch(() => null)) !== null;
}

test("renders token statistics in surfaces and dismisses their overlays", async () => {
  const app = await openStatsApp();
  if (!await realStatsPlatform(app)) return;
  await app.waitFor("id:composer-input");
  await app.type("id:composer-input", "read the mock file");
  await app.waitUntil(() => app.enabled("id:send-button"), { timeout: 10000 });
  await app.press("id:send-button");
  await app.waitFor("text:/read the mock file/", { timeout: 10000 });
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.press("id:settings-open");
  await app.wait(1000);
  await app.waitFor("id:settings-plugins");
  await app.press("text:插件管理");
  await app.waitFor("id:plugin-package-path");
  await app.type("id:plugin-package-path", "target/test-tmp/token-stats-plugin/token-stats.nshpkg");
  await app.type("id:plugin-root-path", "target/test-tmp/token-stats-plugin/root.pub");
  await app.press("id:plugin-install");
  await app.waitFor("id:theme-package-com.pigcodets.plugins.tokenstats", { timeout: 10000 });
  await app.press("id:theme-package-com.pigcodets.plugins.tokenstats");
  await app.waitFor("text:active", { timeout: 10000 });
  await app.press("id:settings-back");
  expect(app.count("id:stats-summary-SessionActivity")).toBe(1);
  expect(app.count("id:stats-summary-SessionUsage")).toBe(1);
  expect(app.count("id:stats-summary-SessionContext")).toBe(1);
  expect(app.count("id:stats-summary-TurnStats:0")).toBe(1);
  await app.wait(1000);
  await app.screenshot("target/test-tmp/token-stats-summary.png");

  await app.press("id:stats-summary-SessionActivity");
  await app.waitFor("id:plugin-overlay", { timeout: 10000 });
  await app.screenshot("target/test-tmp/token-stats-activity.png");
  await app.keyboard.press("escape");
  await app.waitFor("id:plugin-overlay", { state: "gone" });
  await app.press("id:stats-summary-SessionActivity");
  await app.waitFor("id:plugin-overlay", { timeout: 10000 });
  await app.press("id:settings-open");
  await app.waitFor("id:plugin-overlay", { state: "gone" });
  await app.press("id:settings-back");
  await app.press("label:统计信息 >> nth=0");
  await app.waitFor("id:plugin-overlay", { timeout: 10000 });
  await app.press("label:统计信息 >> nth=0");
  await app.waitFor("id:plugin-overlay", { state: "gone" });

  await app.press("id:stats-summary-SessionUsage");
  await app.waitFor("id:plugin-overlay", { timeout: 10000 });
  await app.screenshot("target/test-tmp/token-stats-usage.png");
  await app.keyboard.press("escape");
  await app.waitFor("id:plugin-overlay", { state: "gone" });

  await app.press("id:stats-summary-TurnStats:0");
  await app.waitFor("id:plugin-overlay", { timeout: 10000 });
  await app.screenshot("target/test-tmp/token-stats-turn.png");
  await app.keyboard.press("escape");
  await app.waitFor("id:plugin-overlay", { state: "gone" });

  await app.press("id:settings-open");
  await app.press("text:插件管理");
  await app.press("id:theme-package-com.pigcodets.plugins.tokenstats");
  await app.waitFor("id:stats-summary-SessionActivity", { state: "gone" });
  await app.waitFor("id:stats-summary-TurnStats:0", { state: "gone" });
}, 60000);
