import { expect, open, test } from "nsh/test";

const zhSummary = "搜索了 2 个模式（1 失败） · 找了 1 次文件 · 运行了 1 条命令 · 网络搜索了 1 次（1 失败） · 思考了 1 次 · 执行了 1 次工具调用";
const enSummary = "Searched 2 patterns (1 failure) · Found files 1 time · Ran 1 command · Searched the web 1 time (1 failure) · Thought 1 time · Used 1 tool";

test("collapses consecutive activity and preserves expansion when tools append", async () => {
  const app = await open("./App.activity.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1200,
    height: 800,
  });
  expect(app.texts()).toContain(zhSummary);
  expect(app.shows("id:tool-grep-1")).toBe(false);
  expect(app.shows("id:tool-single")).toBe(true);

  await app.press("id:activity-r1");
  await app.waitFor("id:tool-grep-1");
  await app.press("id:append-tool");
  await app.waitFor("id:tool-read-2");
  expect(app.texts()).toContain(zhSummary.replace("执行了 1 次工具调用", "执行了 2 次工具调用"));

  await app.press("id:activity-r1");
  await app.waitUntil(() => !app.shows("id:tool-grep-1"));
  expect(app.shows("id:tool-single")).toBe(true);
});

test("renders the activity summary in English", async () => {
  const app = await open("./App.activity.fixture.tsx", {
    platform: "macos",
    locale: "en-US",
    width: 1200,
    height: 800,
  });
  expect(app.texts()).toContain(enSummary);
  expect(app.shows("id:tool-grep-1")).toBe(false);
});

test("reaches and expands every item in a large activity", async () => {
  const app = await open("./App.activity-large.fixture.tsx", {
    platform: "macos",
    width: 1200,
    height: 4000,
  });
  await app.waitFor("Final answer");
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-0");
  await app.waitFor("id:tool-tool-79");
  await app.press("id:tool-tool-79");
  await app.waitFor("id:wrap-tool-79");
});
