import { expect, open, test } from "nsh/test";

function openFixture(colorScheme: "light" | "dark") {
  return open("./App.kbd.fixture.tsx", {
    platform: "macos",
    locale: "en-US",
    width: 480,
    height: 260,
    colorScheme,
  });
}

test("renders keycaps in every size and variant", async () => {
  const app = await openFixture("dark");
  await app.waitFor("id:kbd-group");
  for (const id of ["kbd-sm", "kbd-md", "kbd-lg", "kbd-raised", "kbd-outline", "kbd-subtle", "kbd-plain"]) {
    expect(app.shows(`id:${id}`)).toBe(true);
  }
  expect(app.spanStyle("id:kbd-sm >> text:K").fontSize).toBe(10);
  expect(app.spanStyle("id:kbd-md >> text:K").fontSize).toBe(12);
  expect(app.spanStyle("id:kbd-lg >> text:K").fontSize).toBe(14);
  expect(app.spanStyle("id:kbd-md >> text:K").fontWeight).toBe(500);
});

test("follows the theme's foreground color", async () => {
  const dark = await openFixture("dark");
  await dark.waitFor("id:kbd-md");
  const light = await openFixture("light");
  await light.waitFor("id:kbd-md");
  expect(dark.spanStyle("id:kbd-md >> text:K").color).not.toBe(light.spanStyle("id:kbd-md >> text:K").color);
});
