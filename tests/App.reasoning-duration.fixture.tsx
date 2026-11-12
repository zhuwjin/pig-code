import { mount, Text, View } from "nsh";
import { Button } from "nsh/base";
import { MessageList } from "../app/components/messages";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const reduce = (event: Record<string, unknown>) => {
  (store as unknown as { reduce(event: Record<string, unknown>): void }).reduce(event);
};

store.current.set("durations");
store.expandedTurns.set({ turn: true });
store.expandedItems.set({ "activity-reason-1": true });
reduce({ TurnStarted: { session_id: "durations", turn_id: "turn" } });
reduce({ ReasoningDelta: { session_id: "durations", item_id: "reason-1", delta: "First reasoning segment" } });

mount(() => (
  <View class="h-full w-full">
    <View class="flex-row flex-wrap gap-2 border-b border-border p-2">
      <Button testID="first-tool" class="rounded border border-border px-3 py-1" onPress={() => {
        reduce({ ToolInputStart: { session_id: "durations", item_id: "tool", tool: "Read" } });
        reduce({ ToolCallBegin: { session_id: "durations", item_id: "tool", tool: "Read", input_summary: "README.md", detail: "{}" } });
        reduce({ ToolCallEnd: { session_id: "durations", item_id: "tool", output: "1\tReadme", is_error: false, stopped: false } });
      }}>
        <Text class="text-xs text-foreground">First tool</Text>
      </Button>
      <Button testID="second-reasoning" class="rounded border border-border px-3 py-1" onPress={() => {
        reduce({ ReasoningDelta: { session_id: "durations", item_id: "reason-2", delta: "Second reasoning segment" } });
      }}>
        <Text class="text-xs text-foreground">Second reasoning</Text>
      </Button>
      <Button testID="answer" class="rounded border border-border px-3 py-1" onPress={() => {
        reduce({ TextDelta: { session_id: "durations", item_id: "answer", delta: "Final answer" } });
        reduce({ TextDone: { session_id: "durations", item_id: "answer", full_text: "Final answer" } });
      }}>
        <Text class="text-xs text-foreground">Answer</Text>
      </Button>
      <Button testID="complete" class="rounded border border-border px-3 py-1" onPress={() => {
        reduce({ TurnComplete: {
          session_id: "durations",
          duration_ms: 5000,
          stats: {
            input: 100,
            cache_read: 0,
            output: 50,
            duration_ms: 5000,
            api_ms: 4500,
            ttft_ms: 100,
            ttft_steps: 1,
            api_steps: 2,
            tool_ms: 500,
            ended_at_ms: 5000,
          },
          finish_reason: "stop",
        } });
      }}>
        <Text class="text-xs text-foreground">Complete</Text>
      </Button>
    </View>
    <MessageList store={store} themeStore={themeStore} items={store.items().durations ?? []} />
  </View>
));
