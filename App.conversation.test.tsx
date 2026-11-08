import { expect, open, test } from "nsh/test";
import {
  activitySummary,
  buildConversationRows,
  conversationRowsEqual,
  formatClock,
  groupConversationActivity,
  formatTokens,
  mentionSegments,
  parseReadOutput,
  parseSearchOutput,
  partialJsonField,
  reuseConversationRows,
  streamingDiffRows,
  turnDurationMs,
  turnSpeed,
  userMessageIsLong,
} from "./app/conversation";
import type { ChatItem, TurnState } from "./app/protocol";
import { setLanguagePreference } from "./app/i18n";

const turn: TurnState = {
  id: "turn-1",
  status: "done",
  startedAt: 0,
  durationMs: 2400,
  stats: {
    input: 100,
    cache_read: 300,
    output: 200,
    duration_ms: 2400,
    api_ms: 2000,
    ttft_ms: 500,
    ttft_steps: 1,
    api_steps: 1,
    tool_ms: 300,
    ended_at_ms: 2400,
  },
  changes: [{ path: "src/a.ts", unified_diff: "", additions: 3, deletions: 1 }],
  hooks: [],
};

const items: ChatItem[] = [
  { kind: "user", id: "u1", text: "hello", files: [], imageNums: [], sentAtMs: 1 },
  { kind: "reasoning", id: "r1", text: "think", streaming: false, turnId: "turn-1" },
  {
    kind: "tool",
    id: "call-1",
    tool: "Read",
    summary: "src/a.ts",
    detail: "{}",
    output: "1\tone",
    status: "done",
    turnId: "turn-1",
  },
  { kind: "markdown", id: "m1", text: "intermediate", streaming: false, turnId: "turn-1" },
  { kind: "markdown", id: "m2", text: "final", streaming: false, turnId: "turn-1" },
];

test("folds a finished turn around its final answer and restores work on demand", () => {
  const collapsed = buildConversationRows(items, [turn]);
  expect(collapsed.map((row) => row.type)).toEqual(["item", "work", "item", "changes", "actions"]);
  expect(collapsed[1].type).toBe("work");
  if (collapsed[1].type === "work") {
    expect(collapsed[1].items.map((item) => item.id)).toEqual(["r1", "call-1", "m1"]);
  }
  expect(collapsed[2].type).toBe("item");
  if (collapsed[2].type === "item") expect(collapsed[2].item.id).toBe("m2");

  const expanded = buildConversationRows(items, [turn]);
  expect(expanded.map((row) => row.type)).toEqual(collapsed.map((row) => row.type));
  if (expanded[1].type === "work") {
    expect(expanded[1].items.map((item) => item.id)).toEqual(["r1", "call-1", "m1"]);
  }
});

test("groups consecutive activity while preserving body and single tools", () => {
  const grouped = groupConversationActivity(items);
  expect(grouped.map((row) => row.type)).toEqual(["item", "activity", "item", "item"]);
  expect(grouped[1].id).toBe("activity-r1");
  if (grouped[1].type === "activity") {
    expect(grouped[1].items.map((item) => item.id)).toEqual(["r1", "call-1"]);
  }

  const call = items[2] as Extract<ChatItem, { kind: "tool" }>;
  const appended = groupConversationActivity([items[0], items[1], call, { ...call, id: "call-2" }]);
  expect(appended.map((row) => row.type)).toEqual(["item", "activity"]);
  expect(appended[1].id).toBe(grouped[1].id);
  expect(groupConversationActivity([call, items[3]]).map((row) => row.type)).toEqual(["item", "item"]);
});

test("summarizes activity categories and failures in both locales", () => {
  const call = items[2] as Extract<ChatItem, { kind: "tool" }>;
  const activityItems: ChatItem[] = [
    items[1],
    { ...call, id: "grep-1", tool: "Grep" },
    { ...call, id: "grep-2", tool: "Grep", status: "error" },
    { ...call, id: "glob-1", tool: "Glob" },
    { ...call, id: "bash-1", tool: "Bash" },
    { ...call, id: "web-1", tool: "WebSearch", status: "error" },
    call,
  ];
  try {
    setLanguagePreference("en");
    expect(activitySummary(activityItems)).toBe("Searched 2 patterns (1 failure) · Found files 1 time · Ran 1 command · Searched the web 1 time (1 failure) · Thought 1 time · Used 1 tool");
    setLanguagePreference("zh-CN");
    expect(activitySummary(activityItems)).toBe("搜索了 2 个模式（1 失败） · 找了 1 次文件 · 运行了 1 条命令 · 网络搜索了 1 次（1 失败） · 思考了 1 次 · 执行了 1 次工具调用");
  } finally {
    setLanguagePreference("system");
  }
});

