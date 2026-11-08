import { expect, open, test } from "nsh/test";
import { shimmerLayerX } from "./app/components/shimmer-text";

test("shows one accessible label and mounts the native highlight only while active", async () => {
  const app = await open("./App.shimmer-text.fixture.tsx", {
    platform: "macos",
    locale: "en-US",
    width: 500,
    height: 240,
    colorScheme: "dark",
    clock: { time: 0 },
  });
  await app.waitFor("id:shimmer-active-highlight");
  expect(app.count("label:Thinking")).toBe(1);
  expect(app.count("text:Thinking")).toBe(13);
  expect(app.count("label:Static text")).toBe(1);
  expect(app.shows("id:shimmer-inactive-highlight")).toBe(false);
  await app.clock!.runFor(400);
  expect(app.count("label:Thinking")).toBe(1);
  await app.clock!.runFor(400);
  expect(app.count("label:Thinking")).toBe(1);
});

test("keeps static text without a duplicate highlight under reduced motion", async () => {
  const app = await open("./App.shimmer-text.fixture.tsx", {
    platform: "macos",
    locale: "en-US",
    width: 500,
    height: 240,
    colorScheme: "dark",
    reducedMotion: true,
  });
  await app.waitFor("text:Thinking");
  expect(app.count("label:Thinking")).toBe(1);
  expect(app.count("text:Thinking")).toBe(1);
  expect(app.shows("id:shimmer-active-highlight")).toBe(false);
});

test("starts the highlight outside the label and centers it halfway through", () => {
  expect(shimmerLayerX(0, 100, 35, 30)).toBe(-65);
  expect(shimmerLayerX(0.5, 100, 35, 30)).toBe(20);
  expect(shimmerLayerX(1, 100, 35, 30)).toBe(105);
});
