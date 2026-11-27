import { Accessor, Clipboard, List, Text, token, View } from "nsh";
import { Button, ContextMenu, DiffView, MenuItem, TextView, Tooltip } from "nsh/base";
import { t } from "../i18n";
import type { AgentCard, ToolItem } from "../protocol";
import { toggleExpanded, type UiContext } from "../ui-context";
import {
  isExecTool,
  parseReadOutput,
  parseSearchOutput,
  partialJsonField,
  pathParts,
  readFirstLine,
  readLineCount,
  streamingDiffRows,
  toolIcon,
  toolLabel,
} from "../conversation";
import { Icon } from "./icon";
import { CodeView, type CodeViewLine } from "./code-view";
import { ShimmerText } from "./shimmer-text";

type ToolContext = UiContext & { activityPreview?: boolean };

function copyText(props: UiContext, id: string, text: string): void {
  void Clipboard.writeText(text).then(() => props.store.markCopied(id));
}

export function CodeCard(props: UiContext & {
  id: string;
  title: string;
  code: () => string;
  lines?: () => CodeViewLine[];
  language: string;
  lineNumbers?: boolean;
  maxHeight?: number;
  fill?: boolean;
  error?: boolean;
}) {
  const wrap = () => props.store.isExpanded(`wrap-${props.id}`);
  return (
    <View class={`overflow-hidden rounded-xl border border-border bg-tool-surface ${props.fill ? "min-h-0 flex-1" : ""}`}>
      <View class="h-6 shrink-0 flex-row items-center gap-2 border-b border-border px-3">
        <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-xs text-muted-foreground">{props.title}</Text>
        <Button
          testID={`wrap-${props.id}`}
          accessibilityLabel={t(wrap() ? "tools.wrap_off" : "tools.wrap_on")}
          class="h-5 w-5 items-center justify-center rounded hover:bg-accent"
          onPress={() => toggleExpanded(props, `wrap-${props.id}`)}
        >
          <Icon name="text-wrap" size={12} color={token(wrap() ? "foreground" : "muted-foreground")} />
        </Button>
        <Button
          testID={`copy-${props.id}`}
          accessibilityLabel={t("tools.copy_code")}
          class="h-5 w-5 items-center justify-center rounded hover:bg-accent"
          onPress={() => copyText(props, `copy-${props.id}`, props.code())}
        >
          <Icon
            name={props.store.copiedItems()[`copy-${props.id}`] ? "check" : "copy"}
            size={12}
            color={token(props.store.copiedItems()[`copy-${props.id}`] ? "success" : "muted-foreground")}
          />
        </Button>
      </View>
      <CodeView
        testID={`code-${props.id}`}
        code={props.code}
        lines={props.lines}
        language={props.language}
        wrap={wrap}
        lineNumbers={props.lineNumbers}
        maxHeight={props.maxHeight}
        fill={props.fill}
        error={props.error}
      />
    </View>
  );
}

function ReadBody(props: UiContext & { item: Accessor<ToolItem> }) {
  const parsed = () => parseReadOutput(props.item().output);
  const firstLine = () => props.item().output.split("\n")[0] ?? "";
  return (
    <View class="gap-2">
      <If when={parsed()}>
        {(value) => {
          const start = readFirstLine(firstLine());
          return (
            <CodeCard
              {...props}
              id={props.item().id}
              title={`${pathParts(props.item().summary).name} · ${t("tools.lines", { count: value().lines.length })}${start && start > 1 ? t("tools.from_line", { line: start }) : ""}`}
              code={() => [...value().lines.map((line) => line.text), ...value().notes].join("\n")}
              lines={() => [
                ...value().lines.map((line) => ({ number: line.number, text: line.text })),
                ...value().notes.map((text) => ({ text })),
              ]}
              language={props.item().summary}
              lineNumbers
              maxHeight={320}
            />
          );
        }}
      </If>
      <If when={!parsed()}><GenericBody {...props} /></If>
    </View>
  );
}

function BashBody(props: UiContext & { item: Accessor<ToolItem> }) {
  return (
    <View class="gap-2">
      <CodeCard
        {...props}
        id={`${props.item().id}-command`}
        title={t("tools.command")}
        code={() => props.item().summary || props.item().detail}
        language="bash"
        maxHeight={160}
      />
      <CodeCard
        {...props}
        id={`${props.item().id}-output`}
        title={t("tools.output")}
        code={() => props.item().output || (
          props.item().status === "stopped"
            ? t("tools.stopped")
            : props.item().status === "running"
              ? t("tools.running")
              : t("tools.no_output")
        )}
        language="text"
        maxHeight={320}
        error={props.item().status === "error"}
      />
    </View>
  );
}

