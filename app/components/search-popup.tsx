import { effect, keymap, List, signal, Text, token, View, type YNode } from "nsh";
import { Button, Dialog, Input } from "nsh/base";
import { t, tCount } from "../i18n";
import type { SessionContentHit, SessionMeta, WorkspaceMeta } from "../protocol";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { Icon } from "./icon";
import { formatSessionTime } from "./sidebar";

export const SEARCH_TABS = ["all", "workspaces", "sessions"] as const;
export type SearchTab = typeof SEARCH_TABS[number];

type SearchResultKind = "workspace" | "session" | "content" | "command";

type SearchResult = {
  id: string;
  kind: SearchResultKind;
  title: string;
  detail: string;
  path: string;
  snippet?: SessionContentHit;
  pluginInstance?: string;
  commandId?: string;
};

type SearchListItem =
  | { id: string; kind: "header"; label: string; group: SearchResultKind }
  | { id: string; kind: "row"; result: SearchResult; index: number };

export function nextSearchTab(tab: SearchTab, delta: number): SearchTab {
  const index = SEARCH_TABS.indexOf(tab);
  return SEARCH_TABS[(index + delta + SEARCH_TABS.length) % SEARCH_TABS.length];
}

export function nextSearchSelection(index: number, delta: number, count: number): number {
  return count === 0 ? 0 : (index + delta + count) % count;
}

export function buildSearchResults(options: {
  query: string;
  tab: SearchTab;
  cwd: string;
  sessions: SessionMeta[];
  workspaces: WorkspaceMeta[];
  contentHits?: SessionContentHit[];
  commands?: { instance: string; title: string; commandId: string }[];
}): SearchResult[] {
  const query = options.query.trim().toLowerCase();
  const matches = (value: string) => !query || value.toLowerCase().includes(query);
  const sessions = options.sessions
    .filter((session) => !session.archived)
    .sort((a, b) => b.updated_at - a.updated_at);
  const hidden = new Set(options.workspaces.filter((workspace) => workspace.hidden).map((workspace) => workspace.path));
  const activity = new Map<string, number>();
  for (const session of sessions) {
    if (!hidden.has(session.cwd)) {
      activity.set(session.cwd, Math.max(activity.get(session.cwd) ?? 0, session.updated_at));
    }
  }
  for (const workspace of options.workspaces) {
    if (!workspace.hidden && !activity.has(workspace.path)) activity.set(workspace.path, 0);
  }
  if (!hidden.has(options.cwd) && !activity.has(options.cwd)) activity.set(options.cwd, 0);
  const displayWorkspaceName = (path: string) =>
    options.workspaces.find((workspace) => workspace.path === path)?.alias || workspaceName(path);
  const workspaces = options.tab === "sessions" ? [] : [...activity.entries()]
    .sort(([, a], [, b]) => b - a)
    .map(([path]) => path)
    .filter((path) => matches(displayWorkspaceName(path)) || matches(path))
    .map((path): SearchResult => ({
      id: `workspace:${path}`,
      kind: "workspace",
      title: displayWorkspaceName(path),
      detail: path,
      path,
    }));
  if (options.tab === "workspaces") return workspaces;

  const titleSessions = sessions
    .filter((session) => matches(session.title))
    .slice(0, 50);
  const titleIds = new Set(titleSessions.map((session) => session.id));
  const sessionRow = (session: SessionMeta, content?: SessionContentHit): SearchResult => ({
    id: session.id,
    kind: content ? "content" : "session",
    title: session.title || t("search.untitled_session"),
    detail: `${displayWorkspaceName(session.cwd)} · ${formatSessionTime(session.updated_at)}`,
    path: session.cwd,
    snippet: content,
  });
  const titleResults = titleSessions.map((session) => sessionRow(session));
  const contentResults = (options.contentHits ?? [])
    .filter((hit) => !titleIds.has(hit.session_id))
    .map((hit) => ({ hit, session: sessions.find((session) => session.id === hit.session_id) }))
    .filter((entry): entry is { hit: SessionContentHit; session: SessionMeta } => Boolean(entry.session))
    .sort((a, b) => b.session.updated_at - a.session.updated_at)
    .slice(0, 20)
    .map(({ hit, session }) => sessionRow(session, hit));
  const commands = options.tab === "all" ? (options.commands ?? [])
    .filter((command) => matches(command.title))
    .map((command): SearchResult => ({
      id: `command:${command.instance}:${command.commandId}`,
      kind: "command",
      title: command.title,
      detail: t("search.plugin_command"),
      path: "",
      pluginInstance: command.instance,
      commandId: command.commandId,
    })) : [];
  return [...workspaces, ...titleResults, ...contentResults, ...commands];
}

