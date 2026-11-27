import { mount, View } from "nsh";
import { Composer } from "../app/components/composer";
import { MessageList } from "../app/components/messages";
import type { ChatItem, TurnState } from "../app/protocol";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turn: TurnState = {
  id: "turn-stats",
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
    user_wait_ms: 100,
    tool_exec_ms: 150,
    tool_wall_ms: 200,
    compact_ms: 50,
    hook_ms: 20,
    review_ms: 30,
    overhead_ms: 20,
    ended_at_ms: 2400,
  },
  changes: [],
  hooks: [],
};
const items: ChatItem[] = [
  { kind: "user", id: "user-stats", text: "Show session statistics", files: [], imageNums: [], sentAtMs: 1 },
  { kind: "markdown", id: "answer-stats", text: "Statistics response", streaming: false, turnId: turn.id },
];
store.current.set("stats");
store.items.set({ stats: items });
store.turns.set({ stats: [turn] });
store.running.set({ stats: false });
store.contextUsage.set({
  stats: { used: 1600, total: 128000, cacheRead: 3000, input: 1000 },
});
store.contextBreakdowns.set({
  stats: { system: 500, tools: 300, messages: 800 },
});

mount(() => (
  <View class="h-full w-full">
    <MessageList store={store} themeStore={themeStore} items={store.items().stats ?? []} />
    <Composer store={store} themeStore={themeStore} />
  </View>
));
