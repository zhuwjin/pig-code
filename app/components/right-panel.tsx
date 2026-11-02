import { Scroll, Text, View, effect, signal, token } from "nsh";
import { Button, Collapsible, DiffView, Editor, Tab, Tabs, TextView, type EditorRef } from "nsh/base";
import { t } from "../i18n";
import type { GitFileChange, RightPanelRequest, SubagentItem } from "../protocol";
import type { UiContext } from "../ui-context";
import { formatDuration, pathParts } from "../conversation";
import { Icon } from "./icon";

function tabId(request: RightPanelRequest): string {
  switch (request.kind) {
    case "changes": return "changes";
    case "file": return `file:${request.path}`;
    case "subagent": return `subagent:${request.agentId}`;
    case "task": return `task:${request.task.id}`;
    case "compact": return "compact";
  }
}

function tabTitle(request: RightPanelRequest): string {
  switch (request.kind) {
    case "changes": return t("right_panel.tab.changes");
    case "file": return pathParts(request.path).name;
    case "subagent": return t("right_panel.tab.subagent");
    case "task": return t("right_panel.tab.task");
    case "compact": return t("right_panel.tab.compact");
  }
}

function ChangeSection(props: UiContext & {
  title: string;
  changes: GitFileChange[];
  staged: boolean;
}) {
  return (
    <View class="gap-1">
      <Text class="px-3 pb-1 pt-3 text-xs font-medium text-muted-foreground">{props.title}</Text>
      <Each of={props.changes} key={(change) => `${props.staged}:${change.path}`}>
        {(change) => (
          <Button
            testID={`git-change-${props.staged ? "staged" : "unstaged"}-${change().path}`}
            class="flex-row items-center gap-2 rounded-(--radius) px-3 py-2 hover:bg-accent"
            onPress={() => props.store.openGitDiff(change().path, props.staged)}
          >
            <Text class="w-4 shrink-0 text-xs text-muted-foreground">{change().status}</Text>
            <Text numberOfLines={1} class="min-w-0 flex-1 text-sm text-foreground">{change().path}</Text>
            <Text class="text-xs text-success">+{change().additions}</Text>
            <Text class="text-xs text-destructive">-{change().deletions}</Text>
          </Button>
        )}
      </Each>
    </View>
  );
}

function DiffClasses(props: UiContext) {
  return {
    gutter: props.themeStore.classes("diff.gutter", "bg-tool-surface"),
    lineAdded: props.themeStore.classes("diff.line-added", "bg-diff-added"),
    lineDeleted: props.themeStore.classes("diff.line-deleted", "bg-diff-deleted"),
    wordAdded: props.themeStore.classes("diff.word-added", "bg-diff-added-word"),
    wordDeleted: props.themeStore.classes("diff.word-deleted", "bg-diff-deleted-word"),
    indicatorAdded: "bg-success",
    indicatorDeleted: "bg-destructive",
    separator: "bg-muted text-muted-foreground",
    fileHeader: "bg-secondary text-secondary-foreground",
  };
}

