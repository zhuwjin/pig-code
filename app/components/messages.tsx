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
  Text,
  View,
  computed,
  effect,
  keymap,
  openUrl,
  signal,
  token,
} from "nsh";
import { Button, Input, TextView } from "nsh/base";
import { t, tCount } from "../i18n";
import type { ChatItem, MarkdownItem, NoteItem, NotificationItem, UserItem } from "../protocol";
import type { UiContext } from "../ui-context";
import {
  activitySummary,
  buildConversationRows,
  conversationRowsEqual,
  groupConversationActivity,
  reuseConversationRows,
  type ConversationRow,
  formatClock,
  formatDuration,
  formatTokens,
  mentionSegments,
  userMessageIsLong,
} from "../conversation";
import { Icon } from "./icon";
import { ImageViewer } from "./image-viewer";
import { ListCollapsible } from "./list-collapsible";
import {
  REASONING_TICKER_INTERVAL_MS,
  REASONING_TICKER_ROLL_MS,
  ReasoningTicker,
  reasoningTickerLine,
} from "./reasoning-ticker";
import { ShimmerText } from "./shimmer-text";
import { ToolRow } from "./tool-card";
import { TurnActionsRow, TurnChangesRow, WorkRow } from "./turn-rows";

type MessageProps = UiContext & {
  textFor: (item: MarkdownItem) => string;
  onImage: (source: string, label: string) => void;
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
        <View selectable class={long() && !open() ? "max-h-28 flex-row flex-wrap items-center overflow-hidden" : "flex-row flex-wrap items-center"}>
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
        <If when={long() && !open()}>
          <View class="items-center pt-1">
            <Button
              testID={`user-expand-${item().id}`}
              accessibilityLabel={t("messages.expand")}
              class="h-6 w-6 items-center justify-center rounded-full border border-border bg-background"
              onPress={() => props.store.toggleExpanded(`user-${item().id}`)}
            >
              <Icon name="chevron-down" size={13} color={token("muted-foreground")} />
            </Button>
          </View>
        </If>
        <If when={long() && open()}>
          <View class="items-center pt-1">
            <Button
              testID={`user-collapse-${item().id}`}
              accessibilityLabel={t("messages.collapse")}
              class="h-6 w-6 items-center justify-center rounded-full border border-border bg-background"
              onPress={() => props.store.toggleExpanded(`user-${item().id}`)}
            >
              <Icon name="chevron-up" size={13} color={token("muted-foreground")} />
            </Button>
          </View>
        </If>
      </View>
      <View class="flex-row items-center gap-1 opacity-0 group-hover:opacity-100">
        <Text class="mr-1 text-xs text-muted-foreground">{formatClock(item().sentAtMs)}</Text>
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
  const open = () => props.store.isExpanded(item().id);
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
  return (
    <ListCollapsible
      open={open()}
      testID={`thinking-${item().id}`}
      content={
        <View class="ml-2 mt-1 max-h-80 border-l border-border pl-3.5">
          <Scroll class="max-h-80">
            <Text style={{ userSelect: "text" }} class="text-sm leading-6 text-muted-foreground">{props.textFor(item())}</Text>
          </Scroll>
        </View>
      }
    >
      <View
        role="button"
        testID={`thinking-${item().id}`}
        accessibilityState={{ expanded: open() }}
        class="w-full flex-row items-center gap-2 py-1"
        onPress={() => props.store.toggleExpanded(item().id)}
      >
        <Icon name="brain" size={16} color={token("muted-foreground")} />
        <ShimmerText active={item().streaming} baseOpacity={item().streaming ? 0.6 : 1} class="shrink-0" textClass="text-sm leading-5">
          {item().streaming ? t("messages.thinking") : item().durationMs != null ? formatDuration(item().durationMs!, t("messages.thought_for")) : t("messages.thought_process")}
        </ShimmerText>
        <If when={item().streaming && !open() && reasoningTickerLine(props.textFor(item()))}>{ticker()}</If>
        <Icon name={open() ? "chevron-down" : "chevron-right"} size={14} color={token("muted-foreground")} />
      </View>
    </ListCollapsible>
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

function NotificationRow(props: UiContext & { item: Accessor<NotificationItem> }) {
  const item = () => props.item();
  return (
    <View class="gap-2">
      <Each of={item().notifications} key={(notification) => notification.id}>
        {(notification) => (
          <View class="overflow-hidden rounded-xl border border-border bg-tool-surface">
            <Button
              testID={`notification-${notification().id}`}
              class="flex-row items-start gap-3 px-3 py-3 hover:bg-accent"
              onPress={() => {
                if (notification().kind === "subagent" && notification().agent_id) {
                  props.store.openSubagent(notification().agent_id!);
                  return;
                }
                const task = props.store.currentTasks().find((candidate) => candidate.id === notification().task_id);
                if (task) props.store.openTask(task);
              }}
            >
              <Icon name={notification().kind === "subagent" ? "bot" : "square-terminal"} size={17} color={token("muted-foreground")} />
              <View class="min-w-0 flex-1 gap-1">
                <View class="flex-row items-center gap-2">
                  <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-left text-sm font-medium text-foreground">
                    {notification().title || notification().description}
                  </Text>
                  <Text class={notification().severity === "warning" ? "text-xs text-warning" : "text-xs text-success"}>
                    {notification().status === "completed" ? t("messages.notification.completed") : notification().status === "failed" ? t("messages.notification.failed") : notification().status}
                  </Text>
                </View>
                <Text style={{ userSelect: "text" }} class="text-left text-xs leading-5 text-muted-foreground">{notification().body}</Text>
                <If when={notification().output?.type === "preview"}>
                  <Text style={{ numberOfLines: 4 }} class="text-left font-mono text-xs text-foreground">
                    {(notification().output as { text?: string } | null | undefined)?.text ?? ""}
                  </Text>
                </If>
              </View>
            </Button>
          </View>
        )}
      </Each>
    </View>
  );
}

function ActivityRow(props: MessageProps & { row: Accessor<Extract<ConversationRow, { type: "activity" }>> }) {
  const row = () => props.row();
  const open = () => props.store.isExpanded(row().id);
  return (
    <ListCollapsible
      open={open()}
      testID={row().id}
      content={
        <View class="gap-2 pt-2">
          <Each of={row().items} key={(item) => item.id}>
            {(item) => <ItemMessageRow {...props} item={item} />}
          </Each>
        </View>
      }
    >
      <View
        role="button"
        testID={row().id}
        accessibilityState={{ expanded: open() }}
        class="w-full min-w-0 flex-row items-center gap-2 py-1"
        onPress={() => props.store.toggleExpanded(row().id)}
      >
        <Icon name="list-collapse" size={16} color={token("muted-foreground")} />
        <Text style={{ numberOfLines: 1 }} class="min-w-0 shrink text-sm text-muted-foreground">
          {activitySummary(row().items)}
        </Text>
        <Icon name={open() ? "chevron-down" : "chevron-right"} size={14} color={token("muted-foreground")} />
      </View>
    </ListCollapsible>
  );
}

function ItemMessageRow(props: MessageProps & { item: Accessor<ChatItem> }) {
  const item = () => props.item();
  return (
    <View>
      <If when={item().kind === "user"}>
        <UserRow {...props} item={() => item() as UserItem} />
      </If>
      <If when={item().kind === "reasoning"}>
        <ReasoningRow {...props} item={() => item() as MarkdownItem} />
      </If>
      <If when={item().kind === "markdown"}>
        <MarkdownRow {...props} item={() => item() as MarkdownItem} />
      </If>
      <If when={item().kind === "tool"}>
        <ToolRow {...props} item={() => item() as Extract<ChatItem, { kind: "tool" }>} />
      </If>
      <If when={item().kind === "note"}>
        <NoteRow {...props} item={() => item() as NoteItem} />
      </If>
      <If when={item().kind === "notification"}>
        <NotificationRow {...props} item={() => item() as NotificationItem} />
      </If>
    </View>
  );
}

function MessageRow(props: MessageProps & { row: Accessor<ConversationRow>; active: boolean; navEligible: boolean }) {
  const row = () => props.row();
  return (
    <View
      class={props.active
        ? `w-full rounded-lg bg-warning/10 ${props.navEligible ? "px-12" : ""}`
        : `w-full ${props.navEligible ? "px-12" : ""}`}
    >
      <View class="mx-auto w-full max-w-[860px] px-4">
        <If when={row().type === "item"}>
          <ItemMessageRow {...props} item={() => (row() as Extract<ConversationRow, { type: "item" }>).item} />
        </If>
        <If when={row().type === "activity"}>
          <ActivityRow {...props} row={() => row() as Extract<ConversationRow, { type: "activity" }>} />
        </If>
        <If when={row().type === "work"}>
          <WorkRow {...props} turn={() => (row() as Extract<ConversationRow, { type: "work" }>).turn}>
            <View class="gap-2 pt-4">
              <Each of={groupConversationActivity((row() as Extract<ConversationRow, { type: "work" }>).items)} key={(activity) => activity.id}>
                {(activity) => (
                  <>
                    <If when={activity().type === "item"}>
                      <ItemMessageRow {...props} item={() => (activity() as Extract<ConversationRow, { type: "item" }>).item} />
                    </If>
                    <If when={activity().type === "activity"}>
                      <ActivityRow {...props} row={() => activity() as Extract<ConversationRow, { type: "activity" }>} />
                    </If>
                  </>
                )}
              </Each>
            </View>
          </WorkRow>
        </If>
        <If when={row().type === "changes"}>
          <TurnChangesRow {...props} turn={() => (row() as Extract<ConversationRow, { type: "changes" }>).turn} />
        </If>
        <If when={row().type === "actions"}>
          <TurnActionsRow
            {...props}
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
        <If when={running()}>
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
              <Text class="flex-1 text-xs text-muted-foreground">{retryLabel(value())}</Text>
              <Button testID="retry-now" class="rounded-full border border-border px-2 py-0.5" onPress={() => props.store.retryNow()}>
                <Text class="text-xs text-foreground">{t("messages.retry_now")}</Text>
              </Button>
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

export function MessageList(props: UiContext & { items: ChatItem[] }) {
  let cachedSession: string | null = null;
  let cachedRows: ConversationRow[] = [];
  const rows = computed(
    () => {
      const session = props.store.current();
      cachedRows = reuseConversationRows(
        cachedSession === session ? cachedRows : [],
        buildConversationRows(props.items, props.store.currentTurns()),
      );
      cachedSession = session;
      return cachedRows;
    },
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
  const activeTurn = signal(0);
  const hoveredTurn = signal<string | null>(null);
  const searchOpen = props.store.threadSearchOpen;
  const query = signal("");
  const activeMatch = signal(0);
  const viewportWidth = signal(0);
  const lightbox = signal<{ src: string; label: string } | null>(null);
  let list: ListRef | undefined;
  let followedSession: string | null = null;
  effect(() => {
    const session = props.store.current();
    if (session === followedSession) return;
    followedSession = session;
    latestVisible.set(false);
    const timer = setTimeout(() => list?.scrollToEnd({ animated: false }), 0);
    return () => clearTimeout(timer);
  });
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
      if (row.type === "work" || row.type === "activity") return row.items.some(itemMatches);
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
        maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 120 }}
        class="min-w-0 flex-1"
        contentClass="gap-4"
        scrollbar
        header={<View class="h-4" />}
        footer={<TailRows {...props} navEligible={userRows().length >= 2} />}
        onScroll={(event) => {
          const distance = event.contentSize.height - event.offset.y - event.size.height;
          latestVisible.set(distance > 1);
        }}
        onViewableItemsChange={({ viewableItems }) => {
          const first = viewableItems[0]?.index ?? 0;
          const users = userRows();
          let active = 0;
          users.forEach((entry, index) => {
            if (entry.index <= first) active = index;
          });
          activeTurn.set(active);
        }}
      >
        {(row) => <MessageRow {...props} row={row} textFor={textFor} active={row().id === activeSearchRow()} navEligible={userRows().length >= 2} onImage={(src, label) => lightbox.set({ src, label })} />}
      </List>
      <If when={latestVisible()}>
        <View class="absolute inset-x-0 bottom-3 items-center" style={{ pointerEvents: "box-none" }}>
          <Button
            testID="latest-messages"
            class="h-[38px] flex-row items-center gap-2 rounded-full border border-border bg-popover px-4 shadow-lg"
            onPress={() => {
              latestVisible.set(false);
              setTimeout(() => list?.scrollToEnd({ animated: true }), 40);
            }}
          >
            <Icon name="arrow-down" size={15} color={token("muted-foreground")} />
            <Text class="text-sm font-medium text-foreground">{t("messages.latest")}</Text>
          </Button>
        </View>
      </If>
      <If when={userRows().length >= 2 && viewportWidth() >= 720}>
        <View class="absolute bottom-24 left-2 top-1/2 z-20 justify-center gap-1.5">
          <Each of={userRows()} key={(entry) => entry.user.id}>
            {(entry, index) => (
              <View
                class="relative"
                onMouseEnter={() => hoverIn(entry().user.id)}
                onMouseLeave={hoverOut}
              >
                <Button
                  testID={`turn-nav-${entry().user.id}`}
                  accessibilityLabel={t("messages.jump_turn", { index: index() + 1 })}
                  class={activeTurn() === index() ? "h-7 w-1.5 rounded-full bg-muted-foreground" : "h-5 w-1 rounded-full bg-muted-foreground/40 hover:w-1.5 hover:bg-muted-foreground"}
                  onPress={() => list?.scrollToIndex({ index: entry().index, viewPosition: 0, animated: true })}
                />
              </View>
            )}
          </Each>
        </View>
      </If>
      <If when={hovered()}>
        {(value) => (
          <View class="absolute bottom-24 left-8 z-30 w-80 gap-2 rounded-xl border border-border bg-popover p-3 shadow-xl pointer-events-none">
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
