import { List, Scroll, Text, View, computed, effect, platform, signal, token, type ListRef } from "nsh";
import { Button, DiffView, Editor, Input, Menu, MenuItem, Popover, Tab, Tabs, TextView, type EditorRef } from "nsh/base";
import { titleBar } from "nsh/desktop";
import { t } from "../i18n";
import type { ChatItem, GitFileChange, RightPanelRequest, SubagentView, TaskSummary, TurnState } from "../protocol";
import { type UiContext } from "../ui-context";
import { conversationRowsEqual, flattenConversationRows, formatDuration, pathParts, taskStatusLabel, turnDurationMs, type ConversationRow } from "../conversation";
import { Icon, type IconName } from "./icon";
import { MessageRow, type MessageProps } from "./messages";
import { CodeCard } from "./tool-card";
import { WINDOW_CONTROLS_WIDTH } from "./window-controls";

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

function tabIcon(request: RightPanelRequest): IconName {
  switch (request.kind) {
    case "changes": return "git-branch";
    case "file": return "file-text";
    case "subagent": return "bot";
    case "task": return "square-terminal";
    case "compact": return "archive";
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
            <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-sm text-foreground">{change().path}</Text>
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
        <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-xs text-muted-foreground">{diff() ? diff()!.path : t("right_panel.file_changes")}</Text>
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
        <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-xs text-muted-foreground">{props.request.path}</Text>
        <If when={truncated()}><Text class="text-xs text-warning">{t("right_panel.first_2mb")}</Text></If>
      </View>
      <If when={error()}>
        <View class="flex-1 items-center justify-center p-6"><Text style={{ userSelect: "text" }} class="text-center text-sm text-destructive">{error()}</Text></View>
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
  const cardsHeight = signal(0);
  const commandMaxHeight = () => cardsHeight() > 0
    ? Math.max(40, Math.floor((cardsHeight() - 8) / 2) - 26)
    : 160;
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
    <View class="min-h-0 flex-1 gap-2 p-3">
      <View class="flex-row items-center gap-2 px-1">
        <Text class="text-xs text-muted-foreground">
          {taskStatusLabel(task().status)} · {formatDuration((task().ended_at_ms ?? Date.now()) - task().started_at_ms)}
          {task().exit_code != null ? t("right_panel.exit_code", { code: task().exit_code ?? 0 }) : ""}
        </Text>
      </View>
      <View class="min-h-0 flex-1 gap-2" onLayout={({ bounds }) => cardsHeight.set(bounds.size.height)}>
        <CodeCard
          {...props}
          id={`task-${task().id}-command`}
          title={t("tools.command")}
          code={() => task().command}
          language="bash"
          maxHeight={commandMaxHeight()}
        />
        <CodeCard
          {...props}
          id={`task-${task().id}-output`}
          title={t("tools.output")}
          code={() => output()}
          language="text"
          fill
        />
      </View>
    </View>
  );
}

function subagentRows(
  agentId: string,
  history: SubagentView | undefined,
  task: TaskSummary | undefined,
  fallbackDuration: number | undefined,
  turnCache: Map<string, TurnState>,
  isTurnExpanded: (id: string) => boolean,
): ConversationRow[] {
  if (!history) return [];
  const items: ChatItem[] = [];
  const turns: TurnState[] = [];
  let turnIndex = -1;
  let currentTurnId: string | undefined;
  let firstUser = true;
  const startTurn = () => {
    turnIndex += 1;
    currentTurnId = `subagent-${agentId}-turn-${turnIndex}`;
    turns.push({
      id: currentTurnId,
      status: "done",
      startedAt: task?.started_at_ms ?? 0,
      changes: [],
      hooks: [],
    });
    return currentTurnId;
  };
  const turnForItem = () => currentTurnId ?? startTurn();
  history.items.forEach((item) => {
    if (item.kind === "user") {
      items.push({
        ...item,
        sentAtMs: item.sentAtMs > 0
          ? item.sentAtMs
          : firstUser
            ? (task?.started_at_ms ?? 0)
            : 0,
      });
      firstUser = false;
      currentTurnId = undefined;
      return;
    }
    const turnId = turnForItem();
    if (item.kind === "markdown" || item.kind === "reasoning") {
      items.push({ ...item, turnId });
    } else if (item.kind === "tool") {
      items.push({ ...item, turnId });
    } else {
      items.push(item);
    }
  });
  if (turns.length > 0) {
    const last = turns[turns.length - 1];
    last.status = history.running ? "running" : "done";
    const duration = task
      ? Math.max(0, (task.ended_at_ms ?? Date.now()) - task.started_at_ms)
      : fallbackDuration;
    last.durationMs = duration;
  }
  // The panel builds its turns itself, so keep one object per unchanged turn
  // instead of making the rows new every time.
  for (let index = 0; index < turns.length; index += 1) {
    const turn = turns[index];
    const cached = turnCache.get(turn.id);
    if (
      cached
      && cached.status === turn.status
      && cached.startedAt === turn.startedAt
      && cached.durationMs === turn.durationMs
    ) {
      turns[index] = cached;
    } else {
      turnCache.set(turn.id, turn);
    }
  }
  return flattenConversationRows(items, turns, isTurnExpanded)
    .filter((row) => row.type !== "actions" && row.type !== "changes");
}

