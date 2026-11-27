import { expect, open, test } from "nsh/test";
import { ICON_CATALOG } from "../app/components/settings/icon-catalog";
import { searchIcons } from "../app/components/settings/icon-search";

function openApp() {
  return open("./App.selftest.tsx", {
    network: "live",
    platform: "macos",
    locale: "zh-CN",
    width: 1440,
    height: 900,
  });
}

test("searches the full merged catalog with aliases", () => {
  expect(ICON_CATALOG.length).toBe(220);
  expect(searchIcons("").length).toBe(220);
  expect(searchIcons("深度求索").map((icon) => icon.id)).toContain("deepseek");
  expect(searchIcons("智谱").map((icon) => icon.id)).toContain("zhipu");
  expect(searchIcons("claude").map((icon) => icon.id)).toContain("anthropic");
  expect(searchIcons("openrouter").map((icon) => icon.id)).toContain("openrouter");
  expect(searchIcons("no-such-icon").length).toBe(0);
});

test("selects, cancels and clears provider icons from the title row", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.waitFor("id:provider-icon-picker-mock", { timeout: 10000 });
  await app.press("id:provider-icon-picker-mock");
  await app.waitFor("id:icon-picker");
  await app.type("id:icon-search", "深度求索");
  await app.press("id:icon-option-deepseek");
  await app.waitFor("id:icon-picker", { state: "gone" });
  await app.waitFor("id:provider-icon-deepseek", { timeout: 20000 });
  expect(app.count("id:provider-editor")).toBe(0);

  await app.press("id:provider-icon-picker-mock");
  await app.type("id:icon-search", "openai");
  await app.press("id:cancel-icon-picker");
  await app.waitFor("id:icon-picker", { state: "gone" });
  expect(app.count("id:provider-icon-deepseek")).toBeGreaterThan(0);
  expect(app.count("id:provider-icon-openai")).toBe(0);

  await app.press("id:provider-icon-picker-mock");
  await app.press("id:icon-option-default");
  await app.waitFor("id:icon-picker", { state: "gone" });
  expect(app.count("id:provider-icon-deepseek")).toBe(0);
});

test("uses the same catalog and independent icon on model rows", async () => {
  const app = await openApp();
  await app.press("id:settings-open");
  await app.waitFor("id:model-icon-picker-mock-mock-model", { timeout: 10000 });
  await app.press("id:model-icon-picker-mock-mock-model");
  await app.waitFor("id:icon-picker");
  await app.type("id:icon-search", "openrouter");
  await app.press("id:icon-option-openrouter");
  await app.waitFor("id:icon-picker", { state: "gone" });
  await app.waitFor("id:provider-icon-openrouter", { timeout: 20000 });
  expect(app.count("id:model-editor")).toBe(0);

  await app.press("id:model-icon-picker-mock-mock-model");
  await app.waitFor("id:icon-picker");
  await app.press("id:icon-option-default");
  await app.waitFor("id:icon-picker", { state: "gone" });
  expect(app.count("id:provider-icon-openrouter")).toBe(0);
});