function SearchBody(props: UiContext & { item: Accessor<ToolItem> }) {
  const results = () => parseSearchOutput(props.item().tool, props.item().output);
  return (
    <View>
      <If when={results()}>
        {(value) => (
          <View class="max-h-60 overflow-hidden rounded-xl border border-border bg-tool-surface">
            <View style={{ height: Math.max(24, Math.min(value().rows.slice(0, 400).length * 24 + value().notes.length * 20, 240)) }}>
              <List
                class="h-full"
                contentClass="p-1"
                each={value().rows.slice(0, 400)}
                keyExtractor={(row, index) => `${row.path}:${row.line ?? 0}:${index}`}
                estimatedItemSize={24}
                footer={value().notes.length > 0 ? (
                  <View class="gap-0.5 px-2 py-1">
                    <Each of={value().notes} key={(note) => note}>
                      {(note) => <Text class="text-xs text-muted-foreground">{note()}</Text>}
                    </Each>
                  </View>
                ) : undefined}
              >
                {(row) => (
                  <Button
                    class="group flex-row items-center gap-2 rounded px-2 py-0.5"
                    onPress={() => props.store.openFile(row().path, row().line)}
                  >
                    <Text style={{ numberOfLines: 1 }} class="w-56 shrink-0 text-left font-mono text-xs text-foreground group-hover:text-info">
                      {row().path}{row().line ? `:${row().line}` : ""}
                    </Text>
                    <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-left font-mono text-xs text-foreground">
                      {row().content ?? ""}
                    </Text>
                  </Button>
                )}
              </List>
            </View>
          </View>
        )}
      </If>
      <If when={!results()}><GenericBody {...props} /></If>
    </View>
  );
}

function GenericBody(props: UiContext & { item: Accessor<ToolItem> }) {
  const item = () => props.item();
  const showInput = () => item().status === "input" || item().detail.includes("\n") || item().detail.length > 80;
  const code = () => [showInput() ? item().detail : "", item().output].filter(Boolean).join("\n");
  return (
    <View class="max-h-60 overflow-hidden rounded-xl border border-border bg-tool-surface">
      <If when={code()}>
        <CodeView
          code={code}
          language="text"
          wrap={() => false}
          maxHeight={240}
          error={item().status === "error"}
        />
      </If>
      <If when={!code() && item().status !== "input" && item().status !== "running"}>
        <Text class="px-3 py-2 font-mono text-xs text-muted-foreground">
          {item().status === "stopped" ? t("tools.stopped") : t("tools.no_output")}
        </Text>
      </If>
    </View>
  );
}

function StreamingDiffBody(props: UiContext & { item: Accessor<ToolItem> }) {
  const rows = () => streamingDiffRows(props.item().tool, props.item().detail)
    .map((row, index) => ({ ...row, id: index }));
  return (
    <View class="mt-2 max-h-60 overflow-hidden rounded-xl border border-border bg-tool-surface">
      <View style={{ height: Math.max(24, Math.min(rows().length * 24 + 16, 240)) }}>
        <List
          class="h-full"
          contentClass="p-2"
          each={rows()}
          keyExtractor={(row) => String(row.id)}
          estimatedItemSize={24}
        >
          {(row) => (
            <View class={row().kind === "added" ? "border-l-2 border-success bg-diff-added px-2 py-0.5" : "border-l-2 border-destructive bg-diff-deleted px-2 py-0.5"}>
              <Text style={{ userSelect: "text" }} class="font-mono text-xs leading-5 text-foreground">{row().text || " "}</Text>
            </View>
          )}
        </List>
      </View>
    </View>
  );
}

