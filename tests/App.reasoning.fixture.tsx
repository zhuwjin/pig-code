import { mount, Text, View } from "nsh";
import { Button } from "nsh/base";
import { MessageList } from "../app/components/messages";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";
import type { ChatItem, TurnState } from "../app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turn: TurnState = {
  id: "turn",
  status: "running",
  startedAt: Date.now(),
  changes: [],
  hooks: [],
};
const firstLine = "First rolling line with enough content to inspect the reasoning preview";
const appendedLine = `${firstLine} and a longer suffix`;
const fullText = `${appendedLine}\nSecond rolling line`;

function updateItems(change: (items: ChatItem[]) => ChatItem[]): void {
  store.items.update((all) => ({ ...all, reasoning: change(all.reasoning ?? []) }));
}

function upsert(item: ChatItem): void {
  updateItems((items) => [...items.filter((current) => current.id !== item.id), item]);
}

function settleReasoning(): void {
  const now = Date.now();
  updateItems((items) => items.map((item) =>
    item.kind === "reasoning" && item.streaming
      ? {
        ...item,
        streaming: false,
        durationMs: item.startedAtMs !== undefined ? Math.max(0, now - item.startedAtMs) : item.durationMs,
      }
      : item
  ));
}

function setText(text: string): void {
  updateItems((items) => items.map((item) => item.id === "thinking"
    ? { ...item, text }
    : item));
}

const reduce = (event: Record<string, unknown>) => {
  (store as unknown as { reduce(event: Record<string, unknown>): void }).reduce(event);
};
let toolSequence = 0;
let reasoningSequence = 0;

function nextTool(): void {
  toolSequence += 1;
  const id = `tool-${toolSequence}`;
  const read = toolSequence % 2 === 0;
  const tool = read ? "Read" : "Bash";
  const summary = read
    ? "README.md"
    : `echo "=== settings-page sections ==="; grep -n "type SettingsPageId\|id: \\\"\|label" app/components/settings && echo "=== a deliberately long command whose title must stay inside the message viewport ==="`;
  settleReasoning();
  upsert({
    kind: "tool",
    id,
    tool,
    summary,
    detail: "{}",
    output: read ? Array.from({ length: 44 }, (_, index) => `${index + 1}\tLine ${index + 1}`).join("\n") : "",
    status: read ? "done" : "running",
    turnId: turn.id,
  });
}

store.current.set("reasoning");
store.items.set({
  reasoning: [
    { kind: "reasoning", id: "thinking", text: firstLine, streaming: true, turnId: turn.id },
  ],
});
reduce({ TurnStarted: { session_id: "reasoning", turn_id: turn.id } });

mount(() => (
  <View class="h-full w-full">
    <View class="flex-row flex-wrap gap-2 border-b border-border p-2">
      <Button testID="append-thinking" class="rounded border border-border px-3 py-1" onPress={() => setText(appendedLine)}>
        <Text class="text-xs text-foreground">Append thinking</Text>
      </Button>
      <Button testID="next-thinking-line" class="rounded border border-border px-3 py-1" onPress={() => setText(fullText)}>
        <Text class="text-xs text-foreground">Next thinking line</Text>
      </Button>
      <Button testID="next-tool" class="rounded border border-border px-3 py-1" onPress={nextTool}>
        <Text class="text-xs text-foreground">Next tool</Text>
      </Button>
      <Button testID="next-reasoning" class="rounded border border-border px-3 py-1" onPress={() => {
        reasoningSequence += 1;
        settleReasoning();
        upsert({
          kind: "reasoning",
          id: `thinking-next-${reasoningSequence}`,
          text: `Next reasoning ${reasoningSequence}`,
          streaming: true,
          turnId: turn.id,
          startedAtMs: Date.now(),
        });
      }}>
        <Text class="text-xs text-foreground">Next reasoning</Text>
      </Button>
      <Button testID="late-thinking" class="rounded border border-border px-3 py-1" onPress={() => {
        updateItems((items) => items.map((item) => item.id === "thinking"
          ? { ...item, text: `${item.kind === "reasoning" ? item.text : ""}\nLate delta after a tool` }
          : item));
      }}>
        <Text class="text-xs text-foreground">Late thinking</Text>
      </Button>
      <Button testID="next-text" class="rounded border border-border px-3 py-1" onPress={() => {
        settleReasoning();
        upsert({
          kind: "markdown",
          id: "answer",
          text: "Answer body",
          streaming: false,
          turnId: turn.id,
        });
      }}>
        <Text class="text-xs text-foreground">Next text</Text>
      </Button>
    </View>
    <MessageList store={store} themeStore={themeStore} items={store.items().reasoning ?? []} />
  </View>
));
