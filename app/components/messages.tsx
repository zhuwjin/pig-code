import {
  Accessor,
  Animated,
  Clipboard,
  createAnimatedValue,
  Easing,
  Image,
  List,
  type ListRef,
  reduceMotion,
  Scroll,
  type ScrollRef,
  type ScrollEvent,
  Text,
  View,
  computed,
  effect,
  keymap,
  openUrl,
  signal,
  token,
  untracked,
} from "nsh";
import { Button, Input, TextView } from "nsh/base";
import { t, tCount } from "../i18n";
import type { ChatItem, MarkdownItem, NoteItem, NotificationItem, TaskNotification, UserItem } from "../protocol";
import { toggleExpanded, toggleTurnExpanded, toggleUserExpanded, type UiContext } from "../ui-context";
import {
  activitySummary,
  conversationRowsEqual,
  flattenConversationRows,
  groupConversationActivity,
  type ConversationRow,
  formatClock,
  formatDuration,
  formatTokens,
  mentionSegments,
  resolveActiveUserRow,
  taskStatusLabel,
  turnDurationMs,
  userMessageIsLong,
} from "../conversation";
import { Icon } from "./icon";
import { ImageViewer } from "./image-viewer";
import { ReasoningView } from "./reasoning-view";
import {
  REASONING_TICKER_INTERVAL_MS,
  REASONING_TICKER_ROLL_MS,
  ReasoningTicker,
  reasoningTickerLine,
} from "./reasoning-ticker";
import { ShimmerText } from "./shimmer-text";
import { ToolRow } from "./tool-card";
import { TurnActionsRow, TurnChangesRow } from "./turn-rows";

const FOLLOW_THRESHOLD = 24;
const ACTIVITY_MAX_HEIGHT = 420;

export type MessageProps = UiContext & {
  textFor: (item: MarkdownItem) => string;
  onImage: (source: string, label: string) => void;
  flat?: boolean;
  activityPreview?: boolean;
  onActivityItemViewable?: (id: string, visible: boolean) => void;
};

function UserRow(props: MessageProps & { item: Accessor<UserItem> }) {
  const item = () => props.item();
  const long = () => userMessageIsLong(item().text);
  const open = () => props.store.isExpanded(`user-${item().id}`);
  const copied = () => props.store.copiedItems()[`user-copy-${item().id}`] ?? false;
  const segments = () => mentionSegments(item().text, item().files, item().imageNums.length);
  const unmatched = () => item().files.filter((file) => !item().text.includes(`@${file}`));
  const canRecall = () => {
    const id = props.store.current();
    if (!id || props.store.running()[id]) return false;
    return (props.store.items()[id] ?? []).findLast((entry) => entry.kind === "user")?.id === item().id;
  };
  return (
    <View class="group items-end gap-1 px-1">
      <If when={unmatched().length > 0}>
        <View class="max-w-[80%] flex-row flex-wrap justify-end gap-1">
          <Each of={unmatched()} key={(path) => path}>
            {(path) => (
              <Button
                class="flex-row items-center gap-1 rounded bg-accent px-2 py-0.5"
                onPress={() => props.store.openFile(path())}
              >
                <Icon name="file-text" size={12} color={token("muted-foreground")} />
                <Text class="text-xs text-foreground">{path()}</Text>
              </Button>
            )}
          </Each>
        </View>
      </If>
      <View
        class={props.themeStore.classes(
          "chat.user",
          "rounded-2xl bg-accent px-3 py-2",
        )}
        style={{ maxWidth: "80%" }}
      >
        <If when={item().imageNums.length > 0}>
          <View class="flex-row flex-wrap gap-2 pb-2">
            <Each of={item().imageNums} key={(number) => number}>
              {(number, index) => {
                const source = () => props.store.current()
                  ? props.store.sessionImageSource(props.store.current()!, number())
                  : "";
                return (
                  <View class="relative h-14 w-14">
                    <Button
                      testID={`user-image-${item().id}-${number()}`}
                      accessibilityLabel={t("messages.image_label", { index: index() + 1 })}
                      class="h-14 w-14 overflow-hidden rounded-lg border border-border"
                      onPress={() => props.onImage(source(), t("messages.image_label", { index: index() + 1 }))}
                    >
                      <If when={source()}>
                        <Image src={source()} style={{ objectFit: "cover" }} class="h-14 w-14" fadeDuration={150} />
                      </If>
                    </Button>
                    <View
                      testID={`user-image-badge-${item().id}-${number()}`}
                      class="absolute -bottom-1 -right-1 h-5 min-w-5 items-center justify-center rounded-full border border-border bg-background px-1"
                      style={{ pointerEvents: "none" }}
                    >
                      <Text style={{ fontSize: 10, lineHeight: 12 }} class="text-foreground">{index() + 1}</Text>
                    </View>
                  </View>
                );
              }}
            </Each>
          </View>
        </If>
        <View selectable class={`flex-row flex-wrap items-center overflow-hidden${long() && !open() ? " max-h-28" : ""}`}>
          <Each of={segments()} key={(segment) => `${segment.type}-${segment.text}-${segment.index ?? 0}`}>
            {(segment) => (
              <View class="flex-row items-center">
                <If when={segment().type === "text"}>
                  <Text class="text-sm leading-6 text-foreground">{segment().text}</Text>
                </If>
                <If when={segment().type !== "text"}>
                  <Button
                    class="flex-row items-center gap-1 rounded bg-accent/60 px-1.5 py-0.5 hover:bg-accent"
                    onPress={() => {
                      if (segment().type === "file") props.store.openFile(segment().text);
                      else if (props.store.current()) {
                        const number = item().imageNums[segment().index ?? 0];
                        props.onImage(props.store.sessionImageSource(props.store.current()!, number), segment().text);
                      }
                    }}
                  >
                    <Icon name={segment().type === "file" ? "file-text" : "image"} size={12} color={token("muted-foreground")} />
                    <Text class="text-xs text-foreground underline">
                      {segment().type === "file" ? segment().text.split(/[\\/]/).pop() : segment().text}
                    </Text>
                  </Button>
                </If>
              </View>
            )}
          </Each>
        </View>
        <If when={long()}>
          <View class="items-center pt-1">
            <Button
              testID={`user-${open() ? "collapse" : "expand"}-${item().id}`}
              accessibilityLabel={t(open() ? "messages.collapse" : "messages.expand")}
              class="h-6 w-6 items-center justify-center rounded-full border border-border bg-background"
              onPress={() => toggleUserExpanded(props, item().id)}
            >
              <Icon name={open() ? "chevron-up" : "chevron-down"} size={13} color={token("muted-foreground")} />
            </Button>
          </View>
        </If>
      </View>
      <View class="flex-row items-center gap-1 opacity-0 group-hover:opacity-100">
        <If when={item().sentAtMs > 0}>
          <Text class="mr-1 text-xs text-muted-foreground">{formatClock(item().sentAtMs)}</Text>
        </If>
        <Button
          testID={`copy-user-${item().id}`}
          accessibilityLabel={t("messages.copy_user")}
          class="h-5 w-5 items-center justify-center rounded hover:bg-accent"
          onPress={() => {
            void Clipboard.writeText(item().text).then(() => props.store.markCopied(`user-copy-${item().id}`));
          }}
        >
          <Icon name={copied() ? "check" : "copy"} size={12} color={token(copied() ? "success" : "muted-foreground")} />
        </Button>
        <If when={canRecall()}>
          <Button
            testID={`recall-${item().id}`}
            accessibilityLabel={t("messages.retract_edit")}
            class="h-5 w-5 items-center justify-center rounded hover:bg-accent"
            onPress={() => props.store.recallLastMessage()}
          >
            <Icon name="undo-2" size={12} color={token("muted-foreground")} />
          </Button>
        </If>
      </View>
    </View>
  );
}

