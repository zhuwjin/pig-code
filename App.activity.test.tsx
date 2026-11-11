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

test("expands the last item in a large activity", async () => {
  const app = await open("./App.activity-large.fixture.tsx", {
    platform: "macos",
    width: 1200,
    height: 4000,
  });
  await app.waitFor("Final answer");
  await app.keyboard.press("mod+f");
  await app.type("id:thread-search-input", "Inspect activity");
  await app.keyboard.press("enter");
  await app.keyboard.press("escape");
  await app.waitFor("id:activity-tool-0");
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-998");
  await app.press("id:tool-tool-998");
  await app.waitFor("id:wrap-tool-998");
}, 180_000);

test("virtualizes a long expanded activity", async () => {
  const app = await open("./App.activity-large.fixture.tsx", {
    platform: "macos",
    width: 1200,
    height: 700,
  });
  await app.waitFor("Final answer");
  await app.keyboard.press("mod+f");
  await app.type("id:thread-search-input", "Inspect activity");
  await app.keyboard.press("enter");
  await app.keyboard.press("escape");
  await app.waitFor("id:activity-tool-0");
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-998");
  expect(app.count("id:message-list")).toBe(1);
  expect(app.count("id:tool-tool-0")).toBe(0);
}, 180_000);