function ToolBody(props: UiContext & { item: Accessor<ToolItem> }) {
  const item = () => props.item();
  if ((item().status === "input" || item().status === "running") && (item().tool === "Write" || item().tool === "Edit")) {
    return <StreamingDiffBody {...props} />;
  }
  return (
    <View class="mt-2">
      <If when={item().edit}>
        {(edit) => (
          <View class={props.themeStore.classes("diff.root", "h-80 overflow-hidden rounded-xl border border-border bg-tool-surface")}>
            <DiffView
              patch={edit().unified_diff}
              language={edit().path}
              layout="unified"
              softWrap
              classes={{
                gutter: props.themeStore.classes("diff.gutter", "bg-tool-surface"),
                lineAdded: props.themeStore.classes("diff.line-added", "bg-diff-added"),
                lineDeleted: props.themeStore.classes("diff.line-deleted", "bg-diff-deleted"),
                wordAdded: props.themeStore.classes("diff.word-added", "bg-diff-added-word"),
                wordDeleted: props.themeStore.classes("diff.word-deleted", "bg-diff-deleted-word"),
                indicatorAdded: "bg-success",
                indicatorDeleted: "bg-destructive",
                separator: "bg-muted text-muted-foreground",
                fileHeader: "bg-secondary text-secondary-foreground",
              }}
            />
          </View>
        )}
      </If>
      <If when={!item().edit && item().tool === "Read" && (item().status === "done" || item().status === "error")}>
        <ReadBody {...props} />
      </If>
      <If when={!item().edit && isExecTool(item().tool) && item().status !== "input"}>
        <BashBody {...props} />
      </If>
      <If when={!item().edit && (item().tool === "Glob" || item().tool === "Grep") && item().status === "done"}>
        <SearchBody {...props} />
      </If>
      <If when={
        !item().edit &&
        !(item().tool === "Read" && (item().status === "done" || item().status === "error")) &&
        !(isExecTool(item().tool) && item().status !== "input") &&
        !((item().tool === "Glob" || item().tool === "Grep") && item().status === "done")
      }>
        <GenericBody {...props} />
      </If>
    </View>
  );
}

function AgentCards(props: ToolContext & { item: Accessor<ToolItem> }) {
  const agents = () => props.item().agents ?? [];
  const status = () => props.item().status;
  const complete = () => agents().filter((agent) =>
    agent.finished || (!agent.background && status() !== "input" && status() !== "running")
  ).length;
  const ordered = () => agents().map((agent, index) => ({ agent, index })).sort((a, b) => {
    if (props.item().tool !== "AgentSwarm") return a.index - b.index;
    const aDone = a.agent.finishedSeq !== undefined;
    const bDone = b.agent.finishedSeq !== undefined;
    return Number(bDone) - Number(aDone) || (a.agent.finishedSeq ?? a.index) - (b.agent.finishedSeq ?? b.index);
  });
  const parent = () => {
    const first = agents()[0];
    if (!first) return null;
    return (
      <Button
        testID={`swarm-parent-${props.item().id}`}
        class="w-full flex-row items-center gap-3 rounded-lg border border-border bg-tool-surface p-3 hover:bg-accent"
        onPress={() => toggleExpanded(props, `swarm-${props.item().id}`)}
      >
        <View class="h-10 w-10 shrink-0 items-center justify-center rounded-md bg-accent">
          <Icon name="bot" size={20} color={token("foreground")} />
        </View>
        <View class="min-w-0 flex-1 gap-0.5">
          <Text style={{ numberOfLines: 1 }} class="text-left text-sm font-medium text-foreground">
            {first.description || props.item().summary}
          </Text>
          <Text style={{ numberOfLines: 1 }} class="text-left text-xs text-muted-foreground">
            {first.model || first.profile}
          </Text>
        </View>
        <Text class="shrink-0 text-sm text-info">{t("tools.agents_complete", { done: complete(), total: agents().length })}</Text>
      </Button>
    );
  };
  const rows = () => (
    <View class="gap-1">
      <Each of={ordered()} key={(entry) => entry.agent.agent_id}>
        {(entry) => (
          <AgentRow
            {...props}
            agent={() => entry().agent}
            status={status()}
            approvalPending={props.item().approvalPending ?? false}
          />
        )}
      </Each>
    </View>
  );
  if (props.item().tool === "AgentSwarm") {
    const open = () => !props.activityPreview && props.store.isExpanded(`swarm-${props.item().id}`);
    return (
      <View class="w-full">
        <View
          role="button"
          testID={`swarm-${props.item().id}`}
          accessibilityState={{ expanded: open() }}
          class="w-full min-w-0 flex-row items-center gap-2 py-1"
          onPress={() => toggleExpanded(props, `swarm-${props.item().id}`)}
        >
          <Icon name="bot" size={16} color={token("muted-foreground")} />
          <Text class="shrink-0 text-sm font-medium text-muted-foreground">Swarm</Text>
          <Text style={{ numberOfLines: 1 }} class="min-w-0 shrink text-sm text-muted-foreground">{props.item().summary}</Text>
          <Text class="shrink-0 text-xs text-info">{t("tools.agents_complete", { done: complete(), total: agents().length })}</Text>
          <Icon name={open() ? "chevron-down" : "chevron-right"} size={14} color={token("muted-foreground")} />
        </View>
        <If when={open()}>
          <View class="gap-1 pt-1">
            {parent()}
            <List
              viewport="parent"
              class="w-full"
              contentClass="gap-1"
              each={ordered()}
              keyExtractor={(entry) => entry.agent.agent_id}
              estimatedItemSize={80}
            >
              {(entry, index) => (
                <AgentRow
                  {...props}
                  agent={() => entry().agent}
                  status={status()}
                  approvalPending={props.item().approvalPending ?? false}
                  index={index() + 1}
                />
              )}
            </List>
          </View>
        </If>
      </View>
    );
  }
  return rows();
}

