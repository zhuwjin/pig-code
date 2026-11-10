import { expect, open, test } from "nsh/test";

test("expands official reveal while reading above the latest", async () => {
  const app = await open("../main.tsx", {
    platform: "windows",
    width: 900,
    height: 700,
    reducedMotion: false,
  });
  await app.waitFor("Final answer");
  await app.press("id:load-history");
  await app.waitFor("History answer 80");
  await app.scroll("id:message-list", -1000);
  await app.waitFor("id:latest-messages");
  await app.press("id:show-work");
  await app.press("id:toggle-work");
  await app.waitFor("id:tool-0");
  expect(app.shows("id:tool-39")).toBe(false);
});
