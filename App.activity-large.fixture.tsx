import { mount, View } from "nsh";
import { MessageList } from "./app/components/messages";
import { AgentStore } from "./app/store";
import { ThemeStore } from "./app/theme";
import type { ChatItem, TurnState } from "./app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turn: TurnState = {
  id: "turn-1",
  status: "done",
  startedAt: 0,
  durationMs: 1000,
  changes: [],
  hooks: [],
};
const items: ChatItem[] = [
  { kind: "user", id: "user-1", text: "Inspect activity", files: [], imageNums: [], sentAtMs: 1 },
  ...Array.from({ length: 80 }, (_, index): ChatItem => ({
    kind: "tool",
    id: `tool-${index}`,
    tool: index % 3 === 0 ? "Bash" : "Read",
    summary: index % 3 === 0 ? "ls -la && echo app" : `crates/pig-core/src/module-${index}.rs`,
    detail: "{}",
    output: "1\ttext",
    status: "done",
    turnId: turn.id,
  })),
  { kind: "markdown", id: "final", text: "Final answer", streaming: false, turnId: turn.id },
];
store.current.set("activity-large");
store.items.set({ "activity-large": items });
store.turns.set({ "activity-large": [turn] });
store.expandedTurns.set({ [turn.id]: true });
store.running.set({ "activity-large": false });

mount(() => (
  <View class="h-full w-full">
    <MessageList store={store} themeStore={themeStore} items={store.items()["activity-large"] ?? []} />
  </View>
));
