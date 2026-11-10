import { expect, open, test } from "nsh/test";

async function openPanel() {
  const app = await open("./App.tasks-panel.fixture.tsx", {
    platform: "windows",
    locale: "zh-CN",
    width: 520,
    height: 460,
    colorScheme: "dark",
  });
  await app.press("id:aux-tasks");
  await app.waitFor("id:task-filter-shell");
  return app;
}

test("shows a title and segmented filters, and filters tasks by state", async () => {
  const app = await openPanel();
  expect(app.texts()).toContain("后台终端");
  expect(app.shows("id:task-running")).toBe(true);
  expect(app.shows("id:task-queued")).toBe(true);
  expect(app.shows("id:task-done")).toBe(false);
  expect(app.shows("id:task-failed")).toBe(false);

  await app.press("id:task-filter-shell-finished");
  await app.waitFor("id:task-done");
  expect(app.shows("id:task-running")).toBe(false);
  expect(app.shows("id:task-failed")).toBe(true);

  await app.press("id:task-filter-shell-all");
  await app.waitFor("id:task-queued");
  expect(app.shows("id:task-done")).toBe(true);
  expect(app.shows("id:task-running")).toBe(true);

  await app.press("id:task-filter-shell-running");
  await app.waitFor("id:task-running");
  expect(app.shows("id:task-done")).toBe(false);
});
