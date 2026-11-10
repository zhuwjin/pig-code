import { open, test } from "nsh/test";

test("captures expansion after scrolling", async () => {
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
  await app.wait(1000);
  await app.screenshot("target/test-tmp/long-scroll-expand.png");
});
