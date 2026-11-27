import { expect, open, test } from "nsh/test";
import { buildSearchResults, nextSearchSelection, nextSearchTab } from "../app/components/search-popup";
import type { SessionMeta, WorkspaceMeta } from "../app/protocol";

const ALPHA = "C:/work/alpha";
const BETA = "C:/work/beta";

function session(id: string, title: string, updatedAt: number, cwd = ALPHA, archived = false): SessionMeta {
  return {
    id,
    title,
    cwd,
    created_at: updatedAt,
    updated_at: updatedAt,
    pinned: false,
    archived,
  };
}

const workspaces: WorkspaceMeta[] = [
  { path: ALPHA, added_at: 1, alias: "Alpha Team", hidden: false },
  { path: BETA, added_at: 2, hidden: false },
];

test("cycles search scopes and wraps the selected result", () => {
  expect(nextSearchTab("all", 1)).toBe("workspaces");
  expect(nextSearchTab("sessions", 1)).toBe("all");
  expect(nextSearchTab("all", -1)).toBe("sessions");
  expect(nextSearchSelection(0, -1, 2)).toBe(1);
  expect(nextSearchSelection(1, 1, 2)).toBe(0);
  expect(nextSearchSelection(0, 1, 0)).toBe(0);
});

test("groups, sorts and deduplicates original-style search results", () => {
  const sessions = [
    session("old", "Project notes", 200),
    session("new", "Release checklist", 300, BETA),
    session("archived", "Release archived", 400, ALPHA, true),
  ];
  const all = buildSearchResults({ query: "", tab: "all", cwd: ALPHA, sessions, workspaces });
  expect(all.map((result) => result.kind)).toEqual(["workspace", "workspace", "session", "session"]);
  expect(all.filter((result) => result.kind === "workspace").map((result) => result.title)).toEqual(["beta", "Alpha Team"]);
  expect(all.filter((result) => result.kind === "session").map((result) => result.id)).toEqual(["new", "old"]);

  const aliased = buildSearchResults({ query: "alpha team", tab: "workspaces", cwd: ALPHA, sessions, workspaces });
  expect(aliased.map((result) => result.path)).toEqual([ALPHA]);
  const titlesAndContent = buildSearchResults({
    query: "notes",
    tab: "sessions",
    cwd: ALPHA,
    sessions,
    workspaces,
    contentHits: [
      { session_id: "old", before: "duplicate ", hit: "notes", after: "" },
      { session_id: "new", before: "release ", hit: "notes", after: " for later" },
    ],
  });
  expect(titlesAndContent.map((result) => `${result.kind}:${result.id}`)).toEqual(["session:old", "content:new"]);
  expect(titlesAndContent[1].snippet?.after).toBe(" for later");

  const commands = [{ instance: "plugin", title: "Release command", commandId: "release" }];
  expect(buildSearchResults({ query: "release", tab: "all", cwd: ALPHA, sessions, workspaces, commands }).at(-1)?.kind).toBe("command");
  expect(buildSearchResults({ query: "release", tab: "sessions", cwd: ALPHA, sessions, workspaces, commands }).some((result) => result.kind === "command")).toBe(false);
});

test("restores search filters, grouped metadata and conversation-content results", async () => {
  const app = await open("./App.selftest.tsx", {
    network: "live",
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.waitFor("id:session-list >> text:auto-title selftest", { timeout: 10000 });

  await app.press("id:open-search");
  await app.waitFor("id:global-search-input");
  expect(app.count("id:search-tab-all")).toBe(1);
  expect(app.count("id:search-tab-workspaces")).toBe(1);
  expect(app.count("id:search-tab-sessions")).toBe(1);
  expect(app.count("id:search-key-hints")).toBe(1);
  await app.waitFor("text:工作区 1");
  await app.waitFor("text:会话 1");
  await app.waitFor("id:global-search-results >> text:workspace · 刚刚");

  await app.keyboard.press("tab");
  expect(app.count("id:search-workspace-header")).toBe(1);
  expect(app.count("id:search-session-header")).toBe(0);
  await app.keyboard.press("tab");
  expect(app.count("id:search-workspace-header")).toBe(0);
  expect(app.count("id:search-session-header")).toBe(1);
  await app.keyboard.press("shift+tab");
  expect(app.count("id:search-workspace-header")).toBe(1);
  await app.keyboard.press("shift+tab");
  await app.waitFor("id:search-session-header");

  await app.type("id:global-search-input", "MOCK_REPLY_OK");
  await app.waitFor("id:search-content-header", { timeout: 10000 });
  expect(app.count("id:search-session-header")).toBe(0);
  await app.waitFor("id:global-search-results >> text:MOCK_REPLY_OK");
  await app.press("id:search-session-0");
  expect(app.count("id:global-search-input")).toBe(0);
});
