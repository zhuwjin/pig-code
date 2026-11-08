import { expect, open, test } from "nsh/test";
import { AgentStore } from "./app/store";
import type { ToolItem } from "./app/protocol";

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

test("advances the reasoning ticker", async () => {
  const app = await openCards();
  await app.press("id:advance-thinking");
  await app.waitFor("Second rolling line with a longer suffix");
  await app.wait(250);
  expect(app.shows("First rolling line")).toBe(false);
});

test("assigns completion sequence once for each agent", () => {
  const store = new AgentStore(true);
  const agent = (id: string): NonNullable<ToolItem["agents"]>[number] => ({
    agent_id: id,
    profile: "coder",
    description: id,
    model: "mock-model",
    background: false,
    finished: false,
  });
  const swarm: ToolItem = {
    kind: "tool",
    id: "swarm",
    tool: "AgentSwarm",
    summary: "Build",
    detail: "{}",
    output: "",
    status: "running",
    agents: [agent("a"), agent("b")],
  };
  store.current.set("session");
  store.items.set({ session: [swarm] });
  const reducer = store as unknown as { reduce(event: Record<string, unknown>): void };
  reducer.reduce({ SubagentActivity: { session_id: "session", agent_id: "a", finished: true, item: null } });
  reducer.reduce({ SubagentActivity: { session_id: "session", agent_id: "a", finished: true, item: null } });
  reducer.reduce({ SubagentActivity: { session_id: "session", agent_id: "b", finished: true, item: null } });
  const agents = store.items()["session"][0] as ToolItem;
  expect(agents.agents?.map((item) => item.finishedSeq)).toEqual([1, 2]);
});
