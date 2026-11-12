import { mount, signal, Text, View } from "nsh";
import { MessageList } from "../app/components/messages";
import type { ChatItem, TurnState } from "../app/protocol";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turn: TurnState = {
  id: "expand-turn",
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
  { kind: "user", id: "expand-user", text: "Expand this turn", files: [], imageNums: [], sentAtMs: 12 },
  {
    kind: "reasoning",
    id: "expand-reasoning",
    text: "Inspect the scroll position.",
    streaming: false,
    turnId: turn.id,
  },
  {
    kind: "tool",
    id: "expand-tool",
    tool: "Read",
    summary: "app/components/messages.tsx",
    detail: "{}",
    output: "1\ttext",
    status: "done",
    turnId: turn.id,
  },
  { kind: "markdown", id: "expand-final", text: "Final answer", streaming: false, turnId: turn.id },
];
store.current.set("scroll-expand");
store.items.set({ "scroll-expand": items });
store.turns.set({ "scroll-expand": [turn] });
store.running.set({ "scroll-expand": false });

type LatestSample = { visible: boolean; distance: number };
const latestLog = signal("");
const positionLog = signal("");
const latestSamples: LatestSample[] = [];
let lastDistance = 0;

mount(() => (
  <View class="h-full w-full">
    <MessageList
      store={store}
      themeStore={themeStore}
      items={store.items()["scroll-expand"] ?? []}
      onScroll={(event) => {
        lastDistance = event.contentSize.height - event.offset.y - event.size.height;
        positionLog.set(`POSITION_${Math.round(lastDistance)}`);
      }}
      onLatestVisibleChange={(visible) => {
        latestSamples.push({ visible, distance: lastDistance });
        latestLog.set(JSON.stringify(latestSamples));
      }}
    />
    <View class="h-px overflow-hidden"><Text style={{ numberOfLines: 1, fontSize: 1, lineHeight: 1 }}>{`LATEST_LOG${latestLog()}`}</Text></View>
    <View class="h-px overflow-hidden"><Text style={{ numberOfLines: 1, fontSize: 1, lineHeight: 1 }}>{positionLog()}</Text></View>
  </View>
));