function ResultLine(props: { result: () => SearchResult }) {
  return (
    <View class="min-w-0 flex-1 flex-row items-center gap-2">
      <Icon
        name={props.result().kind === "workspace" ? "folder-closed" : props.result().kind === "command" ? "terminal" : "message-square"}
        size={16}
        color={token("muted-foreground")}
      />
      <Text numberOfLines={1} class="min-w-0 flex-1 text-left text-sm text-foreground">
        {props.result().title}
      </Text>
      <Text numberOfLines={1} class="min-w-0 max-w-60 shrink-0 text-right text-xs text-muted-foreground">
        {props.result().detail}
      </Text>
    </View>
  );
}

function SearchGroupHeader(props: { item: () => Extract<SearchListItem, { kind: "header" }> }) {
  return (
    <Text
      testID={`search-${props.item().group}-header`}
      class="h-6 px-4 text-xs font-medium leading-6 text-muted-foreground"
    >
      {props.item().label}
    </Text>
  );
}

function SearchResultButton(props: {
  item: () => Extract<SearchListItem, { kind: "row" }>;
  selected: () => boolean;
  onChoose: (result: SearchResult) => void;
}) {
  const result = () => props.item().result;
  return (
    <Button
      testID={`search-${result().kind === "content" ? "session" : result().kind}-${props.item().index}`}
      selected={props.selected()}
      class="mx-2 rounded-md px-2 py-1.5 hover:bg-accent/50 selected:bg-accent"
      onPress={() => props.onChoose(result())}
    >
      <If when={!result().snippet}>
        <ResultLine result={result} />
      </If>
      <If when={Boolean(result().snippet)}>
        <View class="gap-0.5">
          <ResultLine result={result} />
          <View class="min-w-0 flex-1 flex-row items-center pl-6">
            <Text numberOfLines={1} class="max-w-[45%] shrink-0 text-xs text-muted-foreground">
              {result().snippet?.before}
            </Text>
            <Text class="shrink-0 rounded-sm bg-accent px-0.5 text-xs text-foreground">
              {result().snippet?.hit}
            </Text>
            <Text numberOfLines={1} class="min-w-0 flex-1 text-xs text-muted-foreground">
              {result().snippet?.after}
            </Text>
          </View>
        </View>
      </If>
    </Button>
  );
}

function KeyHint(props: { keys: string[]; label: string }) {
  return (
    <View class="flex-row items-center gap-1.5">
      <Each of={props.keys}>
        {(key) => (
          <Text class="rounded border border-border bg-accent/50 px-1 py-0.5 text-[10px] text-muted-foreground">
            {key()}
          </Text>
        )}
      </Each>
      <Text class="text-xs text-muted-foreground">{props.label}</Text>
    </View>
  );
}