function ChangesContent(props: UiContext) {
  const { store, themeStore } = props;
  effect(() => {
    store.cwd();
    store.closeGitDiff();
    store.refreshGitStatus();
  });
  const status = () => store.gitStatus();
  const diff = () => store.gitDiff();
  return (
    <View class="min-h-0 flex-1">
      <View class="h-9 shrink-0 flex-row items-center gap-2 border-b border-border px-3">
        <Text numberOfLines={1} class="min-w-0 flex-1 text-xs text-muted-foreground">{diff() ? diff()!.path : t("right_panel.file_changes")}</Text>
        <If when={diff()}>
          <Button testID="git-diff-back" class="rounded px-2 py-1 hover:bg-accent" onPress={() => store.closeGitDiff()}>
            <Text class="text-xs text-muted-foreground">{t("common.back")}</Text>
          </Button>
        </If>
        <Button
          testID="right-refresh"
          accessibilityLabel={t("right_panel.refresh_changes")}
          class="h-7 w-7 items-center justify-center rounded hover:bg-accent"
          onPress={() => store.refreshGitStatus()}
        >
          <Icon name="refresh-cw" size={13} color={token("muted-foreground")} />
        </Button>
      </View>
      <If when={diff()}>
        {(value) => (
          <View class="min-h-0 flex-1">
            <If when={value().note}>
              <Text class="px-3 py-2 text-xs text-muted-foreground">
                {t(value().note === "Binary" ? "right_panel.diff.binary" : value().note === "TooLarge" ? "right_panel.diff.too_large" : "right_panel.diff.truncated")}
              </Text>
            </If>
            <View class={themeStore.classes("diff.root", "min-h-0 flex-1")}>
              <DiffView patch={value().diff} language={value().path} layout="unified" softWrap classes={DiffClasses(props)} />
            </View>
          </View>
        )}
      </If>
      <If when={!diff() && !status()}>
        <View class="flex-1 items-center justify-center"><Text class="text-sm text-muted-foreground">{t("right_panel.reading_changes")}</Text></View>
      </If>
      <If when={!diff() && status()?.is_git === false}>
        <View class="flex-1 items-center justify-center px-6">
          <Text class="text-center text-sm text-muted-foreground">{t("right_panel.not_git")}</Text>
        </View>
      </If>
      <If when={!diff() && status()?.is_git}>
        <Scroll class="flex-1" contentClass="p-2">
          <ChangeSection {...props} title={t("right_panel.unstaged")} changes={status()!.unstaged} staged={false} />
          <ChangeSection {...props} title={t("right_panel.staged")} changes={status()!.staged} staged />
          <If when={status()!.unstaged.length + status()!.staged.length === 0}>
            <Text class="px-3 py-8 text-center text-sm text-muted-foreground">{t("right_panel.no_changes")}</Text>
          </If>
        </Scroll>
      </If>
    </View>
  );
}

function FileContent(props: UiContext & { request: Extract<RightPanelRequest, { kind: "file" }> }) {
  const content = signal("");
  const error = signal("");
  const truncated = signal(false);
  let editor: EditorRef | undefined;
  effect(() => {
    props.request.key;
    try {
      const result = props.store.readFile(props.request.path, props.request.cwd);
      content.set(result.content);
      truncated.set(result.truncated);
      error.set("");
      if (props.request.line) setTimeout(() => editor?.scrollToLine(props.request.line!), 50);
    } catch (cause) {
      content.set("");
      error.set(cause instanceof Error ? cause.message : String(cause));
    }
  });
  return (
    <View class="min-h-0 flex-1">
      <View class="h-9 shrink-0 flex-row items-center gap-2 border-b border-border px-3">
        <Text numberOfLines={1} class="min-w-0 flex-1 text-xs text-muted-foreground">{props.request.path}</Text>
        <If when={truncated()}><Text class="text-xs text-warning">{t("right_panel.first_2mb")}</Text></If>
      </View>
      <If when={error()}>
        <View class="flex-1 items-center justify-center p-6"><Text selectable class="text-center text-sm text-destructive">{error()}</Text></View>
      </If>
      <If when={!error()}>
        <Editor
          ref={(node) => { editor = node; }}
          value={content()}
          language={props.request.path}
          readOnly
          softWrap={false}
          lineNumbers
          class="min-h-0 flex-1"
        />
      </If>
    </View>
  );
}

