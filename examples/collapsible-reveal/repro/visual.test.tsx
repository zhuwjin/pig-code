import { expect, open, test } from "nsh/test";
import { screenshotIfSupported, supportsScreenshots } from "./screenshot";

test("captures official reveal in a real Windows window", async () => {
  const app = await open("../main.tsx", {
    platform: "windows",
    width: 900,
    height: 700,
    reducedMotion: false,
  });
  await app.waitFor("Final answer");
  if (!await supportsScreenshots(app)) return;
  await app.press("id:toggle-work");
  await app.waitFor("id:tool-39");
  expect(app.shows("Final answer")).toBe(true);
  await screenshotIfSupported(app, "target/test-tmp/official-reveal-expanded.png");

  await app.press("id:toggle-work");
  await app.waitUntil(() => !app.shows("id:tool-39"));
  await app.wait(500);
  expect(app.shows("Final answer")).toBe(true);
  expect(app.shows("id:latest-messages")).toBe(false);
  await screenshotIfSupported(app, "target/test-tmp/official-reveal-collapsed.png");
});
