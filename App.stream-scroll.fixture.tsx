import { mount, signal, Text, View } from "nsh";
import { MessageList } from "./app/components/messages";
import type { ChatItem, TurnState } from "./app/protocol";
import { AgentStore } from "./app/store";
import { ThemeStore } from "./app/theme";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turn: TurnState = {
  id: "stream-turn",
  status: "running",
  startedAt: 0,
  durationMs: 0,
  changes: [],
  hooks: [],
};
const history: ChatItem[] = Array.from({ length: 30 }, (_, index) => [
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
const work = signal<ChatItem[]>([]);
const items = (): ChatItem[] => [
  ...history,
  { kind: "user", id: "stream-user", text: "Stream this", files: [], imageNums: [], sentAtMs: 30 },
  ...work(),
];
store.current.set("stream");
store.turns.set({ stream: [turn] });
store.running.set({ stream: true });

let seq = 0;
setInterval(() => {
  seq += 1;
  if (seq % 3 === 1) {
    work.update((previous) => [...previous, {
      kind: "reasoning",
      id: `stream-reason-${seq}`,
      text: `Thinking ${seq}: checking the next step of the task.`,
      streaming: false,
      turnId: turn.id,
    }]);
    return;
  }
  if (seq % 3 === 2) {
    work.update((previous) => [...previous, {
      kind: "tool",
      id: `stream-tool-${seq}`,
      tool: "Bash",
      summary: `echo step-${seq}`,
      detail: "{}",
      output: `step-${seq}`,
      status: "running",
      turnId: turn.id,
    }]);
    return;
  }
  work.update((previous) => [...previous, {
    kind: "markdown",
    id: `stream-answer-${seq}`,
    text: `Streaming answer ${seq}: more output arrives while the list is scrolled up.`,
    streaming: false,
    turnId: turn.id,
  }]);
}, 220);

const pos = signal("");

mount(() => (
  <View class="h-full w-full">
    <MessageList
      store={store}
      themeStore={themeStore}
      items={items()}
      onScroll={(event) => {
        const distance = event.contentSize.height - event.offset.y - event.size.height;
        pos.set(`POSITION_${Math.round(distance)}_offset_${Math.round(event.offset.y)}_content_${Math.round(event.contentSize.height)}_view_${Math.round(event.size.height)}`);
      }}
    />
    <View class="h-px overflow-hidden"><Text style={{ numberOfLines: 1, fontSize: 1, lineHeight: 1 }}>{pos()}</Text></View>
  </View>
));
