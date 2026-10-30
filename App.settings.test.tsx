import { expect, open, test } from "nsh/test";

function openApp() {
  return open("./App.selftest.tsx", {
    platform: "macos",
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
  await app.press("id:provider-test-mock");
  await app.waitFor("连接成功 · HTTP 200", { timeout: 20000 });
  await app.press("id:settings-appearance");
  await app.waitFor("id:theme-picker");
  await app.press("id:theme-picker-close");
  await app.press("id:settings-back");
  await app.waitFor("id:settings-page", { state: "gone" });
  expect(app.count("id:composer-input")).toBe(1);
});

test("creates, edits and deletes providers and models through real config", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.press("id:provider-add");
  await app.type("id:provider-name", "Mock Copy");
  expect(app.value("id:provider-id")).toBe("mock-copy");
  await app.press("id:provider-requires-key");
  await app.press("id:provider-save");
  await app.waitFor("id:model-add-mock-copy");

  await app.press("id:model-add-mock-copy");
  await app.type("id:model-id", "mock-copy-model");
  await app.type("id:model-name", "Mock Copy Model");
  await app.type("id:model-reasoning", "low, high");
  await app.type("id:model-default-reasoning", "low");
  await app.press("id:model-vision");
  await app.press("id:model-save");
  await app.waitFor("id:model-edit-mock-copy-mock-copy-model");
  expect(app.texts()).toContain("视觉");

  await app.press("id:model-default-mock-copy-mock-copy-model");
  await app.waitFor("id:model-default-mock-copy-mock-copy-model", { state: "gone" });
  await app.press("id:model-toggle-mock-copy-mock-copy-model");
  await app.press("id:model-edit-mock-copy-mock-copy-model");
  await app.type("id:model-name", " Edited");
  await app.press("id:model-save");
  await app.waitFor("Mock Copy Model Edited");

  await app.press("id:provider-edit-mock-copy");
  await app.type("id:provider-name", " Renamed");
  await app.press("id:provider-save");
  await app.waitFor("Mock Copy Renamed");

  app.onDialog(() => "删除");
  await app.press("id:model-delete-mock-copy-mock-copy-model");
  await app.waitFor("id:model-delete-mock-copy-mock-copy-model", { state: "gone" });
  await app.press("id:provider-delete-mock-copy");
  await app.waitFor("id:provider-select-mock-copy", { state: "gone" });
});

test("stores and clears provider keys without exposing them in the page", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.press("id:provider-edit-mock");
  await app.type("id:provider-api-key", "selftest-secret");
  await app.press("id:provider-save");
  await app.waitFor("密钥已保存", { timeout: 20000 });
  expect(app.texts()).not.toContain("selftest-secret");

  await app.press("id:provider-clear-key-mock");
  await app.waitFor("尚未配置密钥", { timeout: 20000 });
});
