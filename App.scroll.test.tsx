import { expect, open, test } from "nsh/test";

test("opens long history at the latest, preserves an upward position and resumes following", async () => {
  const app = await open("./App.scroll.fixture.tsx", {
    platform: "macos",
    width: 900,
    height: 700,
  });
  await app.waitFor("Message 249");
  expect(app.shows("Message 1")).toBe(false);
  expect(app.texts().filter((text) => text.startsWith("Message ")).length).toBeLessThan(40);

  await app.scroll("id:message-list", -10);
  await app.waitFor("id:latest-messages");

  await app.scroll("id:message-list", -1000);
  await app.keyboard.press("mod+f");
  await app.type("id:thread-search-input", "User message 0");
  await app.keyboard.press("enter");
  await app.keyboard.press("escape");
  await app.waitFor("Message 1");
  expect(app.shows("Message 249")).toBe(false);

  await app.waitFor("id:latest-messages", { timeout: 10000 });
  expect(app.shows("Message 250")).toBe(false);

  await app.press("id:latest-messages");
  await app.waitFor("Message 250");
  await app.waitUntil(() => !app.shows("id:latest-messages"));

  await app.waitFor("Message 251");
  expect(app.shows("id:latest-messages")).toBe(false);
});

test("preserves an upward position when expanding work inside the follow threshold", async () => {
  const app = await open("./App.scroll-expand.fixture.tsx", {
    platform: "macos",
    width: 900,
    height: 700,
  });
  await app.waitFor("Final answer");
  await app.scroll("id:message-list", -10);
  await app.waitFor("id:latest-messages");
  expect(app.shows("id:work-expand-turn")).toBe(true);

  await app.press("id:work-expand-turn");
  await app.waitFor("id:activity-expand-reasoning");
  await app.wait(500);
  expect(app.shows("id:latest-messages")).toBe(true);

  await app.press("id:latest-messages");
  await app.waitUntil(() => !app.shows("id:latest-messages"));
});

