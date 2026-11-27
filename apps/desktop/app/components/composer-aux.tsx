import { signal, Text, token, View, type JSX } from "nsh";
import { Button, Popover } from "nsh/base";
import { t } from "../i18n";
import type { TaskStatus, TaskSummary } from "../protocol";
import type { UiContext } from "../ui-context";
import { changeTotals, formatDuration, taskStatusLabel } from "../conversation";
import { Icon, type IconName } from "./icon";
import { Segmented } from "./segmented";

function taskStatusIcon(status: TaskStatus): { name: IconName; color: string; spin?: boolean } {
  switch (status) {
    case "queued": return { name: "clock", color: token("muted-foreground") };
    case "running": return { name: "loader-circle", color: token("info"), spin: true };
    case "completed": return { name: "circle-check", color: token("success") };
    case "failed": return { name: "circle-x", color: token("destructive") };
    case "timed_out": return { name: "circle-alert", color: token("warning") };
    case "killed": return { name: "ban", color: token("muted-foreground") };
    case "lost": return { name: "circle-alert", color: token("warning") };
  }
}

function TaskPanel(props: UiContext & { kind: "shell" | "subagent" }) {
  const filter = signal<"running" | "finished" | "all">("running");
  const tasks = () => props.store.currentTasks().filter((task) => {
    if (task.kind !== props.kind) return false;
    if (filter() === "all") return true;
    const active = task.status === "queued" || task.status === "running";
    return filter() === "running" ? active : !active;
  });
  return (
    <View class="gap-2">
      <View class="flex-row items-center gap-2">
        <View class="min-w-0 flex-row items-center gap-1.5">
          <Icon name={props.kind === "shell" ? "square-terminal" : "bot"} size={14} color={token("muted-foreground")} />
          <Text style={{ numberOfLines: 1 }} class="text-sm font-semibold text-foreground">
            {t(props.kind === "shell" ? "tasks.background_terminal" : "tasks.background_agent")}
          </Text>
        </View>
        <View class="flex-1" />
        <Segmented
          testID={`task-filter-${props.kind}`}
          value={filter()}
          onChange={(value) => filter.set(value)}
          options={[
            { value: "running", label: t("tasks.filter.running") },
            { value: "finished", label: t("tasks.filter.finished") },
            { value: "all", label: t("tasks.filter.all") },
          ]}
        />
      </View>
      <If when={tasks().length === 0}>
        <Text class="py-4 text-center text-xs text-muted-foreground">{t("tasks.empty")}</Text>
      </If>
      <Each of={tasks()} key={(task) => task.id}>
        {(task) => <TaskRow {...props} task={task} />}
      </Each>
    </View>
  );
}

function TaskRow(props: UiContext & { task: () => TaskSummary }) {
  const task = () => props.task();
  const icon = () => taskStatusIcon(task().status);
  const duration = () => formatDuration((task().ended_at_ms ?? Date.now()) - task().started_at_ms);
  return (
    <Button
      testID={`task-${task().id}`}
      accessibilityLabel={`${task().command}, ${taskStatusLabel(task().status)}, ${duration()}`}
      class="flex-row items-center gap-2 rounded-md px-2 py-1.5 hover:bg-accent"
      onPress={() => props.store.openTask(task())}
    >
      <Icon
        name={icon().name}
        size={14}
        color={icon().color}
        class={icon().spin ? "shrink-0 animate-spin" : "shrink-0"}
      />
      <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-left font-mono text-xs text-foreground">
        {task().command}
      </Text>
      <Text class="text-xs text-muted-foreground" style={{ fontVariant: ["tabular-nums"] }}>
        {duration()}
      </Text>
      <Icon name="chevron-right" size={13} color={token("muted-foreground")} />
    </Button>
  );
}

function AuxPopover(props: UiContext & {
  id: string;
  icon: IconName;
  label: string;
  running?: number;
  content: JSX.Element;
}) {
  const open = () => props.store.isExpanded(`aux-${props.id}`);
  return (
    <Popover
      open={open()}
      onOpenChange={({ open: value }) => {
        if (value !== open()) props.store.toggleExpanded(`aux-${props.id}`);
      }}
      placement="top"
      align="start"
      popupClass="max-h-96 w-96 overflow-hidden rounded-xl border border-border bg-popover p-3 shadow-xl"
      trigger={
        <View role="button" testID={`aux-${props.id}`} class="flex-row items-center gap-1 rounded-full px-3 py-1 hover:bg-accent">
          <Icon name={props.icon} size={15} color={token("muted-foreground")} />
          <Text class="text-sm text-muted-foreground">{props.label}</Text>
          <If when={(props.running ?? 0) > 0}>
            <View class="h-1.5 w-1.5 rounded-full bg-info" />
            <Text class="text-xs text-muted-foreground">{props.running}</Text>
          </If>
        </View>
      }
    >
      {props.content}
    </Popover>
  );
}

export function ComposerAux(props: UiContext) {
  const tasks = () => props.store.currentTasks();
  const todos = () => props.store.currentTodos();
  const changes = () => props.store.currentChanges();
  const shell = () => tasks().filter((task) => task.kind === "shell");
  const agents = () => tasks().filter((task) => task.kind === "subagent");
  const running = (items: TaskSummary[]) => items.filter((task) => task.status === "queued" || task.status === "running").length;
  const done = () => todos().filter((todo) => todo.status === "Done").length;
  const totals = () => changeTotals(changes());
  return (
    <View class="w-full flex-row flex-wrap items-center gap-2">
      <If when={shell().length > 0}>
        <AuxPopover
          {...props}
          id="tasks"
          icon="square-terminal"
          label={t("tasks.background_terminal")}
          running={running(shell())}
          content={<TaskPanel {...props} kind="shell" />}
        />
      </If>
      <If when={agents().length > 0}>
        <AuxPopover
          {...props}
          id="agent-tasks"
          icon="bot"
          label={t("tasks.background_agent")}
          running={running(agents())}
          content={<TaskPanel {...props} kind="subagent" />}
        />
      </If>
      <If when={changes().length > 0}>
        <Button
          testID="aux-changes"
          class="flex-row items-center gap-1 rounded-full px-3 py-1 hover:bg-accent"
          onPress={() => props.store.openChanges()}
        >
          <Icon name="diff" size={15} color={token("muted-foreground")} />
          <Text class="text-sm text-muted-foreground">{t("tasks.changes")}</Text>
          <Text class="font-mono text-xs text-success">+{totals().additions}</Text>
          <Text class="font-mono text-xs text-destructive">-{totals().deletions}</Text>
        </Button>
      </If>
      <If when={todos().length > 0}>
        <AuxPopover
          {...props}
          id="todos"
          icon="list-todo"
          label={t("tasks.todo_count", { done: done(), total: todos().length })}
          content={
            <View class="gap-2">
              <Text class="text-sm font-medium text-foreground">{t("tasks.todos")}</Text>
              <Each of={todos()} key={(todo) => `${todo.content}-${todo.status}`}>
                {(todo) => (
                  <View class="flex-row items-start gap-2">
                    <Icon
                      name={todo().status === "Done" ? "check" : todo().status === "InProgress" ? "loader-circle" : "square"}
                      size={13}
                      color={token(todo().status === "Done" ? "success" : "muted-foreground")}
                    />
                    <Text class={todo().status === "Done" ? "flex-1 text-xs text-muted-foreground line-through" : "flex-1 text-xs text-foreground"}>
                      {todo().content}
                    </Text>
                  </View>
                )}
              </Each>
            </View>
          }
        />
      </If>
    </View>
  );
}
