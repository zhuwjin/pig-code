import { open, test } from "nsh/test";
import { screenshotIfSupported, supportsScreenshots } from "./screenshot";

test("captures expansion after scrolling", async () => {
  const app = await open("../main.tsx", {
    platform: "windows",
    width: 900,
    height: 700,
    reducedMotion: false,
  });
  await app.waitFor("Final answer");
  if (!await supportsScreenshots(app)) return;
  await app.press("id:load-history");
  await screenshotIfSupported(app, "target/test-tmp/long-scroll-loaded.png");
  await app.scroll("id:message-list", -1000);
  await app.waitFor("id:latest-messages");
  await screenshotIfSupported(app, "target/test-tmp/long-scroll-scrolled.png");
  await app.press("id:show-work");
  await app.waitUntil(() => app.texts().some((text) => text.startsWith("Visible rows: ") && text.includes("work=yes")));
  await screenshotIfSupported(app, "target/test-tmp/long-scroll-show-work.png");
  await app.press("id:toggle-work");
  await app.wait(1000);
  await screenshotIfSupported(app, "target/test-tmp/long-scroll-expand.png");
});
