import { expect, open, test } from "nsh/test";

test("captures official reveal in a real Windows window", async () => {
  const app = await open("../main.tsx", {
    platform: "windows",
    width: 900,
    height: 700,
    reducedMotion: false,
  });
  await app.waitFor("Final answer");
  await app.press("id:toggle-work");
  await app.waitFor("id:tool-39");
  expect(app.shows("Final answer")).toBe(true);
  await app.screenshot("target/test-tmp/official-reveal-expanded.png");

  await app.press("id:toggle-work");
  await app.waitUntil(() => !app.shows("id:tool-39"));
  expect(app.shows("Final answer")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(false);
  await app.screenshot("target/test-tmp/official-reveal-collapsed.png");
});
