import { expect, open, test } from "nsh/test";
import { AgentStore } from "../app/store";
import type { ChatItem } from "../app/protocol";

async function openCards() {
  const app = await open("./App.tool-cards.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1200,
    height: 800,
  });
  await app.setColorScheme("dark");
  return app;
}

test("shows agent states and orders swarm by completion sequence", async () => {
  const app = await openCards();
  expect(app.shows("id:agent-failed-agent-failed")).toBe(true);
  expect(app.shows("id:agent-complete-agent-failed")).toBe(false);
  expect(app.shows("id:agent-waiting-agent-pending")).toBe(true);
  expect(app.shows("id:agent-complete-agent-complete")).toBe(true);
  await app.press("id:swarm-swarm");
  expect(app.shows("id:swarm-parent-swarm")).toBe(true);

  const texts = app.texts();
  const order = ["Agent three", "Agent one", "Agent two", "Agent four"].map((name) => texts.findLastIndex((text) => text === name));
  expect(order).toEqual([...order].sort((a, b) => a - b));
  for (const row of ["01", "02", "03", "04"]) expect(app.texts()).toContain(row);
});

test("switches a tool row to an agent card when the card arrives", async () => {
  const app = await openCards();
  expect(app.shows("id:tool-late-agent")).toBe(true);
  await app.press("id:attach-agent");
  await app.waitFor("id:agent-late-agent-1");
  expect(app.shows("id:tool-late-agent")).toBe(false);
});

test("shows plan status colors and a file chip", async () => {
  const app = await openCards();
  expect(app.spanStyle("text:计划已批准").color).toBe("#4ade80ff");
  expect(app.spanStyle("text:已拒绝").color).toBe("#f87171ff");
  expect(app.spanStyle("text:待确认").color).toBe("#facc15ff");
  expect(app.texts()).toContain("model-editor.tsx");
  expect(app.texts()).toContain("app/components/settings/");
});

test("copies and opens a Read path through the context menu", async () => {
  const app = await openCards();
  await app.rightClick("id:read-path");
  await app.waitFor("id:read-copy-path");
  await app.press("id:read-copy-path");
  expect(app.clipboard()).toBe("README.mock.md");

  await app.rightClick("id:read-path");
  await app.waitFor("id:read-open-right");
  await app.press("id:read-open-right");
  await app.waitFor("id:right-tab-file:README.mock.md");
});

test("shows a TaskWait as the task it waits for", async () => {
  const app = await openCards();
  expect(app.texts()).toContain("等待");
  expect(app.texts()).toContain("b1");
  expect(app.texts()).not.toContain('{"task_ids":["b1"],"timeout_ms":30000}');
});

test("advances the reasoning ticker", async () => {
  const app = await openCards();
  await app.press("id:advance-thinking");
  await app.waitFor("Second rolling line with a longer suffix");
  await app.wait(250);
  expect(app.shows("First rolling line")).toBe(false);
});

test("upserts subagent conversation snapshots and updates by stable item id", () => {
  const store = new AgentStore(true);
  const reducer = store as unknown as { reduce(event: Record<string, unknown>): void };
  const call: ChatItem = {
    kind: "tool",
    id: "t1-0",
    tool: "TaskWait",
    summary: "wait for b1",
    detail: '{"task_ids":["b1"]}',
    output: "",
    status: "done",
    turnId: "turn-1",
  };
  const snapshot = {
    ConversationSnapshot: {
      session_id: "session",
      agent_id: "a1",
      title: "child",
      subtitle: "test · model",
      items: [call],
      running: true,
    },
  };
  reducer.reduce(snapshot);
  reducer.reduce({
    ConversationUpdate: {
      session_id: "session",
      agent_id: "a1",
      item: { ...call, output: "done" },
    },
  });
  expect(store.subagents()["session:a1"].items).toHaveLength(1);
  const first = store.subagents()["session:a1"].items[0];
  expect(first.kind).toBe("tool");
  if (first.kind !== "tool") throw new Error("expected tool row");
  expect(first.output).toBe("done");

  reducer.reduce(snapshot);
  expect(store.subagents()["session:a1"].items).toHaveLength(1);
  const merged = store.subagents()["session:a1"].items[0];
  expect(merged.kind).toBe("tool");
  if (merged.kind !== "tool") throw new Error("expected tool row");
  expect(merged.output).toBe("done");
});

test("isolates subagent conversation events by run id", () => {
  const store = new AgentStore(true);
  const reducer = store as unknown as { reduce(event: Record<string, unknown>): void };
  const item: ChatItem = {
    kind: "tool",
    id: "t1-0",
    tool: "Read",
    summary: "run",
    detail: "{}",
    output: "current",
    status: "done",
  };
  const snapshot = (runId: string, title: string) => reducer.reduce({
    ConversationSnapshot: {
      session_id: "session",
      agent_id: "a1",
      run_id: runId,
      title,
      items: [],
      running: true,
    },
  });
  const status = (runId: string, running: boolean) => reducer.reduce({
    ConversationStatus: { session_id: "session", agent_id: "a1", run_id: runId, running },
  });
  snapshot("run-1", "first");
  status("run-2", true);
  snapshot("run-2", "second");
  snapshot("run-1", "stale");
  status("run-1", false);
  reducer.reduce({
    ConversationUpdate: { session_id: "session", agent_id: "a1", run_id: "run-1", item },
  });
  expect(store.subagents()["session:a1"].runId).toBe("run-2");
  expect(store.subagents()["session:a1"].title).toBe("second");
  expect(store.subagents()["session:a1"].running).toBe(true);
  expect(store.subagents()["session:a1"].items).toHaveLength(0);
  reducer.reduce({
    ConversationUpdate: { session_id: "session", agent_id: "a1", run_id: "run-2", item },
  });
  expect(store.subagents()["session:a1"].items).toHaveLength(1);
  reducer.reduce({
    ConversationRemove: { session_id: "session", agent_id: "a1", run_id: "run-1", item_ids: ["t1-0"] },
  });
  expect(store.subagents()["session:a1"].items).toHaveLength(1);
});

test("promotes a pending tool to running without regressing a finished row", () => {
  const store = new AgentStore(true);
  const reducer = store as unknown as { reduce(event: Record<string, unknown>): void };
  const update = (item: ChatItem) => reducer.reduce({
    ConversationUpdate: { session_id: "session", agent_id: "a1", item },
  });
  const row = (id: string) => {
    const item = store.subagents()["session:a1"].items.find((entry) => entry.id === id);
    if (!item || item.kind !== "tool") throw new Error(`expected tool ${id}`);
    return item;
  };
  const base: Extract<ChatItem, { kind: "tool" }> = {
    kind: "tool",
    id: "t1-0",
    tool: "Bash",
    summary: "",
    detail: "",
    output: "",
    status: "input",
  };
  update(base);
  update({ ...base, status: "running", summary: "Start-Sleep", detail: '{"command":"sleep 1"}' });
  expect(row("t1-0").status).toBe("running");
  update({ ...base, status: "done", summary: "Start-Sleep", detail: '{"command":"sleep 1"}', output: "slept" });
  expect(row("t1-0").status).toBe("done");
  // A delayed live update must not reopen the finished row.
  update(base);
  expect(row("t1-0").status).toBe("done");
});