function SubagentContent(props: UiContext & { request: Extract<RightPanelRequest, { kind: "subagent" }> }) {
  const key = () => `${props.request.sessionId}:${props.request.agentId}`;
  const history = () => props.store.subagents()[key()];
  const task = () => (props.store.tasks()[props.request.sessionId] ?? [])
    .find((entry) => entry.kind === "subagent" && entry.agent_id === props.request.agentId);
  const parentTurnDuration = () => {
    const items = props.store.items()[props.request.sessionId] ?? [];
    const item = items.find((entry) => entry.kind === "tool" && entry.agents?.some((agent) => agent.agent_id === props.request.agentId));
    if (!item || item.kind !== "tool" || !item.turnId) return undefined;
    const turn = (props.store.turns()[props.request.sessionId] ?? []).find((entry) => entry.id === item.turnId);
    return turn ? turnDurationMs(turn) : undefined;
  };
  const sessionKey = () => `${props.request.sessionId}:${props.request.agentId}`;
  let cachedTurns = new Map<string, TurnState>();
  let cachedTurnsKey = "";
  const rows = computed(
    () => {
      const key = sessionKey();
      if (key !== cachedTurnsKey) {
        cachedTurns = new Map();
        cachedTurnsKey = key;
      }
      return subagentRows(
        props.request.agentId,
        history(),
        task(),
        parentTurnDuration(),
        cachedTurns,
        (id) => props.store.isTurnExpanded(id),
      );
    },
    { equals: conversationRowsEqual },
  );
  const messageProps: MessageProps = {
    ...props,
    textFor: (item) => item.text,
    onImage: () => {},
  };
  const latestVisible = signal(false);
  let list: ListRef | undefined;
  let followedKey = "";
  effect(() => {
    const key = sessionKey();
    if (key === followedKey) return;
    followedKey = key;
    latestVisible.set(false);
    const timer = setTimeout(() => list?.scrollToEnd({ animated: false }), 40);
    return () => clearTimeout(timer);
  });
  return (
    <View class="relative min-h-0 min-w-0 flex-1">
      <View class="gap-1 border-b border-border px-3 py-2">
        <View class="flex-row items-center gap-2">
          <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-xs font-medium text-foreground">
            {history()?.title ?? t("right_panel.loading_subagent")}
          </Text>
          <If when={history()?.running}>
            <View class="h-1.5 w-1.5 rounded-full bg-success" />
            <Text class="text-xs text-muted-foreground">{t("right_panel.running")}</Text>
          </If>
        </View>
        <Text style={{ numberOfLines: 1 }} class="text-xs text-muted-foreground">{history()?.subtitle ?? props.request.agentId}</Text>
      </View>
      <List
        ref={(node) => { list = node; }}
        testID="subagent-message-list"
        each={rows()}
        keyExtractor={(row) => row.id}
        estimatedItemSize={28}
        class="min-h-0 min-w-0 w-full flex-1"
        contentClass="gap-4"
        alignment="bottom"
        maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 24 }}
        header={<View class="h-4" />}
        empty={<Text class="py-8 text-center text-sm text-muted-foreground">{t("right_panel.no_conversation")}</Text>}
        scrollbar
        onScroll={(event) => {
          const distance = event.contentSize.height - event.offset.y - event.size.height;
          latestVisible.set(distance > 24);
        }}
      >
        {(row) => <MessageRow {...messageProps} row={row} active={false} navEligible={false} flat />}
      </List>
      <If when={latestVisible()}>
        <View class="absolute inset-x-0 bottom-3 items-center" style={{ pointerEvents: "box-none" }}>
          <Button
            testID="subagent-latest"
            class="h-[38px] flex-row items-center gap-2 rounded-full border border-border bg-popover px-4 shadow-lg hover:bg-accent"
            onPress={() => {
              latestVisible.set(false);
              list?.scrollToEnd({ animated: true });
            }}
          >
            <Icon name="arrow-down" size={15} color={token("muted-foreground")} />
            <Text class="text-sm font-medium text-foreground">{t("messages.latest")}</Text>
          </Button>
        </View>
      </If>
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

