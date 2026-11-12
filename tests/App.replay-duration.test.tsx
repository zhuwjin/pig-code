import { expect, open, test } from "nsh/test";

test("restores real replay durations and leaves unknown durations unformatted", async () => {
  const app = await open("./App.replay-duration.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1000,
    height: 650,
    colorScheme: "dark",
  });
  await app.waitFor("id:work-replay-1");
  expect(app.texts()).toContain("工作了 33 秒");
  expect(app.texts()).toContain("处理完成");
  expect(app.texts()).not.toContain("工作了 1 秒");

  await app.press("id:work-replay-1");
  await app.waitFor("id:thinking-reason-1");
  expect(app.texts()).toContain("思考过程");
  expect(app.texts()).not.toContain("思考了 33 秒");

  await app.press("id:work-replay-2");
  await app.waitFor("id:thinking-reason-2");
  expect(app.texts()).toContain("思考过程");
});
