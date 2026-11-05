import { Accessor, Clipboard, Text, token, View, type JSX } from "nsh";
import { Button, Collapsible } from "nsh/base";
import { t } from "../i18n";
import type { TurnState } from "../protocol";
import type { UiContext } from "../ui-context";
import { changeTotals, formatClock, formatCompactDuration, formatDuration } from "../conversation";
import { Icon } from "./icon";
import { PluginSlot } from "../plugins/slots";

export function WorkRow(props: UiContext & { turn: Accessor<TurnState>; children: JSX.Element }) {
  const turn = () => props.turn();
  const open = () => props.store.isTurnExpanded(turn().id);
  return (
    <Collapsible open={open()} content={props.children}>
      <Button
        testID={`work-${turn().id}`}
        class="flex-row items-center gap-1 self-start py-1"
        onPress={() => props.store.toggleTurnExpanded(turn().id)}
      >
        <Text class="text-xs text-muted-foreground">
          {turn().status === "stopped" ? t("turn.stopped") : formatDuration(turn().durationMs ?? 0, t("turn.worked_for"))}
        </Text>
        <Icon name={open() ? "chevron-down" : "chevron-right"} size={13} color={token("muted-foreground")} />
      </Button>
    </Collapsible>
  );
}

export function TurnChangesRow(props: UiContext & { turn: Accessor<TurnState> }) {
  const turn = () => props.turn();
  const open = () => props.store.isExpanded(`turn-changes-${turn().id}`);
  const totals = () => changeTotals(turn().changes);
  const visible = () => turn().changes.slice(0, open() ? undefined : 3);
  return (
    <View class="overflow-hidden rounded-xl border border-border bg-tool-surface">
      <View class="flex-row items-center gap-2 px-3 py-2.5">
        <Icon name="file-pen" size={16} color={token("muted-foreground")} />
        <Text class="flex-1 text-sm font-medium text-muted-foreground">{t("turn.files_changed", { count: turn().changes.length })}</Text>
        <Text class="font-mono text-xs text-success">+{totals().additions}</Text>
        <Text class="font-mono text-xs text-destructive">-{totals().deletions}</Text>
      </View>
      <View class="border-t border-border py-1">
        <Each of={visible()} key={(change) => change.path}>
          {(change) => (
            <View class="group flex-row items-center gap-2 px-3 py-1.5 hover:bg-accent">
              <Button
                class="min-w-0 flex-1"
                onPress={() => props.store.openFile(change().path)}
              >
                <Text style={{ numberOfLines: 1 }} class="text-left font-mono text-xs text-muted-foreground group-hover:text-info group-hover:underline">
                  {change().path}
                </Text>
              </Button>
              <Text class="font-mono text-xs text-success">+{change().additions}</Text>
              <Text class="font-mono text-xs text-destructive">-{change().deletions}</Text>
              <Button
                testID={`revert-${turn().id}-${change().path}`}
                accessibilityLabel={t("turn.restore", { path: change().path })}
                class="rounded px-1.5 py-0.5 opacity-0 hover:bg-muted group-hover:opacity-100"
                onPress={() => props.store.revertFile(change().path)}
              >
                <Icon name="undo-2" size={12} color={token("muted-foreground")} />
              </Button>
            </View>
          )}
        </Each>
      </View>
      <If when={turn().changes.length > 3}>
        <Button
          class="flex-row items-center justify-center gap-1 border-t border-border py-1.5 hover:bg-accent"
          onPress={() => props.store.toggleExpanded(`turn-changes-${turn().id}`)}
        >
          <Text class="text-xs text-muted-foreground">
            {open() ? t("common.collapse") : t("turn.more_files", { count: turn().changes.length - 3 })}
          </Text>
          <Icon name={open() ? "chevron-up" : "chevron-down"} size={12} color={token("muted-foreground")} />
        </Button>
      </If>
    </View>
  );
}