function TaskContent(props: UiContext & { request: Extract<RightPanelRequest, { kind: "task" }> }) {
  const task = () => props.request.task;
  const output = signal(task().output_tail);
  effect(() => {
    output.set(task().output_tail);
    if (!task().spill_path) return;
    try {
      output.set(props.store.readFile(task().spill_path!, props.store.cwd()).content);
    } catch {
      output.set(task().output_tail);
    }
  });
  return (
    <View class="min-h-0 flex-1">
      <View class="gap-1 border-b border-border px-3 py-2">
        <Text numberOfLines={1} class="text-xs font-medium text-foreground">{task().command}</Text>
        <Text class="text-xs text-muted-foreground">
          {task().status} · {formatDuration((task().ended_at_ms ?? Date.now()) - task().started_at_ms)}
          {task().exit_code != null ? t("right_panel.exit_code", { code: task().exit_code ?? 0 }) : ""}
        </Text>
      </View>
      <Editor value={output()} language="text" readOnly softWrap lineNumbers={false} class="min-h-0 flex-1" />
    </View>
  );
}

function SubagentItemRow(props: UiContext & { item: SubagentItem; index: number; agentId: string }) {
  if (props.item.role === "tool") {
    const id = `subagent-${props.agentId}-${props.index}`;
    const open = () => props.store.isExpanded(id);
    return (
      <Collapsible
        open={open()}
        content={
          <View class="ml-6 mt-1 rounded-lg bg-tool-surface p-2">
            <Text selectable class="font-mono text-xs leading-5 text-foreground">{props.item.output || t("tools.no_output")}</Text>
          </View>
        }
      >
        <View role="button" class="flex-row items-center gap-2 py-1" onPress={() => props.store.toggleExpanded(id)}>
          <Icon name="chevron-right" size={12} color={token("muted-foreground")} />
          <Text numberOfLines={1} class="min-w-0 flex-1 text-xs text-muted-foreground">
            {props.item.tool ?? t("right_panel.tool")} · {props.item.text}
          </Text>
        </View>
      </Collapsible>
    );
  }
  return (
    <View class={props.item.role === "user" ? "items-end" : "items-start"}>
      <View class={props.item.role === "user" ? "max-w-[85%] rounded-xl bg-accent px-3 py-2" : "w-full px-1 py-1"}>
        <Text selectable class="text-sm leading-6 text-foreground">{props.item.text}</Text>
      </View>
    </View>
  );
}

function SubagentContent(props: UiContext & { request: Extract<RightPanelRequest, { kind: "subagent" }> }) {
  const key = () => `${props.request.sessionId}:${props.request.agentId}`;
  const history = () => props.store.subagents()[key()];
  return (
    <View class="min-h-0 flex-1">
      <View class="gap-1 border-b border-border px-3 py-2">
        <View class="flex-row items-center gap-2">
          <Text numberOfLines={1} class="min-w-0 flex-1 text-xs font-medium text-foreground">
            {history()?.title ?? t("right_panel.loading_subagent")}
          </Text>
          <If when={history()?.running}>
            <View class="h-1.5 w-1.5 rounded-full bg-success" />
            <Text class="text-xs text-muted-foreground">{t("right_panel.running")}</Text>
          </If>
        </View>
        <Text numberOfLines={1} class="text-xs text-muted-foreground">{history()?.subtitle ?? props.request.agentId}</Text>
      </View>
      <Scroll class="flex-1" contentClass="gap-3 p-3">
        <Each of={history()?.items ?? []} key={(item) => `${item.role}-${item.text}-${item.output ?? ""}`}>
          {(item, index) => <SubagentItemRow {...props} item={item()} index={index()} agentId={props.request.agentId} />}
        </Each>
        <If when={history() && history()!.items.length === 0}>
          <Text class="py-8 text-center text-sm text-muted-foreground">{t("right_panel.no_conversation")}</Text>
        </If>
      </Scroll>
    </View>
  );
}

function CompactContent(props: { text: string }) {
  return (
    <Scroll class="min-h-0 flex-1" contentClass="p-4">
      <TextView markdown={props.text} selectable classes={{ root: "text-sm leading-6 text-foreground" }} />
    </Scroll>
  );
}