export function RightPanel(props: UiContext & { onClose: () => void; maximized: boolean; onToggleMaximize: () => void }) {
  const tabs = signal<RightPanelRequest[]>([]);
  const activeKey = signal("");
  const addOpen = signal(false);
  const tabListOpen = signal(false);
  const tabQuery = signal("");
  let initialized = false;
  effect(() => {
    const incoming = props.store.rightPanelRequest();
    if (!incoming && initialized) return;
    initialized = true;
    const request = incoming ?? { kind: "changes", key: "changes" } as RightPanelRequest;
    const id = tabId(request);
    const existing = tabs().find((tab) => tabId(tab) === id);
    if (!existing) {
      tabs.update((current) => [...current, request]);
    } else if (existing.key !== request.key) {
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
    if (remaining.length === 0) {
      tabListOpen.set(false);
      props.onClose();
    }
  };
  const filteredTabs = () => {
    const query = tabQuery().trim().toLowerCase();
    return query ? tabs().filter((tab) => tabTitle(tab).toLowerCase().includes(query)) : tabs();
  };
  return (
    <View testID="right-panel" class="w-full flex-1 bg-background">
      <View class="shrink-0 flex-row items-center border-b border-border" style={{ height: titleBar().height || 44, paddingRight: platform.os === "windows" ? WINDOW_CONTROLS_WIDTH + 4 : titleBar().right, borderBottomWidth: 0.5 }} desktop={{ windowDrag: true }}>
        <Popover
          open={tabListOpen()}
          onOpenChange={({ open }) => {
            tabListOpen.set(open);
            if (!open) tabQuery.set("");
          }}
          placement="bottom"
          align="start"
          offset={4}
          margin={8}
          popupClass="w-60 overflow-hidden rounded-xl border border-border bg-popover shadow-xl"
          trigger={
            <View
              role="button"
              testID="right-tab-list-toggle"
              accessibilityLabel={t("right_panel.tab_list")}
              class="ml-1 h-7 w-7 shrink-0 items-center justify-center rounded hover:bg-accent"
              desktop={{ windowDrag: false }}
            >
              <Icon name="chevron-down" size={14} color={token("muted-foreground")} />
            </View>
          }
        >
          <View class="w-full">
            <View class="h-10 flex-row items-center gap-2 border-b border-border px-3">
              <Icon name="search" size={14} color={token("muted-foreground")} />
              <Input
                testID="right-tab-search"
                autoFocus
                value={tabQuery()}
                onChange={({ value }) => tabQuery.set(value)}
                placeholder={t("right_panel.search_tabs")}
                class="min-w-0 flex-1 text-sm text-foreground placeholder:text-muted-foreground"
              />
            </View>
            <Scroll class="max-h-72">
              <View class="py-1">
                <Text class="px-3 pb-1 pt-2 text-xs font-medium text-muted-foreground">{t("right_panel.open_tabs")}</Text>
                <View class="gap-0.5 px-1.5">
                  <Each of={filteredTabs()} key={(tab) => tabId(tab)}>
                    {(tab) => {
                      const id = () => tabId(tab());
                      return (
                        <View class="flex-row items-center gap-2 rounded-(--radius) px-2 hover:bg-accent">
                          <Button
                            testID={`right-tab-list-${id()}`}
                            class="min-w-0 flex-1 flex-row items-center gap-2 rounded-(--radius) px-1 py-1"
                            onPress={() => {
                              activeKey.set(id());
                              tabListOpen.set(false);
                            }}
                          >
                            <Icon name={tabIcon(tab())} size={14} color={token("muted-foreground")} />
                            <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-left text-sm text-foreground">{tabTitle(tab())}</Text>
                          </Button>
                          <Button
                            testID={`right-tab-list-close-${id()}`}
                            accessibilityLabel={t("right_panel.close_tab", { name: tabTitle(tab()) })}
                            class="h-6 w-6 shrink-0 items-center justify-center rounded hover:bg-foreground/10"
                            onPress={() => closeTab(id())}
                          >
                            <Icon name="x" size={12} color={token("muted-foreground")} />
                          </Button>
                        </View>
                      );
                    }}
                  </Each>
                </View>
                <If when={filteredTabs().length === 0}>
                  <Text class="px-3 py-2 text-sm text-muted-foreground">{t("right_panel.no_matching_tabs")}</Text>
                </If>
              </View>
            </Scroll>
          </View>
        </Popover>
        <Scroll horizontal class="min-w-0 flex-1" contentClass="h-10 items-center gap-1 px-1" desktop={{ windowDrag: true }}>
          <Tabs class="h-10 flex-row items-center gap-1">
            <Each of={tabs()} key={(tab) => tabId(tab)}>
              {(tab) => {
                const id = () => tabId(tab());
                return (
                  <Tab
                    testID={`right-tab-${id()}`}
                    selected={activeKey() === id()}
                    class={`h-6 flex-row items-center gap-1 rounded-(--radius) pl-2 pr-1 ${activeKey() === id() ? "bg-accent" : "hover:bg-accent/60"}`}
                    onPress={() => activeKey.set(id())}
                  >
                    <Icon
                      name={tabIcon(tab())}
                      size={14}
                      color={activeKey() === id() ? token("foreground") : token("muted-foreground")}
                    />
                    <Text class={activeKey() === id() ? "max-w-32 text-xs text-foreground" : "max-w-32 text-xs text-muted-foreground"} style={{ numberOfLines: 1 }}>
                      {tabTitle(tab())}
                    </Text>
                    <Button
                      testID={`right-tab-close-${id()}`}
                      accessibilityLabel={t("right_panel.close_tab", { name: tabTitle(tab()) })}
                      class="h-4 w-4 items-center justify-center rounded hover:bg-foreground/10"
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
        <View class="shrink-0 flex-row items-center gap-1">
          <Menu
            open={addOpen()}
            onOpenChange={({ open }) => addOpen.set(open)}
            placement="bottom"
            align="end"
            popupClass="w-44 rounded-xl border border-border bg-popover p-1 shadow-xl"
            trigger={
              <View
                role="button"
                testID="right-tab-add"
                accessibilityLabel={t("right_panel.add_tab")}
                class="h-8 w-8 shrink-0 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
                desktop={{ windowDrag: false }}
              >
                <Icon name="plus" size={14} color={token("secondary-foreground")} />
              </View>
            }
          >
            <MenuItem
              testID="right-tab-add-changes"
              class="flex-row items-center gap-2 rounded-(--radius) px-2 py-1 hover:bg-accent"
              onSelect={() => {
                addOpen.set(false);
                props.store.openChanges();
              }}
            >
              <Icon name="git-branch" size={14} color={token("muted-foreground")} />
              <Text class="flex-1 text-sm text-foreground">{t("right_panel.tab.changes")}</Text>
            </MenuItem>
          </Menu>
          <Button
            testID="right-panel-maximize"
            accessibilityLabel={t(props.maximized ? "right_panel.restore" : "right_panel.fullscreen")}
            class="h-8 w-8 shrink-0 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
            onPress={props.onToggleMaximize}
          >
            <Icon name={props.maximized ? "minimize-2" : "maximize-2"} size={14} color={token("secondary-foreground")} />
          </Button>
          <Button
            testID="right-panel-close"
            accessibilityLabel={t("right_panel.close_panel")}
            class="h-8 w-8 shrink-0 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
            onPress={props.onClose}
          >
            <Icon name="panel-right-close" size={14} color={token("secondary-foreground")} />
          </Button>
        </View>
      </View>
      <If when={active()}>
        {(request) => <ActiveContent {...props} request={request()} />}
      </If>
    </View>
  );
}
