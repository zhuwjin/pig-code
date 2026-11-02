import { Accessor, Clipboard, Text, token, View, type JSX } from "nsh";
import { Button, Collapsible, Popover } from "nsh/base";
import type { TurnState } from "../protocol";
import type { UiContext } from "../ui-context";
import { changeTotals, formatClock, formatCompactDuration, formatDuration, formatTokens, turnSpeed } from "../conversation";
import { Icon } from "./icon";

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
          {turn().status === "stopped" ? "已停止" : formatDuration(turn().durationMs ?? 0, "工作了 ")}
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
        <Text class="flex-1 text-sm font-medium text-muted-foreground">修改了 {turn().changes.length} 个文件</Text>
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
                <Text numberOfLines={1} class="text-left font-mono text-xs text-muted-foreground group-hover:text-info group-hover:underline">
                  {change().path}
                </Text>
              </Button>
              <Text class="font-mono text-xs text-success">+{change().additions}</Text>
              <Text class="font-mono text-xs text-destructive">-{change().deletions}</Text>
              <Button
                testID={`revert-${turn().id}-${change().path}`}
                accessibilityLabel={`还原 ${change().path}`}
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
            {open() ? "收起" : `还有 ${turn().changes.length - 3} 个文件`}
          </Text>
          <Icon name={open() ? "chevron-up" : "chevron-down"} size={12} color={token("muted-foreground")} />
        </Button>
      </If>
    </View>
  );
}

function StatsPopover(props: UiContext & { turn: Accessor<TurnState>; kind: "speed" | "usage" }) {
  const turn = () => props.turn();
  const stats = () => turn().stats;
  const open = () => props.store.isExpanded(`stats-${turn().id}-${props.kind}`);
  const speed = () => stats() ? turnSpeed(stats()!) : null;
  return (
    <Popover
      open={open()}
      onOpenChange={(value) => {
        if (value !== open()) props.store.toggleExpanded(`stats-${turn().id}-${props.kind}`);
      }}
      placement="top"
      align="start"
      popupClass="w-72 rounded-xl border border-border bg-popover p-3 shadow-xl"
      trigger={
        <View role="button" class="h-5 flex-row items-center gap-1 rounded px-1 hover:bg-accent">
          <Icon name={props.kind === "speed" ? "gauge" : "database"} size={12} color={token("muted-foreground")} />
          <Text class="text-xs text-muted-foreground">
            {props.kind === "speed"
              ? (speed() ? `${speed()!.toFixed(1)} tok/s` : formatCompactDuration(turn().durationMs ?? 0))
              : (stats() ? `${formatTokens(stats()!.input + stats()!.cache_read + stats()!.output)} tok` : "—")}
          </Text>
        </View>
      }
    >
      <If when={stats()}>
        {(value) => (
          <View class="gap-2">
            <Text class="text-sm font-medium text-foreground">{props.kind === "speed" ? "回合速度" : "回合 Token 用量"}</Text>
            <View class="h-px bg-border" />
            <StatLine label="回合耗时" value={formatDuration(turn().durationMs ?? 0)} />
            <StatLine label="API 步数" value={String(value().api_steps)} />
            <If when={props.kind === "speed"}>
              <StatLine label="模型耗时" value={formatCompactDuration(value().api_ms)} />
              <StatLine label="首 Token 平均延迟" value={value().ttft_steps ? formatCompactDuration(value().ttft_ms / value().ttft_steps) : "—"} />
              <StatLine label="工具累计耗时" value={formatCompactDuration(value().tool_ms)} />
              <StatLine label="纯解码速度" value={speed() ? `${speed()!.toFixed(1)} tok/s` : "—"} />
            </If>
            <If when={props.kind === "usage"}>
              <StatLine label="输入" value={value().input.toLocaleString()} />
              <StatLine label="缓存读取" value={value().cache_read.toLocaleString()} />
              <StatLine label="输出" value={value().output.toLocaleString()} />
              <StatLine
                label="缓存命中率"
                value={value().input + value().cache_read > 0
                  ? `${(value().cache_read / (value().input + value().cache_read) * 100).toFixed(1)}%`
                  : "—"}
              />
            </If>
          </View>
        )}
      </If>
    </Popover>
  );
}

function StatLine(props: { label: string; value: string }) {
  return (
    <View class="flex-row items-center gap-3">
      <Text class="flex-1 text-xs text-muted-foreground">{props.label}</Text>
      <Text class="font-mono text-xs text-foreground">{props.value}</Text>
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
            {turn().finishReason === "length" ? "回复因长度限制被截断" : "回复因内容过滤而停止"}
          </Text>
        </View>
      </If>
      <View class={latest() ? "flex-row items-center gap-1" : "flex-row items-center gap-1 opacity-0 group-hover:opacity-100"}>
        <If when={props.markdownText()}>
          <Button
            testID={`copy-turn-${turn().id}`}
            accessibilityLabel="复制回答"
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
          accessibilityLabel="从此回合 Fork"
          class="h-5 w-5 items-center justify-center rounded hover:bg-accent"
          onPress={() => props.store.forkTurn(turn().id)}
        >
          <Icon name="git-fork" size={12} color={token("muted-foreground")} />
        </Button>
        <If when={turn().hooks.length > 0}>
          <Button
            testID={`hooks-${turn().id}`}
            accessibilityLabel="查看 Hook 汇总"
            class="h-5 w-5 items-center justify-center rounded hover:bg-accent"
            onPress={() => props.store.toggleExpanded(`hooks-${turn().id}`)}
          >
            <Icon name="anchor" size={12} color={token("muted-foreground")} />
          </Button>
        </If>
        <If when={stats()}>
          <StatsPopover {...props} kind="speed" />
          <StatsPopover {...props} kind="usage" />
          <Text class="ml-1 text-xs text-muted-foreground">{formatClock(stats()!.ended_at_ms)}</Text>
        </If>
        <If when={!stats() && turn().durationMs}>
          <Text class="text-xs text-muted-foreground">回合结束 · {formatCompactDuration(turn().durationMs!)}</Text>
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
                    {hook().state === "finished" ? "完成" : hook().state === "blocked" ? "已阻止" : "失败"}
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