export function TurnActionsRow(props: UiContext & {
  turn: Accessor<TurnState>;
  markdownText: Accessor<string>;
}) {
  const turn = () => props.turn();
  const stats = () => turn().stats;
  const latest = () => {
    const turns = props.store.currentTurns().filter((candidate) => candidate.status !== "running");
    return turns[turns.length - 1]?.id === turn().id;
  };
  const hooksOpen = () => props.store.isExpanded(`hooks-${turn().id}`);
  const copied = () => props.store.copiedItems()[`turn-copy-${turn().id}`] ?? false;
  return (
    <View class="group gap-2">
      <If when={turn().finishReason === "length" || turn().finishReason === "content_filter"}>
        <View class="flex-row items-center gap-1">
          <Icon name="shield-alert" size={12} color={token("warning")} />
          <Text class="text-xs text-warning">
            {t(turn().finishReason === "length" ? "turn.truncated" : "turn.filtered")}
          </Text>
        </View>
      </If>
      <View class={latest() ? "flex-row items-center gap-1" : "flex-row items-center gap-1 opacity-0 group-hover:opacity-100"}>
        <If when={props.markdownText()}>
          <Button
            testID={`copy-turn-${turn().id}`}
            accessibilityLabel={t("turn.copy_answer")}
            class="h-5 w-5 items-center justify-center rounded hover:bg-accent"
            onPress={() => {
              void Clipboard.writeText(props.markdownText()).then(() => props.store.markCopied(`turn-copy-${turn().id}`));
            }}
          >
            <Icon name={copied() ? "check" : "copy"} size={12} color={token(copied() ? "success" : "muted-foreground")} />
          </Button>
        </If>
        <Button
          testID={`fork-${turn().id}`}
          accessibilityLabel={t("turn.fork")}
          class="h-5 w-5 items-center justify-center rounded hover:bg-accent"
          onPress={() => props.store.forkTurn(turn().id)}
        >
          <Icon name="git-fork" size={12} color={token("muted-foreground")} />
        </Button>
        <If when={turn().hooks.length > 0}>
          <Button
            testID={`hooks-${turn().id}`}
            accessibilityLabel={t("turn.hooks_summary")}
            class="h-5 w-5 items-center justify-center rounded hover:bg-accent"
            onPress={() => props.store.toggleExpanded(`hooks-${turn().id}`)}
          >
            <Icon name="anchor" size={12} color={token("muted-foreground")} />
          </Button>
        </If>
        <PluginSlot {...props} slot="message" context={{ sessionId: props.store.current(), turnId: turn().id }} />
        <If when={stats()}>
          <Text class="ml-1 text-xs text-muted-foreground">{formatClock(stats()!.ended_at_ms)}</Text>
        </If>
        <If when={!stats() && turn().durationMs}>
          <Text class="text-xs text-muted-foreground">{t("turn.finished", { duration: formatCompactDuration(turn().durationMs!) })}</Text>
        </If>
      </View>
      <Collapsible open={hooksOpen()} content={
        <View class="gap-2 rounded-xl border border-border bg-tool-surface p-3">
          <Each of={turn().hooks} key={(hook) => hook.id}>
            {(hook) => (
              <View class="gap-0.5">
                <View class="flex-row items-center gap-2">
                  <Text class="flex-1 text-xs font-medium text-foreground">{hook().label}</Text>
                  <Text class="text-xs text-muted-foreground">{formatCompactDuration(hook().duration_ms)}</Text>
                  <Text class={hook().state === "failed" || hook().state === "blocked" ? "text-xs text-destructive" : "text-xs text-success"}>
                    {t(hook().state === "finished" ? "turn.hook.finished" : hook().state === "blocked" ? "turn.hook.blocked" : "turn.hook.failed")}
                  </Text>
                </View>
                <Text class="text-xs text-muted-foreground">{hook().hook_event}</Text>
                <If when={hook().reason}><Text class="text-xs text-warning">{hook().reason}</Text></If>
              </View>
            )}
          </Each>
        </View>
      }>
        <View />
      </Collapsible>
    </View>
  );
}