function ReasoningRow(props: MessageProps & { item: Accessor<MarkdownItem> }) {
  const item = () => props.item();
  const open = () => !props.activityPreview && props.store.isExpanded(item().id);
  const roll = new ReasoningTicker();
  const tickerCurrent = signal(reasoningTickerLine(props.textFor(item()))?.text ?? "");
  const tickerPrevious = signal<{ text: string } | null>(null);
  const tickerProgress = createAnimatedValue(1);
  const fadeLeft = signal(false);
  const fadeRight = signal(false);
  let tickerAnimation: ReturnType<typeof Animated.timing> | undefined;
  let tickerTimer: ReturnType<typeof setTimeout> | undefined;
  let tickerScrollFrame: number | undefined;
  let tickerScroll: ScrollRef | undefined;

  const syncTicker = () => {
    tickerCurrent.set(roll.displayed?.text ?? "");
    tickerPrevious.set(roll.exiting ? { text: roll.exiting.text } : null);
  };
  const scrollTickerToEnd = () => {
    if (tickerScrollFrame !== undefined) cancelAnimationFrame(tickerScrollFrame);
    tickerScrollFrame = requestAnimationFrame(() => {
      tickerScrollFrame = undefined;
      tickerScroll?.scrollToEnd({ animated: false });
    });
  };
  const playTicker = () => {
    tickerAnimation?.stop();
    if (reduceMotion()) {
      tickerProgress.set(1);
      tickerPrevious.set(null);
      return;
    }
    tickerProgress.set(0);
    tickerAnimation = Animated.timing(tickerProgress, {
      toValue: 1,
      duration: REASONING_TICKER_ROLL_MS,
      easing: Easing.bezier(0.4, 0, 0.2, 1),
    });
    tickerAnimation.start(({ finished }) => {
      if (finished) tickerPrevious.set(null);
    });
  };
  const armTicker = () => {
    if (tickerTimer !== undefined) clearTimeout(tickerTimer);
    const generation = roll.generation;
    tickerTimer = setTimeout(() => {
      tickerTimer = undefined;
      if (roll.fire(generation, Date.now())) {
        syncTicker();
        playTicker();
        armTicker();
        scrollTickerToEnd();
      } else {
        syncTicker();
        tickerProgress.set(1);
      }
    }, REASONING_TICKER_INTERVAL_MS);
  };
  effect(() => {
    const target = reasoningTickerLine(props.textFor(item()));
    if (!item().streaming || open() || !target) {
      if (tickerTimer !== undefined) clearTimeout(tickerTimer);
      tickerTimer = undefined;
      tickerAnimation?.stop();
      roll.reset(target);
      tickerProgress.set(1);
      syncTicker();
      return;
    }
    const promoted = roll.feed(target);
    syncTicker();
    if (promoted) {
      playTicker();
      armTicker();
    }
    scrollTickerToEnd();
  });
  effect(() => {
    scrollTickerToEnd();
    return () => {
      if (tickerTimer !== undefined) clearTimeout(tickerTimer);
      if (tickerScrollFrame !== undefined) cancelAnimationFrame(tickerScrollFrame);
      tickerAnimation?.stop();
    };
  });
  const ticker = () => (
    <View class="min-w-0 shrink flex-row items-center gap-2">
      <Text class="shrink-0 text-sm leading-5 text-muted-foreground">·</Text>
      <View class="relative min-w-0 shrink">
        <Scroll
          ref={(node) => { tickerScroll = node; }}
          testID={`thinking-ticker-${item().id}`}
          horizontal
          class="w-full min-w-0"
          scrollbar={{ horizontal: { class: "hidden" } }}
          bounces={false}
          onScroll={(event) => {
            const max = Math.max(0, event.contentSize.width - event.size.width);
            fadeLeft.set(event.offset.x > 1);
            fadeRight.set(event.offset.x < max - 1);
          }}
        >
          <View class="relative h-5 overflow-hidden">
            <If when={tickerPrevious()}>
              {(previous) => (
                <View
                  class="absolute inset-x-0 top-0 h-5 overflow-hidden"
                  style={{
                    opacity: tickerProgress.interpolate({ inputRange: [0, 1], outputRange: [1, 0] }),
                    transform: [{ translateY: tickerProgress.interpolate({ inputRange: [0, 1], outputRange: [0, -11] }) }],
                  }}
                >
                  <Text class="shrink-0 text-sm leading-5 text-muted-foreground">{previous().text}</Text>
                </View>
              )}
            </If>
            <View
              class="h-5 overflow-hidden"
              style={{
                opacity: tickerProgress,
                transform: [{ translateY: tickerProgress.interpolate({ inputRange: [0, 1], outputRange: [11, 0] }) }],
              }}
            >
              <Text class="shrink-0 text-sm leading-5 text-muted-foreground">{tickerCurrent()}</Text>
            </View>
          </View>
        </Scroll>
        <If when={fadeLeft()}>
          <View
            class="absolute left-0 top-0 h-5 w-4"
            style={{ pointerEvents: "none", backgroundImage: `linear-gradient(to right, ${token("background")}, transparent)` }}
          />
        </If>
        <If when={fadeRight()}>
          <View
            class="absolute right-0 top-0 h-5 w-4"
            style={{ pointerEvents: "none", backgroundImage: `linear-gradient(to right, transparent, ${token("background")})` }}
          />
        </If>
      </View>
    </View>
  );
  const label = () => item().streaming
    ? t("messages.thinking")
    : item().durationMs != null
      ? formatDuration(item().durationMs!, t("messages.thought_for"))
      : t("messages.thought_process");
  return (
    <View class="w-full">
      <View
        role="button"
        testID={`thinking-${item().id}`}
        accessibilityState={{ expanded: open() }}
        class="w-full flex-row items-center gap-2 py-1"
        onPress={() => toggleExpanded(props, item().id)}
      >
        <Icon name="brain" size={16} color={token("muted-foreground")} />
        <If when={item().streaming}>
          <ShimmerText active baseOpacity={0.6} class="shrink-0" textClass="text-sm leading-5">
            {label()}
          </ShimmerText>
        </If>
        <If when={!item().streaming}>
          <Text class="shrink-0 text-sm leading-5 text-muted-foreground">{label()}</Text>
        </If>
        <If when={item().streaming && !open() && reasoningTickerLine(props.textFor(item()))}>{ticker()}</If>
        <Icon name={open() ? "chevron-down" : "chevron-right"} size={14} color={token("muted-foreground")} />
      </View>
      <If when={open()}>
        <View
          testID={`thinking-${item().id}-content`}
          class="ml-2 mt-1 overflow-hidden border-l border-border pl-3.5"
        >
          <ReasoningView text={() => props.textFor(item())} accessibilityLabel={label()} />
        </View>
      </If>
    </View>
  );
}