function AgentRow(props: UiContext & {
  agent: Accessor<AgentCard>;
  status: ToolItem["status"];
  approvalPending: boolean;
  index?: number;
}) {
  const agent = () => props.agent();
  const failed = () => props.status === "error";
  const done = () => agent().finished || (!agent().background && props.status !== "input" && props.status !== "running");
  const waiting = () => !agent().background && props.approvalPending;
  const running = () => agent().background
    ? !agent().finished
    : (props.status === "input" || props.status === "running") && !agent().finished && !props.approvalPending;
  return (
    <View class="overflow-hidden rounded-xl border border-border bg-tool-surface">
      <Button
        testID={`agent-${agent().agent_id}`}
        class="flex-row items-center gap-3 px-3 py-3 hover:bg-accent"
        onPress={() => props.store.openSubagent(agent().agent_id)}
      >
        <View class="h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-info/10">
          <Icon name="bot" size={18} color={token("info")} />
        </View>
        <View class="min-w-0 flex-1 gap-0.5">
          <Text style={{ numberOfLines: 1 }} class="text-left text-sm font-medium text-foreground">{agent().description}</Text>
          <Text style={{ numberOfLines: 1 }} class={failed() ? "text-left text-xs text-destructive" : "text-left text-xs text-muted-foreground"}>
            {failed() ? t("tools.failed") : `${agent().profile} · ${agent().model}`}
          </Text>
          <If when={running() && agent().live_note}>
            <View class="flex-row items-center gap-1">
              <Icon name="loader-circle" size={12} class="animate-spin" color={token("muted-foreground")} />
              <Text style={{ numberOfLines: 1 }} class="min-w-0 text-xs text-muted-foreground">{agent().live_note}</Text>
            </View>
          </If>
        </View>
        <If when={waiting()}>
          <Text testID={`agent-waiting-${agent().agent_id}`} class="shrink-0 text-xs text-warning">{t("tools.waiting_approval")}</Text>
        </If>
        <If when={!waiting() && running()}>
          <Icon name="loader-circle" size={14} class="animate-spin" color={token("muted-foreground")} />
        </If>
        <If when={!waiting() && !running() && failed()}>
          <Text testID={`agent-failed-${agent().agent_id}`} class="shrink-0 text-xs text-destructive">{t("tools.failed")}</Text>
        </If>
        <If when={!waiting() && !running() && !failed() && done()}>
          <Icon testID={`agent-complete-${agent().agent_id}`} name="circle-check" size={16} color={token("success")} />
        </If>
        <If when={props.index !== undefined}>
          <Text class="w-6 shrink-0 text-right text-xs text-muted-foreground">{String(props.index).padStart(2, "0")}</Text>
        </If>
      </Button>
    </View>
  );
}

