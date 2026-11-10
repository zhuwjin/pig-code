import { expect, open, test } from "nsh/test";

test("merges the working row into the retry row and puts the action first", async () => {
  const app = await open("./App.retry-status.fixture.tsx", {
    platform: "windows",
    locale: "zh-CN",
    width: 760,
    height: 420,
    colorScheme: "dark",
  });
  await app.waitFor("id:retry-now");

  const texts = app.texts();
  expect(texts.some((text) => text.includes("正在工作"))).toBe(false);
  expect(texts.some((text) => text.includes("正在重试（2/10）"))).toBe(true);
  expect(texts.indexOf("立即重试")).toBeLessThan(texts.findIndex((text) => text.includes("正在重试")));
  expect(app.count("id:retry-now")).toBe(1);

  await app.press("id:toggle-retry");
  await app.waitUntil(() => app.texts().some((text) => text.includes("正在工作")));
  expect(app.shows("id:retry-now")).toBe(false);
});