function MarkdownRow(props: MessageProps & { item: Accessor<MarkdownItem> }) {
  const item = () => props.item();
  const { themeStore } = props;
  return (
    <View class="gap-1 py-1">
      <TextView
        markdown={props.textFor(item())}
        options={{ streaming: item().streaming }}
        selectable
        classes={{
          root: themeStore.classes("chat.markdown", "text-sm leading-6 text-foreground"),
          codeBlock: themeStore.classes("markdown.code-block", "border-border bg-tool-surface"),
          codeBlockHeader: "text-muted-foreground",
          code: themeStore.classes("markdown.inline-code", "rounded bg-muted text-foreground"),
          link: "text-info",
          table: "border-border",
          tableHead: "bg-muted",
          rule: "border-border",
          alertNote: "border-info/40 bg-info/10 text-info",
          alertTip: "border-success/40 bg-success/10 text-success",
          alertImportant: "border-primary/40 bg-primary/10 text-foreground",
          alertWarning: "border-warning/40 bg-warning/10 text-warning",
          alertCaution: "border-destructive/40 bg-destructive/10 text-destructive",
        }}
        codeBlockActions={[{ id: "copy", label: t("common.copy") }]}
      />
      <If when={(item().sources?.length ?? 0) > 0}>
        <View class="flex-row flex-wrap items-center gap-2">
          <Text class="text-xs text-muted-foreground">{t("messages.sources")}</Text>
          <Each of={item().sources ?? []} key={(source) => source.url}>
            {(source) => (
              <Button
                class="rounded px-1 py-0.5 hover:bg-accent"
                onPress={() => openUrl(source().url.trim())}
                tooltip={source().snippet ?? source().url}
              >
                <Text class="text-xs text-info underline">{source().title || source().url}</Text>
              </Button>
            )}
          </Each>
        </View>
      </If>
    </View>
  );
}