function PlanCard(props: ToolContext & { item: Accessor<ToolItem> }) {
  const item = () => props.item();
  const open = () => !props.activityPreview && props.store.isExpanded(`plan-${item().id}`);
  const approved = () => item().output.includes("Plan approved");
  const statusLabel = () => item().status !== "done"
    ? t("tools.plan_pending")
    : approved() ? t("tools.plan_approved") : t("tools.plan_rejected");
  const statusColor = () => token(item().status !== "done" ? "warning" : approved() ? "success" : "destructive");
  const plan = () => {
    try {
      const detail = JSON.parse(item().detail) as { plan?: string };
      return detail.plan || item().output || item().detail;
    } catch {
      return item().output || item().detail;
    }
  };
  return (
    <View class="rounded-xl border border-border bg-tool-surface">
      <View
        role="button"
        testID={`plan-${item().id}`}
        accessibilityState={{ expanded: open() }}
        class="flex-row items-center gap-2 px-3 py-2"
        onPress={() => toggleExpanded(props, `plan-${item().id}`)}
      >
        <Icon name="lightbulb" size={16} color={token("info")} />
        <Text class="shrink-0 text-sm text-muted-foreground">{t("tools.exit_plan")}</Text>
        <Text class="min-w-0 text-sm" style={{ color: statusColor() }}>{statusLabel()}</Text>
        <Icon name={open() ? "chevron-down" : "chevron-right"} size={14} color={token("muted-foreground")} />
      </View>
      <If when={open()}>
        <View class="border-t border-border p-3">
          <TextView markdown={plan()} selectable classes={{ root: "text-sm leading-6 text-foreground" }} />
        </View>
      </If>
    </View>
  );
}

