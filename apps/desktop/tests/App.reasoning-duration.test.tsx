import { expect, open, test } from "nsh/test";

test("tracks each reasoning segment separately from total turn duration", async () => {
  const app = await open("./App.reasoning-duration.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1000,
    height: 650,
    colorScheme: "dark",
    clock: { time: 0 },
  });
  await app.waitFor("id:thinking-ticker-reason-1");
  await app.clock!.runFor(2200);
  await app.press("id:first-tool");
  await app.press("id:second-reasoning");
  await app.waitFor("id:thinking-ticker-reason-2");
  await app.clock!.runFor(1400);
  await app.press("id:answer");
  await app.press("id:complete");
  await app.waitFor("id:thinking-reason-1");
  expect(app.texts()).toContain("工作了 5 秒");
  expect(app.texts().filter((text) => /^思考了 \d+ 秒$/.test(text))).toHaveLength(2);
  expect(app.texts()).not.toContain("思考了 5 秒");
});
