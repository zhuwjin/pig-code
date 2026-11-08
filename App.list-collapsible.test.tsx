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
