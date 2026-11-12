import { mount, View } from "nsh";
import { ComposerAux } from "../app/components/composer-aux";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";
import type { TaskSummary } from "../app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const now = Date.now();

function task(id: string, kind: TaskSummary["kind"], command: string, status: TaskSummary["status"]): TaskSummary {
  return {
    id,
    kind,
    command,
    status,
    started_at_ms: now - 123_000,
    ended_at_ms: status === "running" || status === "queued" ? null : now,
    output_tail: "",
  };
}

store.current.set("tasks");
store.tasks.set({
  tasks: [
    task("running", "shell", "Start-Sleep -Seconds 120", "running"),
    task("queued", "shell", "npm test", "queued"),
    task("done", "shell", "git status --short | Select-Object -First 3", "completed"),
    task("failed", "shell", "cargo build --release", "failed"),
    task("agent", "subagent", "Review the patch", "completed"),
  ],
});

mount(() => (
  <View class="h-full w-full bg-background p-2">
    <View class="flex-1" />
    <ComposerAux store={store} themeStore={themeStore} />
  </View>
));
