import { expect, open, test } from "nsh/test";
import {
  filterSlashCommands,
  modeForPermissions,
  oppositeDelivery,
  parseSlashCommand,
  permissionsForMode,
} from "./app/commands";
import { PluginSequencer, validatePluginEnvelope } from "./app/plugins/runtime";

function openApp() {
  return open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
}

test("parses slash commands and preserves the protocol permission shape", () => {
  expect(parseSlashCommand("/compact focus on tests")).toEqual({
    name: "/compact",
    instruction: "focus on tests",
  });
  expect(parseSlashCommand("/clearly")).toBeNull();
  expect(parseSlashCommand("hello /plan")).toBeNull();
  expect(filterSlashCommands("pl").map((command) => command.name)).toEqual(["/plan"]);
  expect(oppositeDelivery("queue")).toBe("steer");
  expect(oppositeDelivery("steer")).toBe("queue");

  const auto = permissionsForMode("auto");
  expect(auto).toEqual({
    sandbox: { mode: "workspace_write" },
    approval: "on_request",
    reviewer: "user",
  });
  expect(modeForPermissions(auto)).toBe("auto");
  expect(modeForPermissions(permissionsForMode("read-only"))).toBe("read-only");
  expect(modeForPermissions(permissionsForMode("full"))).toBe("full");
  expect(modeForPermissions({ ...auto, approval: "never" })).toBeNull();
});

test("answers every structured question and continues the turn", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "SCENARIO_Q");
  await app.press("id:send-button");
  await app.waitFor("id:question-card", { timeout: 20000 });
  expect(app.texts()).toContain("Choose an implementation approach");
  expect(app.count("id:composer-input")).toBe(0);

  await app.press("id:question-option-0-0");
  await app.press("id:question-next");
  expect(app.texts()).toContain("Should tests run?");
  await app.press("id:question-option-1-0");
  await app.press("id:question-next");
  await app.waitFor("MOCK_QUESTION_OK", { timeout: 20000 });
  expect(app.count("id:question-card")).toBe(0);
  expect(app.count("id:composer-input")).toBe(1);
});

test("selects model, reasoning, permissions and Plan through real controls", async () => {
  const app = await openApp();
  await app.press("id:model-picker");
  await app.type("id:model-search", "mock");
  await app.press("id:model-option-mock-mock-model");
  await app.press("id:model-picker");
  await app.press("id:reasoning-high");
  await app.keyboard.press("escape");

  await app.press("id:exec-mode");
  await app.press("id:exec-mode-read-only");
  expect(app.texts()).toContain("只读");
  await app.press("id:exec-mode");
  await app.press("id:plan-mode");
  await app.press("id:exec-mode");
  expect(app.count("id:plan-chip")).toBe(1);

  await app.type("id:composer-input", "/plan");
  await app.waitFor("id:slash-candidates");
  await app.keyboard.press("enter");
  expect(app.value("id:composer-input")).toBe("/plan ");
  await app.keyboard.press("enter");
  expect(app.value("id:composer-input")).toBe("");
  expect(app.count("id:plan-chip")).toBe(0);
});

test("requires confirmation before enabling full access", async () => {
  const app = await openApp();
  app.onDialog(() => "启用完全访问");
  await app.press("id:exec-mode");
  await app.press("id:exec-mode-full");
  expect(app.texts()).toContain("完全访问");
});

test("dispatches clear and compact instead of sending them as chat", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });

  await app.type("id:composer-input", "/clear");
  await app.keyboard.press("enter");
  expect(app.value("id:composer-input")).toBe("/clear ");
  await app.keyboard.press("enter");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { state: "gone" });
  expect(app.count("id:hero-cwd")).toBe(1);

  await app.type("id:composer-input", "/compact");
  await app.keyboard.press("enter");
  expect(app.value("id:composer-input")).toBe("/compact ");
  await app.type("id:composer-input", "focus on tests");
  await app.keyboard.press("enter");
  await app.waitFor("查看摘要", { timeout: 20000 });
  await app.press("查看摘要");
  expect(app.texts().some((text) => text.includes("MOCK_SUMMARY_OK"))).toBe(true);
  expect(app.value("id:composer-input")).toBe("");
});

test("supports plan review and returns revision feedback", async () => {
  const app = await openApp();
  await app.press("id:exec-mode");
  await app.press("id:plan-mode");
  await app.press("id:exec-mode");
  await app.type("id:composer-input", "PLAN_EXIT_SCENARIO");
  await app.press("id:send-button");
  await app.waitFor("id:approval-card", { timeout: 20000 });
  expect(app.texts()).toContain("是否按此计划开始实施？");
  await app.press("id:plan-revise");
  await app.type("id:plan-revision-input", "Add more validation steps");
  await app.press("id:plan-revision-submit");
  await app.waitFor("id:approval-card", { state: "gone", timeout: 20000 });
  expect(app.count("id:composer-input")).toBe(1);
});

test("queues, steers, reorders, edits and resumes messages while running", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "SLOW_BASH_SCENARIO");
  await app.press("id:send-button");
  await app.waitFor("id:stop-button", { timeout: 20000 });
  await app.type("id:composer-input", "Q_ONE");
  await app.keyboard.press("enter");
  await app.type("id:composer-input", "Q_TWO");
  await app.keyboard.press("enter");
  await app.waitFor("拖动排序 Q_TWO");
  expect(app.count("上移 Q_TWO")).toBe(0);
  await app.drag("拖动排序 Q_TWO", { x: 0, y: -38 });
  await app.waitUntil(() => {
    const texts = app.texts();
    return texts.indexOf("Q_TWO") < texts.indexOf("Q_ONE");
  });

  await app.press("编辑 Q_TWO");
  expect(app.value("id:composer-input")).toBe("Q_TWO");
  await app.press("取消排队 Q_ONE");
  await app.waitFor("Q_ONE", { state: "gone" });

  await app.type("id:composer-input", "Q_THREE");
  await app.keyboard.press("ctrl+enter");
  await app.waitFor("当前回合引导");
  await app.press("移到队列 Q_TWOQ_THREE");
  await app.waitFor("后续消息队列");

  await app.press("id:stop-button");
  await app.waitFor("id:queue-resume", { timeout: 20000 });
  expect(app.texts()).toContain("由于你中断了当前响应，队列已暂停");
  await app.press("id:queue-resume");
  await app.waitFor("id:queue-resume", { state: "gone", timeout: 20000 });
});

test("validates backend envelopes and independent stream sequences", () => {
  const sequences = new PluginSequencer();
  expect(sequences.accept("tools", 1)).toBe(true);
  expect(sequences.accept("services", 1)).toBe(true);
  expect(sequences.accept("tools", 2)).toBe(true);
  expect(sequences.accept("tools", 1)).toBe(false);
  expect(validatePluginEnvelope({
    protocol: 1,
    kind: "tools.result",
    instance: "moon",
    epoch: 1,
    stream: "tools",
    sequence: 3,
    generation: 1,
    body: { request_id: "request-1", result: { echo: "hello" } },
  })).toBe(true);
});
