import { mount, Text, View } from "nsh";
import { MessageList } from "../app/components/messages";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";
import type { ChatItem, TaskNotification, TaskSummary } from "../app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const session = "notification";

const notifications: TaskNotification[] = [
  {
    id: "note-shell",
    task_id: "task-shell",
    kind: "shell",
    status: "completed",
    severity: "info",
    description: "git status --short | Select-Object -First 3",
    title: "Background shell completed",
    body: "Wall time: 2m 9s\ngit status --short completed.",
    duration_ms: 129_000,
    output: {
      type: "preview",
      text: " M app/components/messages.tsx\n M app/components/composer-aux.tsx",
      total_bytes: 76,
      truncated: false,
    },
  },
  {
    id: "note-agent",
    task_id: "task-agent",
    kind: "subagent",
    status: "completed",
    severity: "info",
    description: "Sleep two minutes",
    title: "Background subagent completed",
    body: "Wall time: 2m 9s\nSubagent general-purpose: Sleep two minutes completed.",
    duration_ms: 129_000,
    turns: 2,
    agent_id: "agent-1",
  },
  {
    id: "note-failed",
    task_id: "task-failed",
    kind: "shell",
    status: "failed",
    severity: "warning",
    description: "cargo build --release",
    title: "Background shell failed",
    body: "Wall time: 8s\ncargo build --release failed.",
    duration_ms: 8000,
    exit_code: 1,
  },
];

const tasks: TaskSummary[] = [
  { id: "task-shell", kind: "shell", command: notifications[0].description, status: "completed", started_at_ms: 0, ended_at_ms: 129_000, output_tail: "" },
  { id: "task-failed", kind: "shell", command: notifications[2].description, status: "failed", started_at_ms: 0, ended_at_ms: 8000, output_tail: "" },
];

const items: ChatItem[] = [
  { kind: "notification", id: "notification-1", notifications, sentAtMs: 0 },
  { kind: "markdown", id: "reply", text: "All checks finished.", streaming: false, turnId: "turn-1" },
];

store.current.set(session);
store.items.set({ [session]: items });
store.tasks.set({ [session]: tasks });

mount(() => (
  <View class="h-[620px] w-[900px] gap-2 bg-background p-4">
    <Text testID="request-kind">{store.rightPanelRequest()?.kind ?? "none"}</Text>
    <MessageList store={store} themeStore={themeStore} items={items} />
  </View>
));
