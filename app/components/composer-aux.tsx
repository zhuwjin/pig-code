import { signal, Text, token, View, type JSX } from "nsh";
import { Button, Popover } from "nsh/base";
import { t } from "../i18n";
import type { TaskStatus, TaskSummary } from "../protocol";
import type { UiContext } from "../ui-context";
import { changeTotals, formatDuration } from "../conversation";
import { Icon, type IconName } from "./icon";

function taskStatus(status: TaskStatus): string {
  switch (status) {
    case "queued": return t("tasks.status.queued");
    case "running": return t("tasks.status.running");
    case "completed": return t("tasks.status.completed");
    case "failed": return t("tasks.status.failed");
    case "timed_out": return t("tasks.status.timed_out");
    case "killed": return t("tasks.status.killed");
    case "lost": return t("tasks.status.lost");
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
      <View class="flex-row items-center gap-1">
        <Each of={["running", "finished", "all"] as const} key={(value) => value}>
          {(value) => (
            <Button
              selected={filter() === value()}
              class="rounded-full px-2 py-1 selected:bg-accent"
              onPress={() => filter.set(value())}
            >
              <Text class="text-xs text-muted-foreground">
                {t(value() === "running" ? "tasks.filter.running" : value() === "finished" ? "tasks.filter.finished" : "tasks.filter.all")}
              </Text>
            </Button>
          )}
        </Each>
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
  return (
    <Button
      testID={`task-${task().id}`}
      class="gap-1 rounded-lg px-2 py-2 text-left hover:bg-accent"
      onPress={() => props.store.openTask(task())}
    >
      <View class="flex-row items-center gap-2">
        <Text numberOfLines={1} class="min-w-0 flex-1 text-left text-xs font-medium text-foreground">{task().command}</Text>
        <Text class={task().status === "failed" ? "text-xs text-destructive" : "text-xs text-muted-foreground"}>
          {taskStatus(task().status)}
        </Text>
      </View>
      <Text class="text-left text-xs text-muted-foreground">
        {formatDuration((task().ended_at_ms ?? Date.now()) - task().started_at_ms)}
      </Text>
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
      onOpenChange={(value) => {
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
