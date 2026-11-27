import { expect, open, test } from "nsh/test";

async function openStats(locale: "en" | "zh-CN" = "zh-CN") {
  return open("./App.stats.fixture.tsx", {
    platform: "macos",
    locale,
    width: 1200,
    height: 800,
    colorScheme: "dark",
  });
}

test("shows turn readings and the session status row", async () => {
  const app = await openStats();
  await app.waitFor("id:turn-speed-turn-stats");
  expect(app.texts()).toContain("133.3 tok/s");
  expect(app.texts()).toContain("600 tok");
  expect(app.texts()).toContain("1 回合 · 1 步 · 133 tok/s");
  expect(app.texts()).toContain("4.2K tok · 缓存命中 75.0%");
  expect(app.shows("id:session-status")).toBe(true);
  expect(app.shows("id:context-usage-ring")).toBe(true);
  expect(app.texts()).toContain("1%");
});

test("opens turn and session statistic details", async () => {
  const app = await openStats();
  await app.waitFor("id:turn-speed-turn-stats");

  await app.press("id:turn-speed-turn-stats");
  await app.waitFor("id:turn-speed-turn-stats-panel");
  expect(app.texts()).toContain("回合速度");
  expect(app.texts()).toContain("纯解码速度");
  expect(app.texts()).toContain("纯工具执行");
  expect(app.texts()).toContain("工具关键路径");
  expect(app.texts()).toContain("等待人工审批 / 提问");
  expect(app.texts()).toContain("审批审核");
  expect(app.texts()).toContain("上下文压缩");
  expect(app.texts()).toContain("其他开销");
  await app.press("id:turn-speed-turn-stats");
  await app.waitFor("id:turn-speed-turn-stats-panel", { state: "gone" });

  await app.press("id:turn-usage-turn-stats");
  await app.waitFor("id:turn-usage-turn-stats-panel");
  expect(app.texts()).toContain("回合 Token 用量");
  expect(app.texts()).toContain("缓存命中率");

  await app.press("id:session-activity");
  await app.waitFor("id:session-activity-panel");
  expect(app.texts()).toContain("加权解码速度");
  expect(app.texts()).toContain("纯工具执行累计");
  expect(app.texts()).toContain("工具关键路径累计");
  expect(app.texts()).toContain("人工等待累计");
  expect(app.texts()).toContain("审批审核累计");

  await app.press("id:session-usage");
  await app.waitFor("id:session-usage-panel");
  expect(app.texts()).toContain("会话 Token 用量");

  await app.press("id:context-usage");
  await app.waitFor("id:context-usage-panel");
  expect(app.texts()).toContain("上下文已使用 1%");
  expect(app.texts()).toContain("系统提示（估算）");
});


test("shows localized critical-path labels in English", async () => {
  const app = await openStats("en");
  await app.waitFor("id:turn-speed-turn-stats");
  await app.press("id:turn-speed-turn-stats");
  await app.waitFor("id:turn-speed-turn-stats-panel");
  expect(app.texts()).toContain("Tool execution");
  expect(app.texts()).toContain("Tool critical path");
  expect(app.texts()).toContain("User wait");
  expect(app.texts()).toContain("Approval review");
  expect(app.texts()).toContain("Compaction");
  expect(app.texts()).toContain("Other overhead");
});