function NoteRow(props: UiContext & { item: Accessor<NoteItem> }) {
  const item = () => props.item();
  if (item().noteKind === "compacting" || item().noteKind === "compacted") {
    const label = () => item().noteKind === "compacting"
      ? t("store.compacting")
      : item().usedBefore != null && item().usedAfter != null
        ? t("messages.compacted", { before: formatTokens(item().usedBefore!), after: formatTokens(item().usedAfter!) })
        : t("messages.compacted_plain");
    return (
      <View class="w-full flex-row items-center gap-3 py-2">
        <View class="h-px flex-1 bg-border" />
        <Text class={item().noteKind === "compacting" ? "text-sm text-foreground animate-pulse" : "text-sm text-muted-foreground"}>{label()}</Text>
        <If when={item().noteKind === "compacted"}>
          <Button onPress={() => props.store.openCompactSummary(item().summary ?? item().text)}>
            <Text class="text-sm text-info hover:underline">{t("messages.view_summary")}</Text>
          </Button>
        </If>
        <View class="h-px flex-1 bg-border" />
      </View>
    );
  }
  return (
    <View class="w-full items-center px-1 py-2">
      <Text style={{ userSelect: "text" }} class={item().error ? "text-center text-xs text-muted-foreground" : "text-center text-xs text-muted-foreground"}>
        {item().error ? `⚠ ${item().text}` : item().text}
      </Text>
    </View>
  );
}

function notificationFailed(notification: TaskNotification): boolean {
  return notification.status === "failed"
    || notification.status === "timed_out"
    || notification.status === "lost";
}

function notificationTitle(notification: TaskNotification): string {
  const description = notification.description
    .split(/\r?\n/)
    .map((line) => line.trim())
    .find(Boolean);
  return description || notification.title;
}

function notificationStatusLine(notification: TaskNotification): string {
  const status = taskStatusLabel(notification.status);
  return notification.duration_ms != null
    ? `${status} · ${t("messages.notification.duration", { duration: formatDuration(notification.duration_ms) })}`
    : status;
}

function NotificationRow(props: UiContext & { item: Accessor<NotificationItem> }) {
  const item = () => props.item();
  return (
    <View class="w-full items-end gap-2">
      <Each of={item().notifications} key={(notification) => notification.id}>
        {(notification) => {
          const failed = () => notificationFailed(notification());
          return (
            <View class="w-full items-end gap-1">
              <View class="flex-row items-center gap-1">
                <Icon
                  name={failed() ? "circle-x" : "circle-check"}
                  size={12}
                  color={token(failed() ? "destructive" : "success")}
                />
                <Text class={failed() ? "text-xs text-destructive" : "text-xs text-muted-foreground"}>
                  {t(notification().kind === "subagent" ? "messages.notification.sent_by_agent" : "messages.notification.sent_by_shell")}
                </Text>
              </View>
              <Button
                testID={`notification-${notification().id}`}
                class="max-w-[520px] items-start gap-1 rounded-2xl bg-accent px-3 py-2 hover:bg-accent active:opacity-90"
                onPress={() => {
                  if (notification().kind === "subagent" && notification().agent_id) {
                    props.store.openSubagent(notification().agent_id!);
                    return;
                  }
                  const task = props.store.currentTasks().find((candidate) => candidate.id === notification().task_id);
                  if (task) props.store.openTask(task);
                }}
              >
                <Text
                  style={{ numberOfLines: 1 }}
                  class={notification().kind === "shell"
                    ? "text-left font-mono text-sm leading-5 text-foreground"
                    : "text-left text-sm font-medium leading-5 text-foreground"}
                >
                  {notificationTitle(notification())}
                </Text>
                <Text class={failed() ? "text-xs text-destructive" : "text-xs text-muted-foreground"}>
                  {notificationStatusLine(notification())}
                </Text>
                <If when={notification().output?.type === "preview"}>
                  <Text style={{ numberOfLines: 3 }} class="text-left font-mono text-xs leading-5 text-muted-foreground">
                    {(notification().output as { text?: string } | null | undefined)?.text ?? ""}
                  </Text>
                </If>
              </Button>
            </View>
          );
        }}
      </Each>
    </View>
  );
}

function ActivityRow(props: MessageProps & { row: Accessor<Extract<ConversationRow, { type: "activity" }>> }) {
  const row = () => props.row();
  const open = () => props.store.isExpanded(row().id);
  const active = () => row().items.findLast((item) =>
    (item.kind === "reasoning" && item.streaming)
    || (item.kind === "tool" && (item.status === "input" || item.status === "running") && !item.approvalPending)
  );
  const preview = () => open() ? undefined : active();
  const toggle = () => toggleExpanded(props, row().id);
  const header = () => (
    <View
      role="button"
      testID={row().id}
      accessibilityState={{ expanded: open() }}
      class="w-full min-w-0 flex-row items-center gap-2 py-1"
      onPress={toggle}
    >
      <Icon name="list-collapse" size={16} color={token("muted-foreground")} />
      <Text style={{ numberOfLines: 1 }} class="min-w-0 shrink text-sm text-muted-foreground">
        {activitySummary(row().items)}
      </Text>
      <Icon name={open() ? "chevron-down" : "chevron-right"} size={14} color={token("muted-foreground")} />
    </View>
  );
  const body = () => (
    <Scroll
      testID={`${row().id}-body`}
      class="w-full"
      style={{ maxHeight: ACTIVITY_MAX_HEIGHT }}
      scrollbar
      bounces={false}
    >
      <List
        viewport="parent"
        class="w-full"
        contentClass="gap-2"
        each={row().items}
        keyExtractor={(item) => item.id}
        estimatedItemSize={28}
        onViewableItemsChange={({ viewableItems }) => {
          for (const entry of viewableItems) {
            props.onActivityItemViewable?.(entry.item.id, entry.isViewable);
          }
        }}
      >
        {(item) => <ItemMessageRow {...props} item={item} />}
      </List>
    </Scroll>
  );
  return (
    <View class="w-full">
      <If when={preview()} else={header()}>
        {(item) => (
          <View
            role="button"
            testID={row().id}
            accessibilityState={{ expanded: false }}
            class="w-full min-w-0"
            onPress={toggle}
          >
            <View class="w-full min-w-0" style={{ pointerEvents: "none" }}>
              <ItemMessageRow {...props} item={item} activityPreview />
            </View>
          </View>
        )}
      </If>
      <If when={open()}>
        <View
          testID={`${row().id}-content`}
          class="ml-2 mt-1 overflow-hidden border-l border-border pl-3.5 pt-1"
        >
          {body()}
        </View>
      </If>
    </View>
  );
}

