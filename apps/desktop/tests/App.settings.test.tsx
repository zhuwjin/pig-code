import { expect, open, test } from "nsh/test";

async function openApp() {
  const app = await open("./App.selftest.tsx", {
    network: "live",
    platform: "macos",
    locale: "zh-CN",
    width: 1440,
    height: 900,
  });
  await app.waitFor("text:mock-model", { timeout: 10000 });
  return app;
}

test("opens model settings, lists models and tests the connection", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.waitFor("id:settings-page");
  await app.waitFor("id:provider-edit-mock", { timeout: 10000 });
  expect(app.texts()).toContain("模型设置");
  expect(app.texts()).toContain("Mock");
  expect(app.texts()).toContain("mock-model");

  await app.press("id:models-tab");
  expect(app.texts()).toContain("mock-model");
  await app.press("id:providers-tab");
  await app.press("id:provider-edit-mock");
  await app.hover("id:provider-format");
  await app.press("id:provider-format");
  await app.press("id:provider-format-OpenAiChat");
  await app.press("id:provider-test-mock");
  await app.waitFor("连接成功 · HTTP 200", { timeout: 20000 });
  expect(app.spanStyle("text:连接成功 · HTTP 200").color).toBe("#16a34aff");
  await app.press("id:settings-appearance");
  await app.waitFor("id:theme-picker");
  await app.press("id:settings-back");
  await app.waitFor("id:settings-page", { state: "gone" });
  expect(app.count("id:composer-input")).toBe(1);
});

test("uses dedicated pages for models, appearance and plugin management", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  expect(app.count("id:settings-navigation")).toBe(1);
  expect(app.count("id:new-task")).toBe(0);
  expect(app.count("id:open-search")).toBe(0);
  expect(app.count("id:theme-picker")).toBe(0);

  await app.press("id:settings-appearance");
  await app.waitFor("id:theme-picker");
  expect(app.count("id:plugin-package-path")).toBe(0);

  await app.press("id:settings-plugins");
  await app.waitFor("id:plugin-manager-page");
  expect(app.count("id:theme-picker")).toBe(0);
  expect(app.count("id:plugin-package-path")).toBe(1);
  expect(app.texts()).toContain("安装签名插件");

  await app.press("id:settings-models");
  expect(app.count("id:plugin-manager-page")).toBe(0);
  expect(app.texts()).toContain("模型设置");
  await app.press("id:settings-back");
  await app.waitFor("id:settings-page", { state: "gone" });
});

test("toggles the developer resource HUD", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.waitFor("id:settings-navigation");
  await app.waitFor("id:settings-developer");
  await app.press("id:settings-developer");
  await app.waitFor("id:developer-settings");
  expect(app.texts()).toContain("开发者选项");
  expect(app.texts()).toContain("显示性能 HUD");
  expect(app.count("id:fps-meter")).toBe(0);

  await app.waitUntil(() => app.enabled("id:developer-show-fps"), { timeout: 10000 });
  await app.press("id:developer-show-fps");
  await app.waitFor("id:fps-meter", { timeout: 10000 });
  for (const text of ["CPU", "MEM"]) {
    expect(app.texts()).toContain(text);
  }
  for (const text of ["FPS", "INTERVAL", "P95", "DROP"]) {
    expect(app.texts()).not.toContain(text);
  }
  expect(app.count("id:fps-meter-fps")).toBe(0);
  expect(app.count("id:fps-meter-trace")).toBe(0);
  await app.waitUntil(
    () => app.texts().some((text) => /^\d+ MB$/.test(text)),
    { timeout: 5000 },
  );
  await app.waitUntil(
    () => app.texts().some((text) => /^\d+%$/.test(text)),
    { timeout: 5000 },
  );

  await app.press("id:developer-show-fps");
  await app.waitFor("id:fps-meter", { state: "gone" });
});

test("toggles the OS sandbox switch in developer settings", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.waitFor("id:settings-navigation");
  await app.waitFor("id:settings-developer");
  await app.press("id:settings-developer");
  await app.waitFor("id:developer-settings");
  expect(app.texts()).toContain("启用系统沙盒");

  await app.waitUntil(() => app.enabled("id:developer-sandbox-enabled"), { timeout: 10000 });
  await app.press("id:developer-sandbox-enabled");
  await app.waitUntil(
    () => app.texts().includes("已显式关闭系统沙盒——命令将不经操作系统沙盒隔离运行。"),
    { timeout: 10000 },
  );

  await app.press("id:developer-sandbox-enabled");
  await app.waitUntil(
    () => !app.texts().includes("已显式关闭系统沙盒——命令将不经操作系统沙盒隔离运行。"),
    { timeout: 10000 },
  );
});

