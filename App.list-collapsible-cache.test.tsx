import { expect, open, test } from "nsh/test";

function heights(app: { texts(): string[] }): number[] {
  const sample = app.texts().find((text) => text.includes(",") && /^[\d.,]+$/.test(text));
  expect(sample).toBeDefined();
  return sample!.split(",").map(Number);
}

test("restores measured heights when identities are recycled", async () => {
  const app = await open("./App.list-collapsible-cache.fixture.tsx", {
    platform: "macos",
    width: 800,
    height: 600,
  });
  await app.waitFor("id:switch");
  await app.wait(300);
  expect(heights(app).at(-1)).toBe(320);

  await app.press("id:switch");
  await app.wait(300);
  const other = heights(app);
  expect(other.at(-1)).toBe(160);

  await app.press("id:switch");
  await app.wait(300);
  const restored = heights(app);
  expect(restored[restored.length - 1]).toBe(320);
  expect(restored.slice(other.length)).toEqual([320]);

  const beforeRecycle = restored.length;
  await app.press("id:recycle");
  await app.wait(50);
  await app.press("id:recycle");
  await app.wait(300);
  expect(heights(app).slice(beforeRecycle)).toEqual([0, 320]);
});

test("does not reset the window to zero when content versions change", async () => {
  const app = await open("./App.list-collapsible-cache.fixture.tsx", {
    platform: "macos",
    width: 800,
    height: 600,
  });
  await app.waitFor("id:resize");
  await app.wait(300);
  const before = heights(app);

  await app.press("id:resize");
  await app.wait(300);
  const resized = heights(app);
  expect(resized.at(-1)).toBe(160);
  expect(resized.slice(before.length)).not.toContain(0);

  await app.press("id:resize");
  await app.wait(300);
  expect(heights(app).at(-1)).toBe(320);
});
