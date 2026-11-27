import { mount, Text, View } from "nsh";
import { Button } from "nsh/base";
import { MessageList } from "../app/components/messages";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";
import type { ChatItem } from "../app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turnId = "turn";
const reduce = (event: Record<string, unknown>) => {
  (store as unknown as { reduce(event: Record<string, unknown>): void }).reduce(event);
};

function updateItems(change: (items: ChatItem[]) => ChatItem[]): void {
  store.items.update((all) => ({ ...all, durations: change(all.durations ?? []) }));
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

store.current.set("durations");
store.expandedTurns.set({ turn: true });
store.expandedItems.set({ "activity-reason-1": true });
reduce({ TurnStarted: { session_id: "durations", turn_id: turnId } });
upsert({
  kind: "reasoning",
  id: "reason-1",
  text: "First reasoning segment",
  streaming: true,
  turnId,
  startedAtMs: Date.now(),
});

mount(() => (
  <View class="h-full w-full">
    <View class="flex-row flex-wrap gap-2 border-b border-border p-2">
      <Button testID="first-tool" class="rounded border border-border px-3 py-1" onPress={() => {
        settleReasoning();
        upsert({
          kind: "tool",
          id: "tool",
          tool: "Read",
          summary: "README.md",
          detail: "{}",
          output: "1\tReadme",
          status: "done",
          turnId,
        });
      }}>
        <Text class="text-xs text-foreground">First tool</Text>
      </Button>
      <Button testID="second-reasoning" class="rounded border border-border px-3 py-1" onPress={() => {
        settleReasoning();
        upsert({
          kind: "reasoning",
          id: "reason-2",
          text: "Second reasoning segment",
          streaming: true,
          turnId,
          startedAtMs: Date.now(),
        });
      }}>
        <Text class="text-xs text-foreground">Second reasoning</Text>
      </Button>
      <Button testID="answer" class="rounded border border-border px-3 py-1" onPress={() => {
        settleReasoning();
        upsert({
          kind: "markdown",
          id: "answer",
          text: "Final answer",
          streaming: false,
          turnId,
        });
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
            user_wait_ms: 0,
            tool_exec_ms: 0,
            tool_wall_ms: 0,
            compact_ms: 0,
            hook_ms: 0,
            review_ms: 0,
            overhead_ms: 0,
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
