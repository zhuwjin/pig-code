import { expect, open, test } from "nsh/test";

function openApp() {
  return open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1440,
    height: 900,
  });
}

test("opens model settings, lists models and tests the connection", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.waitFor("id:settings-page");
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
  await app.type("id:model-reasoning", "low");
  await app.press("id:add-level");
  await app.type("id:model-reasoning", "high");
  await app.press("id:add-level");
  await app.press("id:default-level-low");
  await app.press("id:model-vision");
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
  await app.press("id:provider-edit-mock");
  await app.press("id:provider-toggle-mask");
  await app.press("id:provider-toggle-mask");
  await app.type("id:provider-api-key", "selftest-secret");
  await app.waitUntil(() => app.value("id:provider-api-key") === "", { timeout: 20000 });
  expect(app.texts()).not.toContain("selftest-secret");

  await app.press("id:provider-clear-key-mock");
  await app.waitUntil(() => !app.enabled("id:provider-clear-key-mock"), { timeout: 20000 });
});