function WorkRow(props: MessageProps & { row: Accessor<Extract<ConversationRow, { type: "work" }>> }) {
  const row = () => props.row();
  const turn = () => row().turn;
  const open = () => props.store.isTurnExpanded(turn().id);
  const flat = () => props.flat === true;
  const grouped = computed(() => groupConversationActivity(row().items));
  return (
    <View class="w-full">
      <Button
        testID={`work-${turn().id}`}
        class="flex-row items-center gap-1 self-start py-1"
        onPress={() => toggleTurnExpanded(props, turn().id)}
      >
        <Text class="text-xs text-muted-foreground">
          {turn().status === "stopped"
            ? t("turn.stopped")
            : (turnDurationMs(turn()) ?? 0) > 0
              ? formatDuration(turnDurationMs(turn())!, t("turn.worked_for"))
              : t("turn.processed")}
        </Text>
        <Icon name={open() ? "chevron-down" : "chevron-right"} size={13} color={token("muted-foreground")} />
      </Button>
      <If when={open() && !flat()}>
        <View class="pt-2">
          <List
            viewport="parent"
            class="w-full"
            contentClass="gap-2"
            each={grouped()}
            keyExtractor={(child) => child.id}
            estimatedItemSize={32}
          >
            {(child) => (
              <If
                when={child().type === "activity"}
                else={<ItemMessageRow {...props} item={() => (child() as Extract<ConversationRow, { type: "item" }>).item} />}
              >
                <ActivityRow {...props} row={() => child() as Extract<ConversationRow, { type: "activity" }>} />
              </If>
            )}
          </List>
        </View>
      </If>
    </View>
  );
}

function ItemMessageRow(props: MessageProps & { item: Accessor<ChatItem> }) {
  const item = () => props.item();
  const rowProps = props;
  return (
    <View>
      <If when={item().kind === "user"}>
        <UserRow {...rowProps} item={() => item() as UserItem} />
      </If>
      <If when={item().kind === "reasoning"}>
        <ReasoningRow {...rowProps} item={() => item() as MarkdownItem} />
      </If>
      <If when={item().kind === "markdown"}>
        <MarkdownRow {...rowProps} item={() => item() as MarkdownItem} />
      </If>
      <If when={item().kind === "tool"}>
        <ToolRow {...rowProps} item={() => item() as Extract<ChatItem, { kind: "tool" }>} />
      </If>
      <If when={item().kind === "note"}>
        <NoteRow {...rowProps} item={() => item() as NoteItem} />
      </If>
      <If when={item().kind === "notification"}>
        <NotificationRow {...rowProps} item={() => item() as NotificationItem} />
      </If>
    </View>
  );
}

export function MessageRow(props: MessageProps & { row: Accessor<ConversationRow>; active: boolean; navEligible: boolean }) {
  const row = () => props.row();
  const rowProps = props;
  return (
    <View
      class={props.active
        ? `w-full rounded-lg bg-warning/10 ${props.navEligible ? "px-12" : ""}`
        : `w-full ${props.navEligible ? "px-12" : ""}`}
    >
      <View class="mx-auto w-full max-w-[860px] px-4">
        <If when={row().type === "item"}>
          <ItemMessageRow {...rowProps} item={() => (row() as Extract<ConversationRow, { type: "item" }>).item} />
        </If>
        <If when={row().type === "activity"}>
          <ActivityRow {...rowProps} row={() => row() as Extract<ConversationRow, { type: "activity" }>} />
        </If>
        <If when={row().type === "work"}>
          <WorkRow {...rowProps} row={() => row() as Extract<ConversationRow, { type: "work" }>} />
        </If>
        <If when={row().type === "changes"}>
          <TurnChangesRow {...rowProps} turn={() => (row() as Extract<ConversationRow, { type: "changes" }>).turn} />
        </If>
        <If when={row().type === "actions"}>
          <TurnActionsRow
            {...rowProps}
            turn={() => (row() as Extract<ConversationRow, { type: "actions" }>).turn}
            markdownText={() => (row() as Extract<ConversationRow, { type: "actions" }>).markdown.map((item) => item.text).join("\n\n")}
          />
        </If>
      </View>
    </View>
  );
}

