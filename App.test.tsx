import { expect, open, test } from "nsh/test";

function openApp() {
  return open("./App.selftest.tsx", {
    platform: "macos",
    width: 1280,
    height: 800,
  });
}

test("creates, streams and replays a session", async () => {
  const app = await openApp();
  expect(app.texts()).toContain("Pig Code");
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  expect(app.texts()).toContain("Read");
  expect(app.value("id:composer-input")).toBe("");

  await app.press("id:new-task");
  await app.press("auto-title selftest");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  expect(app.count("Conclusion: MOCK_REPLY_OK")).toBe(1);
});

test("answers dangerous command approvals", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "DANGER_SCENARIO");
  await app.press("id:send-button");
  await app.waitFor("需要批准：ExecCommand", { timeout: 20000 });
  await app.press("允许");
  await app.waitFor("需要批准：ExecCommand", { timeout: 20000 });
  await app.press("允许");
  await app.waitFor("MOCK_DANGER_DONE", { timeout: 20000 });
  expect(app.texts()).toContain("MOCK_DANGER_DONE");
});

test("switches appearance and folds the sidebar", async () => {
  const app = await openApp();
  expect(app.count("id:appearance-dark")).toBe(1);
  await app.press("id:appearance-dark");
  expect(app.count("id:appearance-light")).toBe(1);
  await app.press("id:sidebar-toggle");
  expect(app.count("id:new-task")).toBe(0);
  await app.press("id:sidebar-toggle");
  expect(app.count("id:new-task")).toBe(1);
});
