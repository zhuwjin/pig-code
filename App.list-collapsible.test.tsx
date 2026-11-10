import { expect, open, test } from "nsh/test";

function heights(app: { texts(): string[] }): number[] {
  const sample = app.texts().find((text) => text.includes(",") && /^[\d.,]+$/.test(text));
  expect(sample).toBeDefined();
  return sample!.split(",").map(Number);
}

test("measures throttled intermediate window heights and exact final sizes", async () => {
  const app = await open("./App.list-collapsible.fixture.tsx", {
    platform: "macos",
    width: 800,
    height: 600,
  });
  await app.waitFor("id:toggle");

  await app.press("id:toggle");
  await app.wait(500);
  const opening = heights(app);
  expect(opening.some((height) => height > 0 && height < 320)).toBe(true);
  expect(opening.length).toBeLessThanOrEqual(15);
  expect(opening[opening.length - 1]).toBe(320);

  await app.press("id:toggle");
  await app.wait(500);
  const closing = heights(app);
  expect(closing.slice(opening.length).some((height) => height > 0 && height < 320)).toBe(true);
  expect(closing.length).toBeLessThanOrEqual(30);
  expect(closing[closing.length - 1]).toBe(0);
});

test("reports transition lifecycle while preserving every animated collapse frame", async () => {
  const app = await open("./App.list-collapsible.fixture.tsx", {
    platform: "macos",
    width: 800,
    height: 600,
  });
  await app.waitFor("id:toggle");
  await app.press("id:toggle");
  await app.wait(500);
  const opening = heights(app);

  await app.press("id:toggle");
  await app.wait(500);
  const closing = heights(app);
  expect(closing.slice(opening.length).some((height) => height > 0 && height < 320)).toBe(true);
  expect(app.texts()).toContain("start,end,start,end,");
});

test("does not restart a transition when another expansion state changes", async () => {
  const app = await open("./App.list-collapsible-isolated.fixture.tsx", {
    platform: "macos",
    width: 800,
    height: 600,
  });
  await app.waitFor("id:first");
  await app.press("id:first");
  await app.wait(100);
  await app.press("id:other");
  await app.wait(500);

  expect(app.texts()).toContain("start,end,");
  const measured = heights(app);
  expect(measured[measured.length - 1]).toBe(320);
  expect(measured.every((height, index) => index === 0 || height >= measured[index - 1])).toBe(true);
});
