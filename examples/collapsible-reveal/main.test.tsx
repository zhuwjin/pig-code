import { expect, open, test } from "nsh/test";

test("expands official reveal after inserting history rows", async () => {
  const app = await open("./main.tsx", {
    platform: "windows",
    width: 900,
    height: 700,
    reducedMotion: false,
  });
  await app.waitFor("Final answer");
  await app.press("id:load-history");
  await app.waitFor("History answer 80");
  await app.press("id:toggle-work");
  await app.waitFor("id:tool-39");
  expect(app.shows("Final answer")).toBe(true);
});
