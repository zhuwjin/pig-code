import { mount, Text, View } from "nsh";
import { Button } from "nsh/base";
import { MessageList } from "./app/components/messages";
import { AgentStore } from "./app/store";
import { ThemeStore } from "./app/theme";
import type { ChatItem, TurnState } from "./app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const turn: TurnState = {
  id: "turn-code-view",
  status: "done",
  startedAt: 0,
  durationMs: 1000,
  changes: [],
  hooks: [],
};

function readOutput(count: number): string {
  return Array.from({ length: count }, (_, index) => {
    const line = index + 1;
    if (line % 10 === 1) return `${line}\t# Plugin system ${line}`;
    if (line % 10 === 2 || line % 10 === 4 || line % 10 === 6) return `${line}\t`;
    if (line % 10 === 3) return `${line}\tPig Code installs signed NSH packages without rebuilding or restarting the app.`;
    if (line % 10 === 5) return `${line}\t## Scope ${line}`;
    if (line % 10 === 7) return `${line}\t- Agent tools, including model- and session-aware tool selection;`;
    if (line % 10 === 8) return `${line}\t- Commands invoked from the host's command search;`;
    if (line % 10 === 9) return `${line}\t- Lifecycle hooks (\`SessionStart\`, \`PermissionRequest\`, \`PostToolUse\`);`;
    return `${line}\t- HTTP origins and credential declarations;`;
  }).join("\n");
}

const items: ChatItem[] = [
  { kind: "user", id: "user-code-view", text: "Compare the virtual code view", files: [], imageNums: [], sentAtMs: 1 },
  {
    kind: "tool",
    id: "read",
    tool: "Read",
    summary: "PLUGIN_SYSTEM.md",
    detail: "{}",
    output: readOutput(160),
    status: "done",
    turnId: turn.id,
  },
  {
    kind: "tool",
    id: "bash",
    tool: "Bash",
    summary: "ls -la",
    detail: "{}",
    output: ["total 744", "drwxr-xr-x@ 41 jin staff 1312 Nov 7 21:31 .", "drwxr-xr-x 8 jin staff 256 Nov 1 18:46 .."].join("\n"),
    status: "done",
    turnId: turn.id,
  },
  { kind: "markdown", id: "final-code-view", text: "Final answer", streaming: false, turnId: turn.id },
];
store.current.set("code-view");
store.items.set({ "code-view": items });
store.turns.set({ "code-view": [turn] });
store.expandedTurns.set({ [turn.id]: true });
store.running.set({ "code-view": false });

mount(() => (
  <View class="h-full w-full">
    <View class="flex-row gap-2 border-b border-border p-2">
      <Button testID="open-read" class="rounded border border-border px-3 py-1" onPress={() => store.toggleExpanded("read", true)}>
        <Text class="text-xs text-foreground">Toggle Read</Text>
      </Button>
      <Button testID="open-bash" class="rounded border border-border px-3 py-1" onPress={() => store.toggleExpanded("bash", true)}>
        <Text class="text-xs text-foreground">Toggle Bash</Text>
      </Button>
    </View>
    <MessageList store={store} themeStore={themeStore} items={store.items()["code-view"] ?? []} />
  </View>
));
