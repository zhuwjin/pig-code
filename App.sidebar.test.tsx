import { expect, open, test } from "nsh/test";
import { languagePreference, setLanguagePreference } from "./app/i18n";
import { ICON_ELEMENTS } from "./app/components/icon-elements";
import {
  formatSessionTime,
  nextWorkspaceSessionLimit,
  sessionTitleScroll,
  workspaceSessionLimit,
} from "./app/components/sidebar";

test("uses filled geometry for pinned stars", () => {
  expect(ICON_ELEMENTS["star-fill"].every((element) => element.filled)).toBe(true);
});

test("formats session timestamps like the original sidebar", () => {
  const previous = languagePreference();
  setLanguagePreference("zh-CN");
  try {
    const now = 2_000_000_000;
    expect(formatSessionTime(now - 30, now)).toBe("刚刚");
    expect(formatSessionTime(now - 2 * 60, now)).toBe("2 分钟");
    expect(formatSessionTime(now - 16 * 3600, now)).toBe("16 小时");
    expect(formatSessionTime(now - 4 * 86400, now)).toBe("4 天");
    expect(formatSessionTime(now - 14 * 86400, now)).toBe("2 周");
    expect(formatSessionTime(now - 60 * 86400, now)).toBe("2 个月");
    expect(formatSessionTime(now + 30, now)).toBe("刚刚");
  } finally {
    setLanguagePreference(previous);
  }
});

test("paginates workspace sessions five at a time and resets after collapsing", () => {
  expect(workspaceSessionLimit(15, false)).toBe(5);
  expect(workspaceSessionLimit(15, true)).toBe(15);
  expect(nextWorkspaceSessionLimit(15, false)).toBe(10);
  expect(nextWorkspaceSessionLimit(10, true)).toBe(15);
});

test("expands and collapses the workspace from both disclosure controls", async () => {
  const app = await open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });

  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  const row = "id:session-list >> text:auto-title selftest";
  await app.waitFor(row, { timeout: 10000 });

  expect(app.count("id:session-list")).toBe(1);
  await app.press("id:workspace-row");
  expect(app.count(row)).toBe(0);
  expect(app.count("label:展开当前工作区")).toBe(1);
  await app.press("id:workspace-row");
  expect(app.count(row)).toBe(1);

  await app.press("id:toggle-workspace");
  expect(app.count(row)).toBe(0);
  expect(app.count("label:展开工作区")).toBe(1);
  await app.press("id:toggle-workspace");
  expect(app.count(row)).toBe(1);
});

test("renders compact workspace sessions and detailed flat sessions", async () => {
  const app = await open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });

  await app.waitFor("id:session-list >> text:刚刚");
  expect(app.count("id:session-detail")).toBe(0);

  await app.press("id:list-manage");
  expect(app.count("text:视图")).toBe(1);
  expect(app.count("text:平铺列表")).toBe(1);
  expect(app.count("text:按工作区分组")).toBe(1);
  await app.press("id:list-view-flat");
  await app.waitFor("id:session-detail");
  expect(app.count("id:session-list >> text:刚刚")).toBe(1);
});

test("supports workspace hover actions and the context menu", async () => {
  const app = await open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });

  await app.hover("id:workspace-row");
  expect(app.shows("id:workspace-menu")).toBe(true);
  expect(app.shows("id:workspace-add")).toBe(true);
  await app.press("id:workspace-add");
  expect(app.count("label:折叠当前工作区")).toBe(1);
  expect(app.count("id:session-list")).toBe(1);
  await app.hover("id:workspace-row");
  await app.press("id:workspace-menu");
  expect(app.count("id:workspace-new-task")).toBe(1);
  await app.press("id:workspace-new-task");

  await app.rightClick("id:workspace-row");
  expect(app.count("id:workspace-new-task")).toBe(1);
  expect(app.count("text:复制路径")).toBe(1);
  expect(app.count("text:重命名")).toBe(1);
  expect(app.count("text:移除工作区")).toBe(1);
  await app.press("id:workspace-copy-path");
  expect(app.clipboard()).toContain("workspace");

  await app.rightClick("id:workspace-row");
  await app.press("id:workspace-rename");
  await app.clear("id:workspace-rename-input");
  await app.type("id:workspace-rename-input", "renamed-workspace");
  await app.submit("id:workspace-rename-input");
  await app.waitFor("text:renamed-workspace");

  await app.rightClick("id:workspace-row");
  await app.press("id:workspace-remove");
  await app.waitFor("id:workspace-row", { state: "gone" });
  expect(app.count("id:session-list >> text:renamed-workspace")).toBe(0);
});

test("renames, pins, archives and deletes sessions through row actions", async () => {
  const app = await open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.waitFor("id:session-list >> text:auto-title selftest", { timeout: 10000 });

  await app.rightClick("id:session-list >> text:auto-title selftest");
  expect(app.count("text:重命名")).toBe(1);
  expect(app.count("text:置顶")).toBe(1);
  expect(app.count("text:归档")).toBe(1);
  expect(app.count("text:删除会话")).toBe(1);
  await app.press("text:重命名");
  await app.clear("id:session-rename-input");
  await app.type("id:session-rename-input", "Renamed session");
  await app.submit("id:session-rename-input");
  await app.waitFor("id:session-list >> text:Renamed session");

  await app.hover("id:session-list >> text:Renamed session");
  await app.press("label:置顶");
  await app.waitFor("id:pinned-sessions");
  await app.hover("id:pinned-sessions >> text:Renamed session");
  await app.waitFor("label:取消置顶");
  expect(app.count("id:pinned-sessions >> text:Renamed session")).toBe(1);
  expect(app.count("id:pinned-sessions >> text:workspace")).toBe(1);
  expect(app.count("id:session-list >> text:Renamed session")).toBe(1);

  await app.hover("id:pinned-sessions >> text:Renamed session");
  await app.press("label:归档");
  await app.waitFor("id:session-list >> text:Renamed session", { state: "gone" });

  await app.press("id:new-task");
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.waitFor("id:session-list >> text:auto-title selftest", { timeout: 10000 });
  await app.rightClick("id:session-list >> text:auto-title selftest");
  await app.press("text:删除会话");
  await app.waitFor("id:session-list >> text:auto-title selftest", { state: "gone" });
});

test("computes one-way title scrolling from the measured overflow", () => {
  const scroll = sessionTitleScroll(142, 116);
  expect(scroll.offset).toBe(-28);
  expect(Math.round(scroll.duration)).toBe(298);
  expect(sessionTitleScroll(100, 116).offset).toBe(0);
  expect(sessionTitleScroll(100, 116).duration).toBe(0);
});

test("shows the trailing fade for overflowing titles without hovering", async () => {
  const app = await open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.waitFor("id:session-list >> text:auto-title selftest", { timeout: 10000 });

  await app.rightClick("id:session-list >> text:auto-title selftest");
  await app.press("text:重命名");
  await app.clear("id:session-rename-input");
  const title = "A long session title that keeps its trailing fade without hovering. ".repeat(2);
  await app.type("id:session-rename-input", title);
  await app.submit("id:session-rename-input");
  await app.waitFor(`id:session-list >> text:${title}`);
  await app.waitFor("id:session-title-fade");
  expect(app.count("id:session-title-fade")).toBe(1);
});