function retryLabel(status: NonNullable<ReturnType<UiContext["store"]["retrying"]>[string]>): string {
  const reason = Object.keys(status.reason)[0] ?? "Network";
  const label = t(reason === "RateLimit" ? "messages.retry.rate_limit" : reason === "Server" ? "messages.retry.server" : reason === "EmptyCompletion" ? "messages.retry.empty_completion" : "messages.retry.network");
  const wait = Math.round(status.delay_ms / 1000);
  const waitText = wait ? t("messages.retry.wait", { secs: wait }) : "";
  return `${t("messages.retry.progress", { attempt: status.attempt, max: status.max_attempts })}${waitText} · ${label}`;
}

function TailRows(props: UiContext & { navEligible: boolean }) {
  const now = signal(Date.now());
  effect(() => {
    const timer = setInterval(() => now.set(Date.now()), 1000);
    return () => clearInterval(timer);
  });
  const id = () => props.store.current();
  const running = () => id() ? (props.store.running()[id()!] ?? false) : false;
  const turn = () => props.store.currentTurns().findLast((candidate) => candidate.status === "running");
  const retry = () => id() ? props.store.retrying()[id()!] : null;
  const hook = () => id() ? props.store.hookRunning()[id()!] : null;
  const notice = () => id() ? props.store.projectHooks()[id()!] : null;
  return (
    <View class={`w-full pb-4 pt-2 ${props.navEligible ? "px-12" : ""}`}>
      <View class="mx-auto w-full max-w-[860px] gap-4 px-4">
        <If when={running() && !retry()}>
          <View class="flex-row items-center gap-2">
            <Icon name="loader-circle" size={14} class="animate-spin" color={token("muted-foreground")} />
            <Text class="text-xs text-muted-foreground animate-pulse">
              {formatDuration(turn() ? now() - turn()!.startedAt : 0, t("messages.working_for"))}
            </Text>
          </View>
        </If>
        <If when={retry()}>
          {(value) => (
            <View class="flex-row items-center gap-2">
              <Icon name="loader-circle" size={13} class="animate-spin" color={token("muted-foreground")} />
              <Button testID="retry-now" class="rounded-full border border-border px-2 py-0.5" onPress={() => props.store.retryNow()}>
                <Text class="text-xs text-foreground">{t("messages.retry_now")}</Text>
              </Button>
              <Text class="min-w-0 flex-1 text-xs text-muted-foreground">{retryLabel(value())}</Text>
            </View>
          )}
        </If>
        <If when={hook()}>
          {(value) => (
            <View class="flex-row items-center gap-2">
              <Icon name="loader-circle" size={13} class="animate-spin" color={token("muted-foreground")} />
              <Text class="text-xs text-muted-foreground">{t("messages.hook_running", { event: value().event, label: value().label })}</Text>
            </View>
          )}
        </If>
        <If when={notice()}>
          {(value) => (
            <View class="flex-row items-center gap-2 rounded-lg border border-border bg-muted px-3 py-2">
              <Text class="min-w-0 flex-1 text-xs text-foreground">
                {tCount("messages.hooks_untrusted", value().count, { path: value().path })}
              </Text>
              <Button testID="hooks-trust" class="rounded-full border border-border px-2 py-0.5" onPress={() => props.store.trustProjectHooks()}>
                <Text class="text-xs text-foreground">{t("messages.trust")}</Text>
              </Button>
              <Button testID="hooks-dismiss" class="px-1" onPress={() => props.store.dismissProjectHooks()}>
                <Text class="text-xs text-muted-foreground">{t("messages.ignore")}</Text>
              </Button>
            </View>
          )}
        </If>
      </View>
    </View>
  );
}