test("keeps visible rows anchored when expanding and collapsing at the bottom", async () => {
  const app = await open("./App.scroll-expand.fixture.tsx", {
    platform: "macos",
    width: 900,
    height: 700,
  });
  await app.waitFor("Final answer");
  expect(app.shows("id:latest-messages")).toBe(false);
  expect(app.shows("History answer 7")).toBe(true);

  await app.press("id:work-expand-turn");
  await app.wait(500);
  expect(app.shows("History answer 7")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(true);

  await app.press("id:latest-messages");
  await app.wait(500);
  expect(app.shows("id:latest-messages")).toBe(false);
  await app.press("id:work-expand-turn");
  await app.wait(500);
  expect(app.shows("History answer 7")).toBe(true);

  const report = app.texts().find((text) => text.startsWith("SCROLL_LOG"));
  expect(report).toBeDefined();
  const samples = JSON.parse(report!.slice("SCROLL_LOG".length)) as { offset: number; max: number; content: number }[];
  expect(samples.length).toBeGreaterThan(5);
  expect(new Set(samples.map((sample) => sample.content)).size).toBeGreaterThan(3);
  expect(Math.max(...samples.map((sample) => sample.offset - sample.max))).toBeLessThanOrEqual(1.5);
});

test("does not apply bottom compensation when collapsing right after expanding", async () => {
  const app = await open("./App.scroll-expand.fixture.tsx", {
    platform: "macos",
    width: 900,
    height: 700,
  });
  await app.waitFor("Final answer");
  await app.press("id:work-expand-turn");
  await app.wait(500);
  expect(app.shows("id:latest-messages")).toBe(true);

  await app.press("id:work-expand-turn");
  await app.wait(500);
  expect(app.shows("id:activity-expand-reasoning")).toBe(false);
  expect(app.shows("id:latest-messages")).toBe(false);

  const report = app.texts().find((text) => text.startsWith("SCROLL_LOG"));
  const samples = report
    ? JSON.parse(report.slice("SCROLL_LOG".length)) as { offset: number }[]
    : [];
  const offsets = samples.map((sample) => sample.offset);
  const travel = offsets.length ? Math.max(...offsets) - Math.min(...offsets) : 0;
  expect(travel).toBeLessThanOrEqual(1.5);
});

test("stays visible until reaching the latest messages", async () => {
  const app = await open("./App.scroll-expand.fixture.tsx", {
    platform: "macos",
    width: 900,
    height: 700,
  });
  await app.waitFor("Final answer");
  await app.scroll("id:message-list", -600);
  await app.waitFor("id:latest-messages");
  await app.press("id:latest-messages");
  await app.wait(500);

  const log = app.texts().find((text) => text.startsWith("LATEST_LOG"));
  expect(log).toBeDefined();
  const samples = JSON.parse(log!.slice("LATEST_LOG".length)) as { visible: boolean; distance: number }[];
  expect(samples.some((sample) => sample.visible)).toBe(true);
  expect(samples.every((sample) => sample.visible || sample.distance <= 1)).toBe(true);
  expect(samples[samples.length - 1]?.visible).toBe(false);
  expect(samples[samples.length - 1]?.distance).toBeLessThanOrEqual(1);
});

test("passes wheel scrolling through the latest messages button", async () => {
  const app = await open("./App.scroll-expand.fixture.tsx", {
    platform: "macos",
    width: 900,
    height: 700,
  });
  await app.waitFor("Final answer");
  await app.scroll("id:message-list", -200);
  await app.waitFor("id:latest-messages");
  const before = app.texts().find((text) => text.startsWith("POSITION_"));
  expect(before).toBeDefined();

  await app.hover("id:latest-messages");
  await app.scroll("id:latest-scroll", -100);
  const after = app.texts().find((text) => text.startsWith("POSITION_"));
  expect(after).toBeDefined();
  expect(Number(after!.slice("POSITION_".length))).toBeGreaterThan(Number(before!.slice("POSITION_".length)) + 50);

  await app.press("id:latest-messages");
  await app.waitUntil(() => !app.shows("id:latest-messages"));
});

test("keeps short conversations at the top", async () => {
  const app = await open("./App.scroll-short.fixture.tsx", {
    platform: "macos",
    width: 900,
    height: 700,
  });
  await app.waitFor("Message 1");
  expect(app.shows("Message 3")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(false);
});

test("collapses long work and inner cards without hiding the final answer", async () => {
  const app = await open("./App.collapse.fixture.tsx", {
    platform: "macos",
    width: 1200,
    height: 800,
  });
  await app.waitFor("当前状态");
  await app.keyboard.press("mod+f");
  await app.type("id:thread-search-input", "module-1");
  await app.keyboard.press("enter");
  await app.keyboard.press("escape");
  await app.waitFor("id:work-turn-1");

  await app.press("id:work-turn-1");
  await app.waitFor("一句话概括");
  expect(app.shows("id:thinking-thinking")).toBe(false);

  await app.press("id:work-turn-1");
  await app.waitFor("id:thinking-thinking");
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-0");
  await app.press("id:tool-tool-0");
  await app.waitFor("id:wrap-tool-0-command");
  await app.press("id:tool-tool-0");
  await app.waitUntil(() => !app.shows("id:wrap-tool-0-command"));

  await app.press("id:thinking-thinking");
  await app.waitFor("text:/Analyze the project structure/");
  await app.press("id:thinking-thinking");
  await app.waitUntil(() => !app.shows("text:/Analyze the project structure/"));

  await app.press("id:work-turn-1");
  await app.waitFor("一句话概括");
});

test("keeps long nested content and siblings consistent through reveals", async () => {
  const app = await open("./App.collapse.fixture.tsx", {
    platform: "macos",
    width: 1200,
    height: 800,
  });
  await app.waitFor("当前状态");
  await app.keyboard.press("mod+f");
  await app.type("id:thread-search-input", "module-1");
  await app.keyboard.press("enter");
  await app.keyboard.press("escape");
  await app.waitFor("id:work-turn-1");

  await app.press("id:thinking-thinking");
  await app.wait(120);
  expect(app.shows("text:/Reasoning section 1/")).toBe(true);
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-0");
  expect(app.shows("id:tool-tool-0")).toBe(true);
  await app.wait(400);

  await app.press("id:thinking-thinking");
  await app.wait(400);
  expect(app.shows("text:/Reasoning section 1/")).toBe(false);

  await app.press("id:work-turn-1");
  await app.wait(120);
  await app.press("id:work-turn-1");
  await app.wait(400);
  expect(app.shows("id:thinking-thinking")).toBe(true);
});