function ActiveContent(props: UiContext & { request: RightPanelRequest }) {
  return (
    <View class="min-h-0 flex-1">
      <If when={props.request.kind === "changes"}><ChangesContent {...props} /></If>
      <If when={props.request.kind === "file"}>
        <FileContent {...props} request={props.request as Extract<RightPanelRequest, { kind: "file" }>} />
      </If>
      <If when={props.request.kind === "task"}>
        <TaskContent {...props} request={props.request as Extract<RightPanelRequest, { kind: "task" }>} />
      </If>
      <If when={props.request.kind === "subagent"}>
        <SubagentContent {...props} request={props.request as Extract<RightPanelRequest, { kind: "subagent" }>} />
      </If>
      <If when={props.request.kind === "compact"}>
        <CompactContent text={(props.request as Extract<RightPanelRequest, { kind: "compact" }>).text} />
      </If>
    </View>
  );
}

export function RightPanel(props: UiContext & { onClose: () => void }) {
  const tabs = signal<RightPanelRequest[]>([]);
  const activeKey = signal("");
  let initialized = false;
  effect(() => {
    const incoming = props.store.rightPanelRequest();
    if (!incoming && initialized) return;
    initialized = true;
    const request = incoming ?? { kind: "changes", key: "changes" } as RightPanelRequest;
    const id = tabId(request);
    const existing = tabs().find((tab) => tabId(tab) === id);
    if (!existing) tabs.update((current) => [...current, request]);
    else if (existing.key !== request.key) {
      tabs.update((current) => current.map((tab) => tabId(tab) === id ? request : tab));
    }
    activeKey.set(id);
  });
  const active = () => tabs().find((tab) => tabId(tab) === activeKey()) ?? tabs()[tabs().length - 1];
  const closeTab = (id: string) => {
    props.store.rightPanelRequest.set(null);
    const remaining = tabs().filter((tab) => tabId(tab) !== id);
    tabs.set(remaining);
    if (activeKey() === id) activeKey.set(remaining.length ? tabId(remaining[remaining.length - 1]) : "");
    if (remaining.length === 0) props.onClose();
  };
  return (
    <View testID="right-panel" class="w-full flex-1 bg-background">
      <View class="h-10 shrink-0 flex-row items-center border-b border-border">
        <Scroll horizontal class="min-w-0 flex-1" contentClass="h-10 items-center gap-1 px-1">
          <Tabs class="h-10 flex-row items-center gap-1 border-b border-border">
            <Each of={tabs()} key={(tab) => tabId(tab)}>
              {(tab) => {
                const id = () => tabId(tab());
                return (
                  <Tab
                    testID={`right-tab-${id()}`}
                    selected={activeKey() === id()}
                    class="group h-10 flex-row items-center gap-1 px-2"
                    onPress={() => activeKey.set(id())}
                  >
                    <Text class={activeKey() === id() ? "max-w-32 text-xs text-foreground" : "max-w-32 text-xs text-muted-foreground"} numberOfLines={1}>
                      {tabTitle(tab())}
                    </Text>
                    <Button
                      testID={`right-tab-close-${id()}`}
                      accessibilityLabel={t("right_panel.close_tab", { name: tabTitle(tab()) })}
                      class="h-5 w-5 items-center justify-center rounded opacity-0 hover:bg-accent group-hover:opacity-100"
                      onPress={() => closeTab(id())}
                    >
                      <Icon name="x" size={10} color={token("muted-foreground")} />
                    </Button>
                  </Tab>
                );
              }}
            </Each>
          </Tabs>
        </Scroll>
        <Button
          testID="right-panel-close"
          accessibilityLabel={t("right_panel.close_panel")}
          class="h-7 w-7 shrink-0 items-center justify-center rounded hover:bg-accent"
          onPress={props.onClose}
        >
          <Icon name="x" size={14} color={token("muted-foreground")} />
        </Button>
      </View>
      <If when={active()}>
        {(request) => <ActiveContent {...props} request={request()} />}
      </If>
    </View>
  );
}
