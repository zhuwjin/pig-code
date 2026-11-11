import { expect, open, test } from "nsh/test";

const options = { platform: "macos" as const, width: 1200, height: 700 };

async function openExpand() {
  return open("./App.follow-expand.fixture.tsx", options);
}

async function openShort() {
  return open("./App.follow-short.fixture.tsx", options);
}

test("bottom: expanding work and activity keeps the header and shows latest", async () => {
  const app = await openExpand();
  await app.waitFor("Matrix final answer");
  expect(app.shows("id:latest-messages")).toBe(false);
  await app.press("id:work-matrix-turn");
  await app.waitFor("id:activity-tool-0");
  expect(app.shows("id:work-matrix-turn")).toBe(true);
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-0");
  expect(app.shows("id:activity-tool-0")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(true);
}, 30_000);

test("10px above bottom: expanding keeps the position", async () => {
  const app = await openExpand();
  await app.waitFor("Matrix final answer");
  await app.scroll("id:message-list", -10);
  await app.waitFor("id:latest-messages");
  await app.press("id:work-matrix-turn");
  await app.waitFor("id:activity-tool-0");
  expect(app.shows("id:work-matrix-turn")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(true);
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-0");
  expect(app.shows("id:activity-tool-0")).toBe(true);
}, 30_000);

test("200px above bottom: expanding does not jump to the end", async () => {
  const app = await openExpand();
  await app.waitFor("Matrix final answer");
  await app.scroll("id:message-list", -200);
  await app.waitFor("id:latest-messages");
  await app.press("id:work-matrix-turn");
  await app.waitFor("id:activity-tool-0");
  expect(app.shows("id:work-matrix-turn")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(true);
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-0");
  expect(app.shows("id:activity-tool-0")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(true);
}, 30_000);

test("bottom: collapsing a long fold to short content rests at the top", async () => {
  const app = await openShort();
  await app.waitFor("id:tool-tool-0");
  await app.press("id:activity-tool-0");
  await app.waitUntil(() => !app.shows("id:tool-tool-0"));
  expect(app.shows("Short final answer")).toBe(true);
  expect(app.shows("Collapse this turn")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(false);
}, 30_000);

test("short content: expanding again shows the tools and latest", async () => {
  const app = await openShort();
  await app.waitFor("id:tool-tool-0");
  await app.press("id:activity-tool-0");
  await app.waitUntil(() => !app.shows("id:tool-tool-0"));
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-0");
  expect(app.shows("Collapse this turn")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(true);
}, 30_000);

test("10px above bottom: collapsing clamps back to the end", async () => {
  const app = await openExpand();
  await app.waitFor("Matrix final answer");
  await app.press("id:work-matrix-turn");
  await app.waitFor("id:activity-tool-0");
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-0");
  await app.scroll("id:message-list", 40000);
  await app.scroll("id:message-list", -50);
  await app.press("id:activity-tool-0");
  await app.waitUntil(() => !app.shows("id:tool-tool-0"));
  expect(app.shows("Matrix final answer")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(false);
}, 30_000);

test("bottom: collapsing while the list stays taller keeps the final answer", async () => {
  const app = await openExpand();
  await app.waitFor("Matrix final answer");
  await app.press("id:work-matrix-turn");
  await app.waitFor("id:activity-tool-0");
  await app.press("id:activity-tool-0");
  await app.waitFor("id:tool-tool-0");
  await app.scroll("id:message-list", 40000);
  await app.press("id:activity-tool-0");
  await app.waitUntil(() => !app.shows("id:tool-tool-0"));
  expect(app.shows("Matrix final answer")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(false);
}, 30_000);