test("keeps row identity while Markdown streams and releases it when the text settles", () => {
  const runningTurn: TurnState = { ...turn, status: "running" };
  const before = buildConversationRows(items, [runningTurn]);
  const streamed = items.map((item) => item.id === "m2" && item.kind === "markdown"
    ? { ...item, text: `${item.text} with more` }
    : item);
  expect(conversationRowsEqual(before, buildConversationRows(streamed, [runningTurn]))).toBe(true);
  const settled = streamed.map((item) => item.id === "m2" && item.kind === "markdown"
    ? { ...item, streaming: true }
    : item);
  expect(conversationRowsEqual(before, buildConversationRows(settled, [runningTurn]))).toBe(false);
});

test("reuses unchanged row objects while tool input streams", () => {
  const runningTurn: TurnState = { ...turn, status: "running" };
  const before = buildConversationRows(items, [runningTurn]);
  const streamed = items.map((item) => item.id === "call-1" && item.kind === "tool"
    ? { ...item, detail: '{"path":"src/a.ts"' }
    : item);
  const next = buildConversationRows(streamed, [runningTurn]);
  const merged = reuseConversationRows(before, next);
  expect(merged).not.toBe(before);
  for (const index of [0, 2, 3, 4]) expect(merged[index]).toBe(before[index]);
  expect(merged[1]).toBe(next[1]);
  expect(merged[1]).not.toBe(before[1]);
  expect(conversationRowsEqual(before, next)).toBe(false);
  expect(conversationRowsEqual(merged, next)).toBe(true);
});

test("parses numbered Read output and preserves trailing notes", () => {
  expect(parseReadOutput("8\tconst a = 1;\n9\t\n[truncated]")).toEqual({
    lines: [
      { number: 8, text: "const a = 1;" },
      { number: 9, text: "" },
    ],
    notes: ["[truncated]"],
  });
  expect(parseReadOutput("(File unchanged)")).toBeNull();
});

test("parses Grep content and count rows without splitting Windows drive letters", () => {
  expect(parseSearchOutput("Grep", "C:/work/src/a.ts:42: const needle = true;\nsrc/b.ts:3")).toEqual({
    rows: [
      { path: "C:/work/src/a.ts", line: 42, content: "const needle = true;" },
      { path: "src/b.ts", content: "3" },
    ],
    notes: [],
  });
});

test("resolves replay durations from stats without inventing unknown durations", () => {
  expect(turnDurationMs({ ...turn, durationMs: 0, stats: { ...turn.stats!, duration_ms: 33000 } })).toBe(33000);
  expect(turnDurationMs({ ...turn, durationMs: 2400, stats: null })).toBe(2400);
  expect(turnDurationMs({ ...turn, durationMs: 0, stats: null })).toBe(0);
  expect(turnDurationMs({ ...turn, durationMs: 2400, stats: { ...turn.stats!, duration_ms: 0 } })).toBe(2400);
  expect(turnDurationMs(undefined)).toBeUndefined();
});

test("calculates decode speed from provider timing rather than character estimates", () => {
  expect(Math.abs(turnSpeed(turn.stats!)! - 200 / 1.5) < 0.0001).toBe(true);
  expect(turnSpeed({ ...turn.stats!, ttft_ms: 0 })).toBeNull();
});

test("formats compact tokens and wall-clock stamps like the original turn actions", () => {
  expect(formatTokens(999)).toBe("999");
  expect(formatTokens(1000)).toBe("1K");
  expect(formatTokens(12400)).toBe("12.4K");
  expect(formatTokens(1000000)).toBe("1M");
  expect(formatTokens(1250000)).toBe("1.3M");
  const now = new Date(2026, 10, 2, 13, 15).getTime();
  expect(formatClock(now, now)).toBe("13:15");
  expect(formatClock(new Date(2026, 9, 29, 16, 27).getTime(), now)).toBe("10-29 16:27");
  expect(formatClock(0, now)).toBe("");
});

test("splits file and image mentions and identifies long user bubbles", () => {
  expect(mentionSegments("read @src/a.ts and Image 2", ["src/a.ts"], 2)).toEqual([
    { type: "text", text: "read " },
    { type: "file", text: "src/a.ts" },
    { type: "text", text: " and " },
    { type: "image", text: "Image 2", index: 1 },
  ]);
  expect(userMessageIsLong("one\ntwo\nthree\nfour\nfive\nsix")).toBe(true);
  expect(userMessageIsLong("short")).toBe(false);
});

