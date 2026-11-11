import { expect, open, test } from "nsh/test";

const options = { platform: "macos" as const, width: 1200, height: 700 };

async function openExpand() {
  return open("./App.follow-expand.fixture.tsx", options);
}

async function openShort() {
  return open("./App.follow-short.fixture.tsx", options);
}

test("bottom: expanding work keeps the latest button hidden", async () => {
  const app = await openExpand();
  await app.waitFor("Matrix final answer");
  expect(app.shows("id:latest-messages")).toBe(false);
  await app.press("id:work-matrix-turn");
  await app.wait(500);
  expect(app.shows("id:latest-messages")).toBe(false);
  expect(app.shows("Matrix final answer")).toBe(true);
}, 30_000);

test("bottom: collapsing a long fold to short content rests at the bottom", async () => {
  const app = await openShort();
  await app.waitFor("id:tool-tool-0");
  await app.press("id:activity-tool-0");
  await app.waitUntil(() => !app.shows("id:tool-tool-0"));
  expect(app.shows("Short final answer")).toBe(true);
  expect(app.shows("Collapse this turn")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(false);
}, 30_000);
