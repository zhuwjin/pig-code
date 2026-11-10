import { effect, mount, signal, Text, View } from "nsh";
import { MessageList } from "./app/components/messages";
import type { ChatItem, TurnState } from "./app/protocol";
import { AgentStore } from "./app/store";
import { ThemeStore } from "./app/theme";

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

type ScrollSample = { offset: number; max: number; content: number };
type LatestSample = { visible: boolean; distance: number };
const scrollLog = signal("");
const latestLog = signal("");
const positionLog = signal("");
const samples: ScrollSample[] = [];
const latestSamples: LatestSample[] = [];
let lastDistance = 0;
let sawExpansion = false;
let recording = false;
effect(() => {
  const expanded = store.isTurnExpanded(turn.id);
  if (expanded) {
    sawExpansion = true;
    recording = false;
    samples.length = 0;
    scrollLog.set("");
  } else if (sawExpansion) {
    recording = true;
    samples.length = 0;
    scrollLog.set("");
  }
});

mount(() => (
  <View class="h-full w-full">
    <MessageList
      store={store}
      themeStore={themeStore}
      items={store.items()["scroll-expand"] ?? []}
      onScroll={(event) => {
        lastDistance = event.contentSize.height - event.offset.y - event.size.height;
        positionLog.set(`POSITION_${Math.round(lastDistance)}`);
        if (!recording) return;
        samples.push({
          offset: Math.round(event.offset.y * 100) / 100,
          max: Math.round((event.contentSize.height - event.size.height) * 100) / 100,
          content: Math.round(event.contentSize.height * 100) / 100,
        });
        scrollLog.set(`SCROLL_LOG${JSON.stringify(samples)}`);
      }}
      onLatestVisibleChange={(visible) => {
        latestSamples.push({ visible, distance: lastDistance });
        latestLog.set(JSON.stringify(latestSamples));
      }}
    />
    <View class="h-px overflow-hidden"><Text style={{ numberOfLines: 1, fontSize: 1, lineHeight: 1 }}>{scrollLog()}</Text></View>
    <View class="h-px overflow-hidden"><Text style={{ numberOfLines: 1, fontSize: 1, lineHeight: 1 }}>{`LATEST_LOG${latestLog()}`}</Text></View>
    <View class="h-px overflow-hidden"><Text style={{ numberOfLines: 1, fontSize: 1, lineHeight: 1 }}>{positionLog()}</Text></View>
  </View>
));