export function MessageList(props: UiContext & { items: ChatItem[]; navFoot?: number; onScroll?: (event: ScrollEvent) => void; onLatestVisibleChange?: (visible: boolean) => void; onActivityItemViewable?: (id: string, visible: boolean) => void }) {
  const rows = computed(
    () => flattenConversationRows(
      props.items,
      props.store.currentTurns(),
      (id) => props.store.isTurnExpanded(id),
    ),
    { equals: conversationRowsEqual },
  );
  const textOverrides = signal<Record<string, string>>({});
  effect(() => {
    const next: Record<string, string> = {};
    for (const item of props.items) {
      if (item.kind === "markdown" || item.kind === "reasoning") next[item.id] = item.text;
    }
    textOverrides.set(next);
  });
  const textFor = (item: MarkdownItem) => textOverrides()[item.id] ?? item.text;
  const userRows = () => rows().flatMap((row, index) => row.type === "item" && row.item.kind === "user"
    ? [{ row, index, user: row.item }]
    : []);
  const latestVisible = signal(false);
  const setLatestVisible = (visible: boolean) => {
    if (latestVisible() === visible) return;
    latestVisible.update(() => visible);
    props.onLatestVisibleChange?.(visible);
  };
  let distanceFromEnd = 0;
  let lastContentHeight = 0;
  let lastViewportHeight = 0;
  const activeTurn = signal(0);
  const hoveredTurn = signal<string | null>(null);
  const navFocus = signal<number | null>(null);
  const searchOpen = props.store.threadSearchOpen;
  const query = signal("");
  const activeMatch = signal(0);
  const viewportWidth = signal(0);
  const lightbox = signal<{ src: string; label: string } | null>(null);
  let list: ListRef | undefined;
  let visibleRowIndices: number[] = [];
  let followedSession: string | null = null;
  effect(() => {
    const session = props.store.current();
    if (session === followedSession) return;
    followedSession = session;
    setLatestVisible(false);
    const timer = setTimeout(() => list?.scrollToEnd({ animated: false }), 40);
    return () => clearTimeout(timer);
  });
  const updateActiveTurn = () => {
    const next = resolveActiveUserRow(
      userRows().map((entry) => entry.index),
      visibleRowIndices,
      !latestVisible(),
    );
    if (next !== null) activeTurn.set(next);
  };
  effect(updateActiveTurn);
  const matches = () => {
    const normalized = query().trim().toLowerCase();
    if (!normalized) return [];
    const itemMatches = (item: ChatItem) => {
      if (item.kind === "user" || item.kind === "markdown" || item.kind === "reasoning" || item.kind === "note") {
        return item.text.toLowerCase().includes(normalized);
      }
      if (item.kind === "tool") return `${item.tool} ${item.summary} ${item.output}`.toLowerCase().includes(normalized);
      if (item.kind === "notification") {
        return item.notifications.some((notification) => `${notification.title} ${notification.body}`.toLowerCase().includes(normalized));
      }
      return false;
    };
    return rows().filter((row) => {
      if (row.type === "work") {
        return row.items.some(itemMatches);
      }
      if (row.type === "activity") {
        return row.items.some(itemMatches);
      }
      if (row.type !== "item") return false;
      return itemMatches(row.item);
    });
  };
  const goMatch = (direction: -1 | 1) => {
    const found = matches();
    if (!found.length) return;
    const next = (activeMatch() + direction + found.length) % found.length;
    activeMatch.set(next);
    const index = rows().findIndex((row) => row.id === found[next].id);
    if (index >= 0) {
      list?.scrollToIndex({ index, viewPosition: 0.5, animated: true });
    }
  };
  effect(() => keymap.bind({
    "ctrl+f": () => searchOpen.set(true),
    "cmd+f": () => searchOpen.set(true),
    escape: () => {
      if (lightbox()) lightbox.set(null);
      else if (searchOpen()) searchOpen.set(false);
    },
  }, { when: "always" }));
  const activeSearchRow = () => matches()[activeMatch()]?.id;
  const hovered = () => userRows().find((entry) => entry.user.id === hoveredTurn());
  const navBarTops = new Map<string, number>();
  const previewText = (userId: string) => {
    const start = props.items.findIndex((item) => item.id === userId);
    if (start < 0) return "";
    const nextUser = props.items.findIndex((item, index) => index > start && item.kind === "user");
    return props.items.slice(start + 1, nextUser < 0 ? undefined : nextUser)
      .flatMap((item) => item.kind === "markdown" ? [item.text] : [])
      .join("\n")
      .slice(0, 300);
  };
  let hoverOpenTimer: ReturnType<typeof setTimeout> | undefined;
  let hoverCloseTimer: ReturnType<typeof setTimeout> | undefined;
  const hoverIn = (id: string) => {
    if (hoverCloseTimer) clearTimeout(hoverCloseTimer);
    if (hoverOpenTimer) clearTimeout(hoverOpenTimer);
    hoverOpenTimer = setTimeout(() => hoveredTurn.set(id), 120);
  };
  const hoverOut = () => {
    if (hoverOpenTimer) clearTimeout(hoverOpenTimer);
    hoverCloseTimer = setTimeout(() => hoveredTurn.set(null), 80);
  };
  effect(() => () => {
    if (hoverOpenTimer) clearTimeout(hoverOpenTimer);
    if (hoverCloseTimer) clearTimeout(hoverCloseTimer);
  });
  return (
    <View
      class="relative min-h-0 min-w-0 flex-1"
      onLayout={({ bounds }) => viewportWidth.set(bounds.size.width)}
    >
      <List
        ref={(node) => { list = node; }}
        testID="message-list"
        each={rows()}
        keyExtractor={(row) => row.id}
        estimatedItemSize={28}
        class="min-h-0 min-w-0 flex-1"
        contentClass="gap-4"
        alignment="bottom"
        maintainVisibleContentPosition={{ autoscrollToBottomThreshold: FOLLOW_THRESHOLD }}
        scrollbar
        header={<View class="h-4" />}
        footer={<TailRows {...props} navEligible={userRows().length >= 2} />}
        onScroll={(event) => {
          props.onScroll?.(event);
          lastContentHeight = event.contentSize.height;
          lastViewportHeight = event.size.height;
          distanceFromEnd = event.contentSize.height - event.offset.y - event.size.height;
          setLatestVisible(distanceFromEnd > FOLLOW_THRESHOLD);
        }}
        onViewableItemsChange={({ viewableItems }) => {
          visibleRowIndices = viewableItems.filter((item) => item.isViewable).map((item) => item.index);
          updateActiveTurn();
        }}
      >
        {(row) => (
          <MessageRow
            {...props}
            row={row}
            textFor={textFor}
            active={row().id === activeSearchRow()}
            navEligible={userRows().length >= 2}
            onImage={(src, label) => lightbox.set({ src, label })}
            flat
          />
        )}
      </List>
      <If when={latestVisible()}>
        <View class="absolute inset-x-0 bottom-3 items-center" style={{ pointerEvents: "box-none" }}>
          <Button
            testID="latest-messages"
            class="h-[38px] flex-row items-center gap-2 rounded-full border border-border bg-popover px-4 shadow-lg hover:bg-accent"
            onPress={() => {
              if (distanceFromEnd > lastViewportHeight) {
                list?.scrollToOffset({
                  offset: Math.max(0, lastContentHeight - lastViewportHeight * 1.5),
                  animated: false,
                });
              }
              setTimeout(() => list?.scrollToEnd({ animated: true }), 40);
            }}
          >
            <Icon name="arrow-down" size={15} color={token("muted-foreground")} />
            <Text class="text-sm font-medium text-foreground">{t("messages.latest")}</Text>
          </Button>
        </View>
      </If>
      <If when={userRows().length >= 2 && viewportWidth() >= 720}>
        <View
          class="absolute left-2 top-0 z-20 justify-center"
          style={{ bottom: -(props.navFoot ?? 0) }}
        >
          <Each of={userRows()} key={(entry) => entry.user.id}>
            {(entry, index) => {
              const distance = () => {
                const focus = navFocus();
                return focus === null ? null : Math.abs(focus - index());
              };
              const scale = () => {
                const value = distance();
                return value === 0 ? 2.6 : value === 1 ? 1.7 : value === 2 ? 1.25 : 1;
              };
              const highlighted = () => distance() === 0 || (navFocus() === null && activeTurn() === index());
              const opacity = () => {
                const value = distance();
                if (value === 0) return 1;
                if (value === 1) return 0.86;
                if (value === 2) return 0.72;
                return navFocus() === null && activeTurn() === index() ? 0.9 : 0.58;
              };
              const barScale = createAnimatedValue(scale());
              const barOpacity = createAnimatedValue(opacity());
              let targetScale = scale();
              let targetOpacity = opacity();
              let wasReduceMotion = reduceMotion();
              let wasFocused = navFocus() !== null;
              effect(() => {
                const nextScale = scale();
                const nextOpacity = opacity();
                const reduced = reduceMotion();
                if (reduced === wasReduceMotion && nextScale === targetScale && nextOpacity === targetOpacity) return;
                const focused = navFocus() !== null;
                const animate = focused || wasFocused;
                wasReduceMotion = reduced;
                wasFocused = focused;
                targetScale = nextScale;
                targetOpacity = nextOpacity;
                if (reduced || !animate) {
                  barScale.set(nextScale);
                  barOpacity.set(nextOpacity);
                  return;
                }
                void barScale.spring(nextScale, { stiffness: 260, damping: 30, mass: 1 });
                void barOpacity.spring(nextOpacity, { stiffness: 260, damping: 30, mass: 1 });
              });
              effect(() => () => {
                void barScale.stop();
                void barOpacity.stop();
              });
              return (
                <View
                  class="relative"
                  onLayout={({ bounds }) => navBarTops.set(entry().user.id, bounds.origin.y)}
                  onMouseEnter={() => {
                    navFocus.set(index());
                    hoverIn(entry().user.id);
                  }}
                  onMouseLeave={() => {
                    if (navFocus() === index()) navFocus.set(null);
                    hoverOut();
                  }}
                >
                  <Button
                    testID={`turn-nav-${entry().user.id}`}
                    accessibilityLabel={t("messages.jump_turn", { index: index() + 1 })}
                    class="h-2.5 w-8 flex-row items-center"
                    onPress={() => list?.scrollToIndex({ index: entry().index, viewPosition: 0, animated: true })}
                  >
                    <View
                      class={highlighted() ? "h-0.5 rounded-full bg-foreground" : "h-0.5 rounded-full bg-muted-foreground/60"}
                      style={{ width: 12, opacity: barOpacity, scale: [barScale, 1], transformOrigin: "left" }}
                    />
                  </Button>
                </View>
              );
            }}
          </Each>
        </View>
      </If>
      <If when={hovered()}>
        {(value) => (
          <View
            testID="turn-nav-card"
            class="absolute left-12 z-30 w-80 gap-2 rounded-xl border border-border bg-popover p-3 shadow-xl pointer-events-none"
            style={{ top: navBarTops.get(value().user.id) ?? 0 }}
          >
            <Text style={{ numberOfLines: 2 }} class="text-sm text-foreground">{value().user.text}</Text>
            <Text style={{ numberOfLines: 3 }} class="text-sm text-muted-foreground">
              {previewText(value().user.id) || t("messages.no_text_reply")}
            </Text>
          </View>
        )}
      </If>
      <If when={searchOpen()}>
        <View class="absolute right-3 top-2 z-40 flex-row items-center gap-1 rounded-lg border border-border bg-popover p-1.5 shadow-xl">
          <Input
            testID="thread-search-input"
            autoFocus
            value={query()}
            onChange={({ value }) => {
              query.set(value);
              activeMatch.set(0);
            }}
            onSubmit={() => goMatch(1)}
            placeholder={t("messages.search_placeholder")}
            class="w-64 px-2 text-sm text-foreground"
          />
          <Text class="text-xs text-muted-foreground">{matches().length ? activeMatch() + 1 : 0}/{matches().length}</Text>
          <Button accessibilityLabel={t("messages.prev_result")} class="h-7 w-7 items-center justify-center" onPress={() => goMatch(-1)}>
            <Icon name="chevron-up" size={13} color={token("muted-foreground")} />
          </Button>
          <Button accessibilityLabel={t("messages.next_result")} class="h-7 w-7 items-center justify-center" onPress={() => goMatch(1)}>
            <Icon name="chevron-down" size={13} color={token("muted-foreground")} />
          </Button>
          <Button accessibilityLabel={t("messages.close_search")} class="h-7 w-7 items-center justify-center" onPress={() => searchOpen.set(false)}>
            <Icon name="x" size={13} color={token("muted-foreground")} />
          </Button>
        </View>
      </If>
      <ImageViewer
        images={lightbox() ? [lightbox()!] : []}
        index={lightbox() ? 0 : null}
        onIndexChange={() => {}}
        onClose={() => lightbox.set(null)}
      />
    </View>
  );
}
