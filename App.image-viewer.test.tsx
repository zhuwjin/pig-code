import { expect, open, test } from "nsh/test";
import { updateImageMentionsAfterRemoval } from "./app/media";

function openApp() {
  return open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
}

test("removes and renumbers image mentions without changing unrelated text", () => {
  expect(updateImageMentionsAfterRemoval("Image 1 look at Image 2 and Image 9", 1, 2)).toBe("look at Image 1 and Image 9");
  expect(updateImageMentionsAfterRemoval("before Image 2 after", 1, 2)).toBe("before Image 1 after");
  expect(updateImageMentionsAfterRemoval("Image 2", 1, 2)).toBe("Image 1");
});

test("mentions, previews, navigates, zooms and renumbers composer images", async () => {
  const app = await openApp();
  await app.paste("id:composer-input", {
    text: "describe this image",
    files: ["target/test-tmp/conversation/pixel.png"],
  });
  await app.waitFor("id:composer-image-image-1");
  await app.paste("id:composer-input", {
    text: "",
    files: ["target/test-tmp/conversation/pixel.png"],
  });
  await app.waitFor("id:composer-image-image-2");
  expect(app.texts()).toContain("图片 1");
  expect(app.texts()).toContain("图片 2");

  await app.hover("id:composer-image-image-2");
  await app.press("id:composer-image-mention-image-2");
  expect(app.value("id:composer-input")).toContain("Image 2 ");

  await app.press("id:composer-image-image-1");
  await app.waitFor("id:image-viewer");
  expect(app.texts()).toContain("1/2");
  const zoomBefore = app.texts().find((text) => /^\d+%$/.test(text));
  await app.press("id:image-viewer-zoom-in");
  const zoomAfter = app.texts().find((text) => /^\d+%$/.test(text));
  expect(zoomAfter).not.toBe(zoomBefore);
  await app.press("id:image-viewer-fit");
  expect(app.texts().find((text) => /^\d+%$/.test(text))).toBe(zoomBefore);
  await app.press("id:image-viewer-next");
  expect(app.texts()).toContain("2/2");
  await app.keyboard.press("left");
  expect(app.texts()).toContain("1/2");
  await app.press("id:image-viewer-close");
  await app.waitFor("id:image-viewer", { state: "gone" });

  await app.hover("id:composer-image-image-1");
  await app.press("id:composer-image-remove-image-1");
  expect(app.value("id:composer-input")).not.toContain("Image 2");
  expect(app.value("id:composer-input")).toContain("Image 1");
  expect(app.texts()).toContain("图片 1");
  expect(app.texts()).not.toContain("图片 2");
});
