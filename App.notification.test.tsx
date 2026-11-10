import { expect, open, test } from "nsh/test";

test("renders background notifications as a source label and a command bubble", async () => {
  const app = await open("./App.notification.fixture.tsx", {
    platform: "windows",
    locale: "zh-CN",
    width: 900,
    height: 620,
    colorScheme: "dark",
  });
  await app.waitFor("id:notification-note-shell");

  const texts = app.texts();
  expect(texts).toContain("由后台发送（Shell）");
  expect(texts).toContain("由后台发送（Agent）");
  expect(texts).toContain("git status --short | Select-Object -First 3");
  expect(texts).toContain("Sleep two minutes");
  expect(texts).toContain("完成 · 耗时 2 分 9 秒");
  expect(texts).toContain("失败 · 耗时 8 秒");

  await app.press("id:notification-note-shell");
  await app.waitUntil(() => app.texts().includes("task"));
});