export function ToolRow(props: ToolContext & { item: Accessor<ToolItem> }) {
  const item = () => props.item();
  const open = () => !props.activityPreview && props.store.isExpanded(item().id);
  const running = () => (item().status === "input" || item().status === "running") && !item().approvalPending;
  const streamRows = () => streamingDiffRows(item().tool, item().detail);
  const streamAdditions = () => streamRows().filter((row) => row.kind === "added").length;
  const streamDeletions = () => streamRows().filter((row) => row.kind === "deleted").length;
  const taskWaitIds = (): string[] | null => {
    const parse = (source: string): string[] | null => {
      try {
        const args = JSON.parse(source) as { task_ids?: unknown };
        if (!Array.isArray(args.task_ids)) return [];
        return args.task_ids
          .filter((id): id is string => typeof id === "string")
          .map((id) => id.trim())
          .filter(Boolean);
      } catch {
        return null;
      }
    };
    const fromDetail = parse(item().detail);
    if (fromDetail?.length) return fromDetail;
    return parse(item().summary) ?? fromDetail;
  };
  const summary = () => {
    if (item().tool === "TaskWait") {
      if (item().status === "input") return "…";
      const ids = taskWaitIds();
      if (ids === null) return item().summary.split(/\s+/).join(" ");
      return ids.length > 0 ? ids.join(", ") : t("tools.wait_any");
    }
    if (item().summary) return item().summary.split(/\s+/).join(" ");
    if ((item().tool === "Write" || item().tool === "Edit") && !item().summary) {
      const path = partialJsonField(item().detail, "path");
      if (path) return path;
    }
    if (item().status === "input" && !item().detail) return "…";
    return item().detail.split(/\s+/).join(" ") || "…";
  };
  const fileChip = () => {
    if (item().edit) return pathParts(summary());
    if (running() && (item().tool === "Write" || item().tool === "Edit")) {
      const path = partialJsonField(item().detail, "path");
      return path ? pathParts(path) : null;
    }
    return null;
  };
  const failureReason = () => {
    const collapsed = item().output.split(/\s+/).join(" ");
    return collapsed.length > 200 ? `${collapsed.slice(0, 199).trimEnd()}…` : collapsed;
  };
  if (item().tool === "ExitPlanMode") return <PlanCard {...props} />;
  return (
    <View class="w-full">
      <If when={(item().agents?.length ?? 0) > 0}>
        <AgentCards {...props} />
      </If>
      <If when={(item().agents?.length ?? 0) === 0}>
        <View class="w-full">
          <View
            role="button"
            testID={`tool-${item().id}`}
            accessibilityState={{ expanded: open() }}
            class="w-full min-w-0 flex-row items-center gap-2 py-1"
            onPress={() => toggleExpanded(props, item().id)}
          >
            <Icon name={toolIcon(item().tool)} size={16} color={token("muted-foreground")} />
            <If when={running()}>
              <ShimmerText active baseOpacity={0.6} class="shrink-0" textClass="text-sm">
                {toolLabel(item().tool, true)}
              </ShimmerText>
            </If>
            <If when={!running()}>
              <Text class="shrink-0 text-sm font-medium text-muted-foreground">
                {toolLabel(item().tool, false)}
              </Text>
            </If>
            <If when={item().tool === "Read"}>
              <ContextMenu
                class="min-w-0 shrink"
                popupClass="w-48 rounded-xl border border-border bg-popover p-1 shadow-xl"
                trigger={
                  <Button
                    testID="read-path"
                    class="w-full min-w-0"
                    onPress={() => props.store.openFile(item().summary, readFirstLine(item().output))}
                  >
                    <Text style={{ numberOfLines: 1 }} class="text-left text-sm text-muted-foreground hover:text-foreground hover:underline">
                      {summary()}
                    </Text>
                  </Button>
                }
              >
                <MenuItem testID="read-copy-path" class="flex-row items-center gap-2 rounded-(--radius) px-2 py-1.5 hover:bg-accent" onSelect={() => void Clipboard.writeText(item().summary)}>
                  <Icon name="copy" size={13} color={token("muted-foreground")} />
                  <Text class="flex-1 text-sm text-foreground">{t("sidebar.copy_path")}</Text>
                </MenuItem>
                <MenuItem testID="read-open-right" class="flex-row items-center gap-2 rounded-(--radius) px-2 py-1.5 hover:bg-accent" onSelect={() => props.store.openFile(item().summary, readFirstLine(item().output))}>
                  <Icon name="eye" size={13} color={token("muted-foreground")} />
                  <Text class="flex-1 text-sm text-foreground">{t("tools.open_file_right")}</Text>
                </MenuItem>
              </ContextMenu>
              <If when={readLineCount(item().output) > 0}>
                <Text class="shrink-0 text-xs text-muted-foreground">{t("tools.lines", { count: readLineCount(item().output) })}</Text>
              </If>
            </If>
            <If when={item().tool !== "Read"}>
              <If when={fileChip()}>
                {(chip) => (
                  <View class="min-w-0 shrink flex-row items-center gap-1">
                    <Text class="shrink-0 text-sm text-foreground">{chip().name}</Text>
                    <If when={chip().directory}>
                      <Text style={{ numberOfLines: 1 }} class="min-w-0 shrink text-sm text-muted-foreground">{chip().directory}</Text>
                    </If>
                  </View>
                )}
              </If>
              <If when={!fileChip()}>
                <Text style={{ numberOfLines: 1 }} class="min-w-0 shrink text-sm text-muted-foreground">{summary()}</Text>
              </If>
            </If>
            <If when={item().edit}>
              <Text class="shrink-0 font-mono text-xs text-success">+{item().edit!.additions}</Text>
              <Text class="shrink-0 font-mono text-xs text-destructive">-{item().edit!.deletions}</Text>
            </If>
            <If when={!item().edit && running() && streamAdditions() + streamDeletions() > 0}>
              <If when={streamAdditions() > 0}>
                <Text class="shrink-0 font-mono text-xs text-success">+{streamAdditions()}</Text>
              </If>
              <If when={streamDeletions() > 0}>
                <Text class="shrink-0 font-mono text-xs text-destructive">-{streamDeletions()}</Text>
              </If>
            </If>
            <If when={item().approvalPending}>
              <Text class="shrink-0 text-xs text-warning">{t("tools.waiting_approval")}</Text>
            </If>
            <If when={item().status === "error"}>
              <View
                testID={`tool-error-${item().id}`}
                class="h-5 w-5 shrink-0 items-center justify-center"
                hitSlop={4}
                tooltip={
                  <Tooltip delay={300} class="max-w-80 rounded-md border border-border bg-popover px-2 py-1 shadow-lg">
                    <Text class="text-xs text-foreground">{failureReason()}</Text>
                  </Tooltip>
                }
              >
                <Icon name="x" size={13} color={token("destructive")} />
              </View>
            </If>
            <Icon name={open() ? "chevron-down" : "chevron-right"} size={14} color={token("muted-foreground")} />
          </View>
          <If when={item().liveNote && running()}>
            <View class="flex-row items-center gap-2 pl-6">
              <Icon name="loader-circle" size={12} class="animate-spin" color={token("muted-foreground")} />
              <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-xs text-muted-foreground">{item().liveNote}</Text>
            </View>
          </If>
          <If when={open()}>
            <ToolBody {...props} />
          </If>
        </View>
      </If>
    </View>
  );
}
