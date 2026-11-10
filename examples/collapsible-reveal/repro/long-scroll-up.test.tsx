import { open, test } from "nsh/test";
import { screenshotIfSupported } from "./screenshot";

test("expands official reveal while reading above the latest", async () => {
  const app = await open("../main.tsx", {
    platform: "windows",
    width: 900,
    height: 700,
    reducedMotion: false,
  });
  const visibleState = () => app.texts().find((text) => text.startsWith("Visible rows: ")) ?? "";
  await app.waitFor("Final answer");
  await app.press("id:load-history");
  await app.waitFor("History answer 80");
  await app.scroll("id:message-list", -1000);
  await app.waitFor("id:latest-messages");
  await app.press("id:show-work");
  await app.waitUntil(() => visibleState().includes("work=yes"));
  await app.press("id:toggle-work");
  await app.waitFor("id:tool-0");
  await app.waitUntil(() => visibleState().includes("work=yes"));
  if (!visibleState().includes("answer=yes")) await app.scroll("id:message-list", 2000);
  await app.waitFor("id:tool-39");
  await app.waitUntil(() => visibleState().includes("answer=yes"));
  await screenshotIfSupported(app, "target/test-tmp/long-scroll-up-final.png");
});
