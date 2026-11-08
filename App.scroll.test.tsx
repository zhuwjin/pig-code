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

  await app.scroll("id:message-list", -20000);
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