export function SearchPopup(props: UiContext & { onClose: () => void }) {
  const query = signal("");
  const tab = signal<SearchTab>("all");
  const selected = signal(0);
  const content = signal<{ query: string; hits: SessionContentHit[] }>({ query: "", hits: [] });
  let input: YNode | undefined;
  let list: YNode | undefined;
  const results = () => buildSearchResults({
    query: query(),
    tab: tab(),
    cwd: props.store.cwd(),
    sessions: props.store.sessions(),
    workspaces: props.store.workspaces(),
    contentHits: content().query === query() ? content().hits : [],
    commands: props.store.pluginCommands().map((command) => ({
      instance: command.instance,
      title: command.title,
      commandId: command.command.id,
    })),
  });
  const groups = () => (["workspace", "session", "content", "command"] as const)
    .map((kind) => ({ kind, results: results().filter((result) => result.kind === kind) }))
    .filter((group) => group.results.length > 0);
  const items = () => {
    const rows: SearchListItem[] = [];
    let index = 0;
    for (const group of groups()) {
      rows.push({
        id: `header:${group.kind}`,
        kind: "header",
        group: group.kind,
        label: tCount(`search.groups.${group.kind === "workspace" ? "workspaces" : group.kind === "session" ? "sessions" : group.kind === "content" ? "content" : "commands"}`, group.results.length),
      });
      for (const result of group.results) {
        rows.push({ id: `row:${result.kind}:${result.id}`, kind: "row", result, index });
        index += 1;
      }
    }
    return rows;
  };
  const listHeight = () => items().reduce((height, item) =>
    height + (item.kind === "header" ? 28 : item.result.snippet ? 52 : 32), items().length === 0 ? 64 : 8
  );
  const choose = (result: SearchResult) => {
    props.onClose();
    if (result.kind === "session" || result.kind === "content") {
      props.store.openSession(result.id);
    } else if (result.kind === "workspace") {
      props.store.newTaskInWorkspace(result.path);
    } else if (result.pluginInstance && result.commandId) {
      props.store.runPluginCommand(result.pluginInstance, result.commandId);
    }
  };
  const setTab = (value: SearchTab) => {
    tab.set(value);
    input?.focus();
  };
  effect(() => {
    query();
    tab();
    selected.set(0);
    list?.scrollToOffset({ offset: 0, animated: false });
  });
  effect(() => {
    const value = query();
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    if (Array.from(value.trim()).length < 2) {
      content.set({ query: "", hits: [] });
    } else {
      timer = setTimeout(() => {
        void props.store.searchSessionContent(value).then((hits) => {
          if (!cancelled) content.set({ query: value, hits });
        }, () => {
          if (!cancelled) content.set({ query: value, hits: [] });
        });
      }, 250);
    }
    return () => {
      cancelled = true;
      if (timer !== undefined) clearTimeout(timer);
    };
  });
  effect(() => {
    const selectedId = results()[selected()]?.id;
    const index = items().findIndex((item) => item.kind === "row" && item.result.id === selectedId);
    if (index < 0) return;
    const frame = requestAnimationFrame(() => list?.scrollToIndex({ index, viewPosition: 0.5, animated: false }));
    return () => cancelAnimationFrame(frame);
  });
  effect(() => keymap.bind({
    up: () => selected.update((index) => nextSearchSelection(index, -1, results().length)),
    down: () => selected.update((index) => nextSearchSelection(index, 1, results().length)),
    tab: () => setTab(nextSearchTab(tab(), 1)),
    "shift+tab": () => setTab(nextSearchTab(tab(), -1)),
    escape: props.onClose,
  }, { when: "always" }));
  return (
    <Dialog
      open
      animation="fade"
      backdropClass="bg-popover/90"
      onOpenChange={(open) => { if (!open) props.onClose(); }}
      testID="global-search-dialog"
      class="mx-auto mt-[120px] w-[600px] max-w-[90%] overflow-hidden rounded-xl border border-border bg-popover shadow-xl"
    >
      <View class="h-12 flex-row items-center gap-2 border-b border-border px-4">
        <Icon name="search" size={16} color={token("muted-foreground")} />
        <Input
          ref={(value) => { input = value; }}
          testID="global-search-input"
          autoFocus
          value={query()}
          onChange={(value) => query.set(value)}
          onSubmit={() => {
            const result = results()[selected()];
            if (result) choose(result);
            else props.onClose();
          }}
          placeholder={t("search.placeholder")}
          returnKeyType="search"
          submitBehavior="submit"
          class="min-w-0 flex-1 px-0 py-0 text-sm text-foreground selection:bg-foreground placeholder:text-muted-foreground"
        />
        <If when={query().length > 0}>
          <Button
            testID="global-search-clear"
            accessibilityLabel={t("search.clear_label")}
            class="h-6 w-6 items-center justify-center rounded-(--radius) hover:bg-accent"
            onPress={() => {
              query.set("");
              input?.focus();
            }}
          >
            <Icon name="x" size={14} color={token("muted-foreground")} />
          </Button>
        </If>
      </View>
      <View class="flex-row items-center gap-1 border-b border-border px-3 py-1.5">
        <Each of={SEARCH_TABS} key={(value) => value}>
          {(value) => (
            <Button
              testID={`search-tab-${value()}`}
              selected={tab() === value()}
              accessibilityLabel={t(`search.tabs.${value()}`)}
              class="h-6 flex-row items-center justify-center rounded-md px-2.5 hover:bg-accent/50 selected:bg-accent"
              onPress={() => setTab(value())}
            >
              <Text class={tab() === value() ? "text-xs font-medium text-foreground" : "text-xs text-muted-foreground"}>
                {t(`search.tabs.${value()}`)}
              </Text>
            </Button>
          )}
        </Each>
      </View>
      <List
        ref={(value) => { list = value; }}
        testID="global-search-results"
        each={items()}
        keyExtractor={(item) => item.id}
        estimatedItemSize={32}
        style={{ height: Math.min(360, listHeight()) }}
        contentClass="py-1"
        empty={<Text class="px-3 py-6 text-center text-sm text-muted-foreground">{t("search.no_results")}</Text>}
      >
        {(item) => (
          <>
            <If when={item().kind === "header"}>
              <SearchGroupHeader item={() => item() as Extract<SearchListItem, { kind: "header" }>} />
            </If>
            <If when={item().kind === "row"}>
              <SearchResultButton
                item={() => item() as Extract<SearchListItem, { kind: "row" }>}
                selected={() => {
                  const current = item();
                  return current.kind === "row" && results()[selected()]?.id === current.result.id;
                }}
                onChoose={choose}
              />
            </If>
          </>
        )}
      </List>
      <View testID="search-key-hints" class="h-10 flex-row items-center gap-4 border-t border-border px-4">
        <KeyHint keys={["Up", "Down"]} label={t("search.hints.select")} />
        <KeyHint keys={["Enter"]} label={t("search.hints.open")} />
        <KeyHint keys={["Tab"]} label={t("search.hints.filter")} />
        <KeyHint keys={["Esc"]} label={t("search.hints.close")} />
      </View>
    </Dialog>
  );
}
