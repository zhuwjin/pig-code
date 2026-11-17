import { mount, Text, View } from "nsh";
import { Button } from "nsh/base";
import { MessageList } from "../app/components/messages";
import { RightPanel } from "../app/components/right-panel";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";
import type { AgentCard, ChatItem, ToolItem } from "../app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();

function agent(id: string, description: string, finished: boolean, finishedSeq?: number): AgentCard {
  return {
    agent_id: id,
    run_id: `run-${id}`,
    profile: "coder",
    description,
    model: "mock-model",
    background: false,
    finished,
    finishedSeq,
    live_note: finished ? null : "Working",
  };
}

function tool(id: string, name: string, status: ToolItem["status"], extra?: Partial<ToolItem>): ToolItem {
  return {
    kind: "tool",
    id,
    tool: name,
    summary: id,
    detail: "{}",
    output: "",
    status,
    ...extra,
  };
}

const items: ChatItem[] = [
  tool("swarm", "AgentSwarm", "running", {
    summary: "Build the plugin",
    agents: [
      agent("agent-1", "Agent one", true, 2),
      agent("agent-2", "Agent two", false),
      agent("agent-3", "Agent three", true, 1),
      agent("agent-4", "Agent four", false),
    ],
  }),
  tool("late-agent", "Agent", "running", { summary: "late agent" }),
  tool("agent-failed", "Agent", "error", {
    agents: [agent("agent-failed", "Failed agent", true)],
  }),
  tool("agent-pending", "Agent", "running", {
    approvalPending: true,
    agents: [agent("agent-pending", "Pending agent", false)],
  }),
  tool("agent-complete", "Agent", "done", {
    agents: [agent("agent-complete", "Complete agent", true)],
  }),
  tool("plan-approved", "ExitPlanMode", "done", { output: "Plan approved" }),
  tool("plan-rejected", "ExitPlanMode", "done", { output: "Plan rejected" }),
  tool("plan-pending", "ExitPlanMode", "running"),
  { kind: "reasoning", id: "thinking", text: "First rolling line", streaming: true },
  tool("write", "Write", "done", {
    summary: "app/components/settings/model-editor.tsx",
    edit: { path: "app/components/settings/model-editor.tsx", unified_diff: "", additions: 4, deletions: 2 },
  }),
  tool("read", "Read", "done", {
    summary: "README.mock.md",
    output: "1\tone",
  }),
  tool("failed", "Grep", "error", { output: "Search backend timed out" }),
  tool("wait", "TaskWait", "done", {
    summary: '{"task_ids":["b1"],"timeout_ms":30000}',
    detail: '{"task_ids":["b1"],"timeout_ms":30000}',
  }),
];
store.current.set("tool-cards");
store.items.set({ "tool-cards": items });
store.turns.set({ "tool-cards": [] });
store.running.set({ "tool-cards": false });

mount(() => (
  <View class="h-full w-full flex-row">
    <View class="flex-row gap-2 border-b border-border p-2">
      <Button
        testID="advance-thinking"
        class="rounded border border-border px-3 py-1"
        onPress={() => {
          store.items.update((all) => ({
            ...all,
            "tool-cards": all["tool-cards"].map((item) => item.kind === "reasoning" && item.id === "thinking"
              ? { ...item, text: "Second rolling line with a longer suffix" }
              : item),
          }));
        }}
      >
        <Text class="text-xs text-foreground">Advance thinking</Text>
      </Button>
      <Button
        testID="attach-agent"
        class="rounded border border-border px-3 py-1"
        onPress={() => {
          store.items.update((all) => ({
            ...all,
            "tool-cards": all["tool-cards"].map((item) => item.kind === "tool" && item.id === "late-agent"
              ? { ...item, agents: [agent("late-agent-1", "Late agent", false)] }
              : item),
          }));
        }}
      >
        <Text class="text-xs text-foreground">Attach agent</Text>
      </Button>
    </View>
    <View class="min-w-0 flex-1">
      <MessageList store={store} themeStore={themeStore} items={store.items()["tool-cards"] ?? []} />
    </View>
    <If when={store.rightPanelRequest()}>
      <View class="w-[360px]">
        <RightPanel store={store} themeStore={themeStore} onClose={() => store.rightPanelRequest.set(null)} maximized={false} onToggleMaximize={() => {}} />
      </View>
    </If>
  </View>
));
