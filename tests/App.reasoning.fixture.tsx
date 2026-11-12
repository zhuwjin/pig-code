import { mount, Text, View } from "nsh";
import { Button } from "nsh/base";
import { MessageList } from "../app/components/messages";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";
import type { TurnState } from "../app/protocol";

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

function setText(text: string): void {
  store.items.update((all) => ({
    ...all,
    reasoning: (all.reasoning ?? []).map((item) => item.kind === "reasoning" && item.id === "thinking"
      ? { ...item, text }
      : item),
  }));
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
    : `echo "=== settings-page sections ==="; grep -n "type SettingsPageId\\|id: \\"\\|label" app/components/settings && echo "=== a deliberately long command whose title must stay inside the message viewport ==="`;
  reduce({ ToolInputStart: { session_id: "reasoning", item_id: id, tool } });
  reduce({ ToolCallBegin: { session_id: "reasoning", item_id: id, tool, input_summary: summary, detail: "{}", output: "", is_error: false } });
  if (read) {
    const output = Array.from({ length: 44 }, (_, index) => `${index + 1}\tLine ${index + 1}`).join("\n");
    reduce({ ToolCallEnd: { session_id: "reasoning", item_id: id, output, is_error: false, stopped: false } });
  }
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
        reduce({ ReasoningDelta: { session_id: "reasoning", item_id: `thinking-next-${reasoningSequence}`, delta: `Next reasoning ${reasoningSequence}` } });
      }}>
        <Text class="text-xs text-foreground">Next reasoning</Text>
      </Button>
      <Button testID="late-thinking" class="rounded border border-border px-3 py-1" onPress={() => {
        reduce({ ReasoningDelta: { session_id: "reasoning", item_id: "thinking", delta: "\nLate delta after a tool" } });
      }}>
        <Text class="text-xs text-foreground">Late thinking</Text>
      </Button>
      <Button testID="next-text" class="rounded border border-border px-3 py-1" onPress={() => {
        reduce({ TextDelta: { session_id: "reasoning", item_id: "answer", delta: "Answer body" } });
        reduce({ TextDone: { session_id: "reasoning", item_id: "answer", full_text: "Answer body" } });
      }}>
        <Text class="text-xs text-foreground">Next text</Text>
      </Button>
    </View>
    <MessageList store={store} themeStore={themeStore} items={store.items().reasoning ?? []} />
  </View>
));