function openApp() {
  return open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
}

test("collapses completed work and keeps turn actions available", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.waitFor("text:/工作了/");
  expect(app.count("label:复制回答")).toBe(1);
  expect(app.texts()).not.toContain("读取");

  await app.press("text:/工作了/");
  await app.press("text:/执行了 1 次工具调用/");
  await app.waitFor("读取");
});

test("keeps stop and send as separate controls during a running turn", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "SLOW_BASH_SCENARIO");
  await app.press("id:send-button");
  await app.waitFor("id:stop-button", { timeout: 20000 });
  expect(app.count("id:send-button")).toBe(1);
  await app.press("id:stop-button");
  await app.waitFor("id:stop-button", { state: "gone", timeout: 20000 });
  expect(app.count("id:send-button")).toBe(1);
});

test("pastes an image and sends it as a persisted user attachment", async () => {
  const app = await openApp();
  await app.paste("id:composer-input", {
    text: "describe this image",
    files: ["target/test-tmp/conversation/pixel.png"],
  });
  await app.waitFor("label:移除图片");
  expect(app.value("id:composer-input")).toBe("describe this image");
  await app.press("id:send-button");
  await app.waitFor("label:图片 1", { timeout: 20000 });
  expect(app.count("label:移除图片")).toBe(0);
  await app.press("label:图片 1");
  await app.wait(100);
  expect(app.texts()).toContain("图片 1");
});

test("searches and inserts an @ file reference through the real composer", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "@README");
  await app.waitFor("id:mention-0", { timeout: 10000 });
  await app.press("id:mention-0");
  expect(app.value("id:composer-input")).toContain("@README.mock.md");
  expect(app.texts()).toContain("README.mock.md");
});

test("opens a Read result in the right-side file tab", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.waitFor("text:/工作了/");
  await app.press("text:/工作了/");
  await app.press("text:/执行了 1 次工具调用/");
  await app.press("id:read-path");
  await app.waitFor("id:right-tab-file:README.mock.md");
  expect(app.texts()).toContain("README.mock.md");
  expect(app.texts()).toContain("A known file used by the pig-core mock provider for self-testing.");
});

test("materializes Write and Edit previews from incomplete streaming JSON", () => {
  const partial = '{"path":"src/a.ts","old_string":"old\\nline","new_string":"new\\nline\\nsecond';
  expect(partialJsonField(partial, "path")).toBe("src/a.ts");
  expect(partialJsonField(partial, "old_string")).toBe("old\nline");
  expect(streamingDiffRows("Edit", partial)).toEqual([
    { kind: "deleted", text: "old" },
    { kind: "deleted", text: "line" },
    { kind: "added", text: "new" },
    { kind: "added", text: "line" },
    { kind: "added", text: "second" },
  ]);
  expect(streamingDiffRows("Write", '{"content":"first\\nsecond')).toEqual([
    { kind: "added", text: "first" },
    { kind: "added", text: "second" },
  ]);
  expect(streamingDiffRows("Write", '{"path":"src/a.ts"}')).toEqual([]);
});

test("searches within the transcript", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.waitFor("text:/工作了/");
  await app.type("id:composer-input", "read the mock file again");
  await app.press("id:send-button");
  await app.waitFor("label:跳转到回合 2", { timeout: 20000 });

  expect(app.count("label:跳转到回合 1")).toBe(1);
  expect(app.count("label:跳转到回合 2")).toBe(1);
  await app.press("label:跳转到回合 1");
  await app.keyboard.press("mod+f");
  await app.waitFor("id:thread-search-input");
  await app.type("id:thread-search-input", "Conclusion");
  expect(app.texts()).toContain("1/2");
  await app.keyboard.press("enter");
  expect(app.texts()).toContain("2/2");
  await app.press("label:关闭搜索");
  expect(app.count("id:thread-search-input")).toBe(0);
});

test("copies and recalls the latest user message through the original action controls", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.hover("text:read the mock file");
  await app.press("label:复制用户消息");
  await app.waitUntil(() => app.clipboard() === "read the mock file");
  await app.press("label:撤回并编辑");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { state: "gone" });
  expect(app.value("id:composer-input")).toBe("read the mock file");
  expect(app.count("label:撤回并编辑")).toBe(0);
});
