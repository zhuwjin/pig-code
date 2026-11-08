import { mount, Text, View } from "nsh";
import { Button } from "nsh/base";
import { MessageList } from "./app/components/messages";
import { AgentStore } from "./app/store";
import { ThemeStore } from "./app/theme";
import type { ChatItem, ToolItem, TurnState } from "./app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turn: TurnState = {
  id: "turn-activity",
  status: "running",
  startedAt: 0,
  changes: [],
  hooks: [],
};

function tool(id: string, name: string, status: ToolItem["status"] = "done"): ToolItem {
  return {
    kind: "tool",
    id,
    tool: name,
    summary: name === "Grep" ? "needle" : name === "Read" ? `src/${id}.ts` : id,
    detail: "{}",
    output: name === "Grep" ? "src/a.ts:1:const needle = true;" : name === "Read" ? "1\tone" : "",
    status,
    turnId: turn.id,
  };
}

const items: ChatItem[] = [
  { kind: "user", id: "user-activity", text: "Inspect activity", files: [], imageNums: [], sentAtMs: 1 },
  { kind: "reasoning", id: "r1", text: "Analyze the request", streaming: false, turnId: turn.id },
  tool("grep-1", "Grep"),
  tool("grep-2", "Grep", "error"),
  tool("glob-1", "Glob"),
  tool("bash-1", "Bash"),
  tool("web-1", "WebSearch", "error"),
  tool("read-1", "Read"),
  { kind: "markdown", id: "middle", text: "Middle answer", streaming: false, turnId: turn.id },
  tool("single", "Read"),
  { kind: "markdown", id: "final", text: "Final answer", streaming: false, turnId: turn.id },
];
store.current.set("activity");
store.items.set({ activity: items });
store.turns.set({ activity: [turn] });
store.running.set({ activity: false });

mount(() => (
  <View class="h-full w-full">
    <View class="flex-row gap-2 border-b border-border p-2">
      <Button
        testID="append-tool"
        class="rounded border border-border px-3 py-1"
        onPress={() => {
          store.items.update((all) => ({
            ...all,
            activity: all.activity.flatMap((item) => item.id === "middle" ? [tool("read-2", "Read"), item] : [item]),
          }));
        }}
      >
        <Text class="text-xs text-foreground">Append tool</Text>
      </Button>
    </View>
    <MessageList store={store} themeStore={themeStore} items={store.items().activity ?? []} />
  </View>
));
