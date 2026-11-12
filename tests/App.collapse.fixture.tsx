import { mount, View } from "nsh";
import { MessageList } from "../app/components/messages";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";
import type { ChatItem, TurnState } from "../app/protocol";

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
const work: ChatItem[] = [
  {
    kind: "reasoning",
    id: "thinking",
    text: [
      "Analyze the project structure.",
      ...Array.from({ length: 35 }, (_, index) => `Reasoning section ${index + 1}: inspect the repository structure and compare the native list layout with the Gallery transition.`),
    ].join("\n\n"),
    streaming: false,
    turnId: turn.id,
  },
  { kind: "markdown", id: "intro", text: "Inspect the project structure and important configuration files.", streaming: false, turnId: turn.id },
  ...Array.from({ length: 22 }, (_, index): ChatItem => ({
    kind: "tool",
    id: `tool-${index}`,
    tool: index % 3 === 0 ? "Bash" : "Read",
    summary: index % 3 === 0 ? "ls -la && echo app" : `crates/pig-core/src/module-${index}.rs`,
    detail: "{}",
    output: "1\ttext",
    status: "done",
    turnId: turn.id,
  })),
];
const finalText = [
  "## 一句话概括",
  "This is the final answer after a long completed turn with many tool calls.",
  ...Array.from({ length: 20 }, (_, index) => `Section ${index + 1}: the final answer stays readable while the large work region and individual tool cards open and close.`),
  "## 当前状态",
  "The message list uses the same measured window height during the reveal and at the final position.",
].join("\n\n");
const items: ChatItem[] = [
  { kind: "user", id: "user-1", text: "Explain this project", files: [], imageNums: [], sentAtMs: 1 },
  ...work,
  { kind: "markdown", id: "final", text: finalText, streaming: false, turnId: turn.id },
];
store.current.set("collapse");
store.items.set({ collapse: items });
store.turns.set({ collapse: [turn] });
store.expandedTurns.set({ [turn.id]: true });
store.running.set({ collapse: false });

mount(() => (
  <View class="h-full w-full">
    <MessageList store={store} themeStore={themeStore} items={store.items().collapse ?? []} />
  </View>
));