test("creates, edits and deletes providers and models through real config", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.press("id:provider-add");
  await app.press("id:preset-custom");
  await app.waitFor("id:model-add-custom-1");
  if (app.count("id:provider-name") === 0) await app.press("id:provider-edit-custom-1");
  await app.clear("id:provider-name");
  await app.type("id:provider-name", "Mock Copy");
  await app.press("id:provider-requires-key");

  await app.press("id:model-add-custom-1");
  await app.type("id:model-id", "mock-copy-model");
  await app.type("id:model-name", "Mock Copy Model");
  await app.press("id:advanced-toggle");
  for (const id of ["model-vision", "model-pdf", "cap-structured", "cap-strict-tools"]) {
    expect(app.shows(`id:${id}-check`)).toBe(false);
    await app.press(`id:${id}`);
    expect(app.shows(`id:${id}-check`)).toBe(true);
  }
  await app.press("id:model-vision");
  expect(app.shows("id:model-vision-check")).toBe(false);
  await app.press("id:model-vision");
  expect(app.shows("id:model-vision-check")).toBe(true);
  await app.type("id:model-reasoning", "low");
  await app.press("id:add-level");
  await app.type("id:model-reasoning", "high");
  await app.press("id:add-level");
  await app.press("id:default-level-low");
  expect(app.value("id:model-id")).toBe("mock-copy-model");
  await app.press("id:model-save");
  await app.waitFor("id:model-edit-custom-1-mock-copy-model");
  expect(app.texts()).toContain("视觉");

  await app.press("id:model-toggle-custom-1-mock-copy-model");
  await app.press("id:model-edit-custom-1-mock-copy-model");
  await app.type("id:model-name", " Edited");
  await app.press("id:model-save");
  await app.waitFor("Mock Copy Model Edited");

  await app.type("id:provider-name", " Renamed");
  await app.waitFor("Mock Copy Renamed");

  await app.press("id:model-delete-custom-1-mock-copy-model");
  await app.waitFor("id:model-delete-custom-1-mock-copy-model", { state: "gone" });
  await app.press("id:provider-delete-custom-1");
  await app.waitFor("确认删除");
  await app.type("id:provider-name", " Stable");
  await app.waitFor("Mock Copy Renamed Stable");
  await app.waitFor("确认删除");
  await app.press("id:provider-delete-custom-1");
  await app.waitFor("id:provider-select-custom-1", { state: "gone" });
});

test("adds a preset provider and maintains models in the fleet table", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.press("id:provider-add");
  await app.waitFor("id:preset-picker");
  await app.type("id:preset-search", "DeepSeek");
  await app.press("id:preset-card-0");
  await app.waitFor("id:model-edit-custom-1-deepseek-flash");
  expect(app.texts()).toContain("DeepSeek");

  await app.press("id:models-tab");
  expect(app.texts()).toContain("上下文");
  expect(app.texts()).toContain("输出");
  expect(app.texts()).toContain("能力");
  expect(app.texts()).toContain("推理等级");
  expect(app.texts()).toContain("deepseek-v4-flash");
  await app.press("id:fleet-provider-settings-custom-1");
  await app.waitFor("id:model-edit-custom-1-deepseek-flash");
});

test("stores and clears provider keys without exposing them in the page", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.waitFor("id:provider-edit-mock", { timeout: 10000 });
  await app.press("id:provider-edit-mock");
  await app.press("id:provider-toggle-mask");
  await app.press("id:provider-toggle-mask");
  await app.type("id:provider-api-key", "selftest-secret");
  await app.wait(600);
  await app.waitUntil(() => app.value("id:provider-api-key") === "", { timeout: 20000 });
  expect(app.texts()).not.toContain("selftest-secret");

  await app.press("id:provider-clear-key-mock");
  await app.waitUntil(() => !app.enabled("id:provider-clear-key-mock"), { timeout: 20000 });
});


test("opens remote control settings with account login and device-code pairing", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.waitFor("id:settings-remote");
  await app.press("id:settings-remote");
  await app.waitFor("id:remote-settings");
  expect(app.texts()).toContain("远程控制与账号");
  expect(app.texts()).toContain("设备码配对登录");
  expect(app.count("id:remote-auth-card")).toBe(1);
  expect(app.count("id:remote-device-code-card")).toBe(1);
  expect(app.value("id:remote-server-url")).toBe("http://127.0.0.1:8787");
  expect(app.enabled("id:remote-sign-in")).toBe(false);
  expect(app.enabled("id:remote-start-device-code")).toBe(true);

  await app.type("id:remote-username", "alice");
  await app.type("id:remote-password", "super-secret-pass");
  expect(app.enabled("id:remote-sign-in")).toBe(true);
  expect(app.enabled("id:remote-register")).toBe(true);
});
