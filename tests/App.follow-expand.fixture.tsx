import { mount, View } from "nsh";
import { MessageList } from "../app/components/messages";
import type { ChatItem, TurnState } from "../app/protocol";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turn: TurnState = {
  id: "matrix-turn",
  status: "done",
  startedAt: 0,
  durationMs: 1000,
  changes: [],
  hooks: [],
};
const history: ChatItem[] = Array.from({ length: 12 }, (_, index) => [
  {
    kind: "user" as const,
    id: `history-user-${index}`,
    text: `History question ${index}`,
    files: [],
    imageNums: [],
    sentAtMs: index,
  },
  {
    kind: "markdown" as const,
    id: `history-answer-${index}`,
    text: `History answer ${index}`,
    streaming: false,
    turnId: `history-turn-${index}`,
  },
]).flat();
const items: ChatItem[] = [
  ...history,
  { kind: "user", id: "matrix-user", text: "Expand this turn", files: [], imageNums: [], sentAtMs: 12 },
  ...Array.from({ length: 40 }, (_, index): ChatItem => ({
    kind: "tool",
    id: `tool-${index}`,
    tool: "Read",
    summary: `crates/pig-core/src/module-${index}.rs`,
    detail: "{}",
    output: "1\ttext",
    status: "done",
    turnId: turn.id,
  })),
  { kind: "markdown", id: "matrix-final", text: "Matrix final answer", streaming: false, turnId: turn.id },
];
store.current.set("follow-expand");
store.items.set({ "follow-expand": items });
store.turns.set({ "follow-expand": [turn] });
store.running.set({ "follow-expand": false });

mount(() => (
  <View class="h-full w-full">
    <MessageList store={store} themeStore={themeStore} items={store.items()["follow-expand"] ?? []} />
  </View>
));
