import { mount, View } from "nsh";
import { MessageList } from "../app/components/messages";
import type { ChatItem, TurnState } from "../app/protocol";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turn: TurnState = {
  id: "short-turn",
  status: "done",
  startedAt: 0,
  durationMs: 1000,
  changes: [],
  hooks: [],
};
const items: ChatItem[] = [
  { kind: "user", id: "short-user", text: "Collapse this turn", files: [], imageNums: [], sentAtMs: 0 },
  ...Array.from({ length: 16 }, (_, index): ChatItem => ({
    kind: "tool",
    id: `tool-${index}`,
    tool: "Read",
    summary: `crates/pig-core/src/module-${index}.rs`,
    detail: "{}",
    output: "1\ttext",
    status: "done",
    turnId: turn.id,
  })),
  { kind: "markdown", id: "short-final", text: "Short final answer", streaming: false, turnId: turn.id },
];
store.current.set("follow-short");
store.items.set({ "follow-short": items });
store.turns.set({ "follow-short": [turn] });
store.running.set({ "follow-short": false });
store.expandedTurns.set({ [turn.id]: true });
store.expandedItems.set({ "activity-tool-0": true });

mount(() => (
  <View class="h-full w-full">
    <MessageList store={store} themeStore={themeStore} items={store.items()["follow-short"] ?? []} />
  </View>
));
