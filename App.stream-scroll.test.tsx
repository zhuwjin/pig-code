import { expect, open, test } from "nsh/test";

function distanceFrom(text: string | undefined): number {
  return Number(text?.slice("POSITION_".length).split("_")[0] ?? Number.NaN);
}

test("stays put while rows stream in after scrolling up slowly", async () => {
  const app = await open("./App.stream-scroll.fixture.tsx", {
    platform: "macos",
    width: 900,
    height: 700,
  });
  await app.waitFor("id:message-list");
  await app.wait(800);
  for (const _ of [1, 2, 3, 4, 5]) {
    await app.scroll("id:message-list", -80);
    await app.wait(160);
  }
  await app.wait(300);
  const position = app.texts().find((text) => text.startsWith("POSITION_"));
  expect(distanceFrom(position)).toBeGreaterThan(300);
});
