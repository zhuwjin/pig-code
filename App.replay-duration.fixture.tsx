import { mount, View } from "nsh";
import { MessageList } from "./app/components/messages";
import { AgentStore } from "./app/store";
import { ThemeStore } from "./app/theme";
import type { ChatItem, TurnState } from "./app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turns: TurnState[] = [
  {
    id: "replay-1",
    status: "done",
    startedAt: 0,
    durationMs: 0,
    stats: {
      input: 100,
      cache_read: 0,
      output: 50,
      duration_ms: 33000,
      api_ms: 32000,
      ttft_ms: 500,
      ttft_steps: 1,
      api_steps: 1,
      tool_ms: 1000,
      ended_at_ms: 33000,
    },
    changes: [],
    hooks: [],
  },
  {
    id: "replay-2",
    status: "done",
    startedAt: 0,
    durationMs: 0,
    stats: null,
    changes: [],
    hooks: [],
  },
];
const items: ChatItem[] = [
  { kind: "reasoning", id: "reason-1", text: "Historical reasoning", streaming: false, turnId: "replay-1" },
  { kind: "markdown", id: "answer-1", text: "First answer", streaming: false, turnId: "replay-1" },
  { kind: "reasoning", id: "reason-2", text: "Reasoning without timing", streaming: false, turnId: "replay-2" },
  { kind: "markdown", id: "answer-2", text: "Second answer", streaming: false, turnId: "replay-2" },
];
store.current.set("replay");
store.items.set({ replay: items });
store.turns.set({ replay: turns });
store.running.set({ replay: false });

mount(() => (
  <View class="h-full w-full">
    <MessageList store={store} themeStore={themeStore} items={store.items().replay ?? []} />
  </View>
));
