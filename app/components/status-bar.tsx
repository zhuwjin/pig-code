import { Text, token, View, type JSX } from "nsh";
import { Popover, Progress } from "nsh/base";
import type { UiContext } from "../ui-context";
import { formatCompactDuration, formatTokens } from "../conversation";
import { Icon } from "./icon";

function DetailLine(props: { label: string; value: string }) {
  return (
    <View class="flex-row items-center gap-3">
      <Text class="flex-1 text-xs text-muted-foreground">{props.label}</Text>
      <Text class="font-mono text-xs text-foreground">{props.value}</Text>
    </View>
  );
}

export function StatusBar(props: UiContext) {
  const { store } = props;
  const running = () => {
    const id = store.current();
    return id ? (store.running()[id] ?? false) : false;
  };
  const rate = () => {
    const id = store.current();
    return id ? (store.outputRate()[id] ?? 0) : 0;
  };
  const usage = () => store.currentUsage();
  const totals = () => store.sessionTotals();
  const speed = () => totals().decodeMs > 0 ? totals().output / (totals().decodeMs / 1000) : null;
  const cacheHit = () => {
    const current = usage();
    const prompt = current ? current.cacheRead + current.input : 0;
    return current && prompt > 0 ? current.cacheRead / prompt * 100 : null;
  };
  const contextRatio = () => {
    const current = usage();
    return current ? Math.min(1, current.used / Math.max(1, current.total)) : 0;
  };
  const mode = (id: "activity" | "usage" | "context") => store.config()?.status_bar?.[id] ?? "detailed";
  const visible = () => running() || rate() > 0 ||
    (totals().turns > 0 && mode("activity") !== "hidden") ||
    (usage() && (mode("usage") !== "hidden" || mode("context") !== "hidden"));
  const popup = (id: "activity" | "usage" | "context", trigger: JSX.Element, content: JSX.Element) => {
    if (mode(id) === "hidden") return <View />;
    if (mode(id) === "compact") return trigger;
    return (
      <Popover
        open={store.isExpanded(`status-${id}`)}
        onOpenChange={(value) => {
          if (value !== store.isExpanded(`status-${id}`)) store.toggleExpanded(`status-${id}`);
        }}
        placement="top"
        align="center"
        popupClass="w-72 rounded-xl border border-border bg-popover p-3 shadow-xl"
        trigger={trigger}
      >
        {content}
      </Popover>
    );
  };
  return (
    <If when={visible()}>
      <View class="mx-auto flex min-h-8 w-full flex-row flex-wrap items-center justify-center gap-2 px-4 pt-1" style={{ maxWidth: 860 }}>
        <If when={running()}>
          <View class="flex-row items-center gap-1.5">
            <View class="h-1.5 w-1.5 rounded-full bg-success" />
            <Text class="text-xs text-muted-foreground">生成中</Text>
          </View>
        </If>
        <If when={running() && rate() > 0}>
          <Text class="text-xs text-muted-foreground">≈ {rate()} token/s</Text>
        </If>
        <If when={totals().turns > 0 && mode("activity") !== "hidden"}>
          {popup("activity",
            <View role="button" class="flex-row items-center gap-1 rounded-full px-1.5 py-0.5 hover:bg-accent">
              <Icon name="gauge" size={13} color={token("muted-foreground")} />
              <Text class="text-xs text-muted-foreground">
                {totals().turns} 回合 · {totals().steps} 步{speed() ? ` · ${Math.round(speed()!)} tok/s` : ""}
              </Text>
            </View>,
            <View class="gap-2">
              <Text class="text-sm font-medium text-foreground">会话活动</Text>
              <View class="h-px bg-border" />
              <DetailLine label="已完成回合" value={String(totals().turns)} />
              <DetailLine label="API 步数" value={String(totals().steps)} />
              <DetailLine label="模型累计耗时" value={formatCompactDuration(totals().apiMs)} />
              <DetailLine label="工具累计耗时" value={formatCompactDuration(totals().toolMs)} />
              <DetailLine label="首 Token 平均延迟" value={totals().ttftSteps ? formatCompactDuration(totals().ttftMs / totals().ttftSteps) : "—"} />
              <DetailLine label="加权解码速度" value={speed() ? `${speed()!.toFixed(1)} tok/s` : "—"} />
            </View>,
          )}
        </If>
        <If when={usage()}>
          {popup("usage",
            <View role="button" class="flex-row items-center gap-1 rounded-full px-1.5 py-0.5 hover:bg-accent">
              <Icon name="database" size={13} color={token("muted-foreground")} />
              <Text class="text-xs text-muted-foreground">
                {formatTokens((usage()?.input ?? 0) + (usage()?.cacheRead ?? 0) + totals().output)} tokens
                {cacheHit() != null ? ` · 缓存 ${cacheHit()!.toFixed(1)}%` : ""}
              </Text>
            </View>,
            <View class="gap-2">
              <Text class="text-sm font-medium text-foreground">会话 Token 用量</Text>
              <View class="h-px bg-border" />
              <DetailLine label="输入" value={(usage()?.input ?? 0).toLocaleString()} />
              <DetailLine label="缓存读取" value={(usage()?.cacheRead ?? 0).toLocaleString()} />
              <DetailLine label="输出" value={totals().output.toLocaleString()} />
              <DetailLine label="缓存命中率" value={cacheHit() == null ? "—" : `${cacheHit()!.toFixed(1)}%`} />
            </View>,
          )}
          {popup("context",
            <View role="button" class="flex-row items-center gap-1 rounded-full px-1.5 py-0.5 hover:bg-accent">
              <View class={contextRatio() > 0.8 ? "h-3 w-3 rounded-full border-2 border-warning" : "h-3 w-3 rounded-full border-2 border-info"} />
              <Text class="text-xs text-muted-foreground">{Math.round(contextRatio() * 100)}%</Text>
            </View>,
            <View class="gap-2">
              <View class="flex-row items-center">
                <Text class="flex-1 text-sm font-medium text-foreground">上下文已使用 {Math.round(contextRatio() * 100)}%</Text>
                <Text class="font-mono text-xs text-muted-foreground">~{formatTokens(usage()?.used ?? 0)} / {formatTokens(usage()?.total ?? 0)}</Text>
              </View>
              <Progress value={contextRatio() * 100} class="h-1.5" />
              <If when={store.currentBreakdown()}>
                {(value) => (
                  <View class="gap-1.5">
                    <DetailLine label="系统提示（估算）" value={`~${formatTokens(value().system)}`} />
                    <DetailLine label="工具 Schema（估算）" value={`~${formatTokens(value().tools)}`} />
                    <DetailLine label="对话（估算）" value={`~${formatTokens(value().messages)}`} />
                  </View>
                )}
              </If>
            </View>,
          )}
        </If>
      </View>
    </If>
  );
}
