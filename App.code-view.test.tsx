import { expect, open, test } from "nsh/test";

function openCodeView() {
  return open("./App.code-view.fixture.tsx", {
    platform: "macos",
    width: 1200,
    height: 800,
  });
}

test("virtualizes long read output and reaches the real last line", async () => {
  const app = await openCodeView();
  await app.press("id:open-read");
  await app.waitFor("id:code-read");
  await app.waitFor("# Plugin system 1");
  expect(app.shows("# Plugin system 151")).toBe(false);

  await app.scroll("id:code-read", 15000);
  await app.waitFor("# Plugin system 151");
  expect(app.shows("# Plugin system 1")).toBe(false);
});

test("keeps command and output interactive in the shared code view", async () => {
  const app = await openCodeView();
  await app.press("id:open-bash");
  await app.waitFor("id:code-bash-command");
  await app.waitFor("id:code-bash-output");
  await app.waitFor("total 744");

  await app.press("id:wrap-bash-command");
  await app.press("id:wrap-bash-output");
  await app.press("id:wrap-bash-command");
  await app.press("id:wrap-bash-output");
  await app.waitFor("total 744");

  await app.press("id:open-bash");
  await app.waitUntil(() => !app.shows("id:code-bash-command"));
  await app.press("id:open-bash");
  await app.waitFor("id:code-bash-command");
});

test("keeps vertical scrolling in the viewport while long lines scroll horizontally", async () => {
  const app = await openCodeView();
  await app.press("id:open-read");
  await app.waitFor("id:code-read-horizontal");
  await app.waitFor("id:code-read-horizontal-scrollbar");

  await app.scroll("id:code-read-horizontal", 800);
  for (let index = 0; index < 6; index += 1) {
    await app.scroll("id:code-read", 120);
    await app.scroll("id:code-read-horizontal", 40);
    await app.scroll("id:code-read-horizontal", -40);
  }
  await app.scroll("id:code-read", 4000);
  await app.waitFor("# Plugin system 151");
  expect(app.shows("# Plugin system 1")).toBe(false);
  expect(app.shows("id:code-read-horizontal")).toBe(true);

  await app.scroll("id:code-read-horizontal", -800);
  if (!app.shows("# Plugin system 151")) await app.scroll("id:code-read", 4000);
  await app.waitFor("# Plugin system 151");
  await app.scroll("id:code-read", -4000);
  await app.waitFor("# Plugin system 1");
});
