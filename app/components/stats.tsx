import { signal, Text, token, View, type JSX } from "nsh";
import { Button, Popover } from "nsh/base";
import { Arc, Plot } from "nsh/base/plot";
import { formatCompactDuration, formatTokens } from "../conversation";
import { t, type MessageKey } from "../i18n";
import type { UiContext } from "../ui-context";
import { Icon, type IconName } from "./icon";

export function formatTokenReading(value: number): string {
  return `${formatTokens(value)} tok`;
}

export function formatSpeedReading(value: number | null, digits = 1): string {
  return value === null ? "—" : `${value.toFixed(digits)} tok/s`;
}

export function formatStatDuration(ms: number): string {
  const seconds = Math.max(0, ms) / 1000;
  if (seconds < 60) return formatCompactDuration(ms);
  const whole = Math.floor(seconds);
  return `${Math.floor(whole / 60)}m ${whole % 60}s`;
}

export function StatsPopover(props: {
  id: string;
  icon: IconName;
  label: string;
  title: string;
  total?: string;
  rows: { label: string; value: string }[];
  leading?: JSX.Element;
  align?: "start" | "center" | "end";
}) {
  const open = signal(false);
  return (
    <Popover
      open={open()}
      onOpenChange={({ open: value }) => open.set(value)}
      placement="top"
      align={props.align ?? "end"}
      offset={6}
      margin={8}
      popupClass="w-auto min-w-72 overflow-hidden rounded-xl border border-border bg-popover shadow-xl"
      trigger={
        <Button
          testID={props.id}
          accessibilityLabel={props.title}
          selected={open()}
          class="h-5 shrink-0 flex-row items-center gap-1.5 rounded px-1.5 hover:bg-accent selected:bg-accent"
        >
          {props.leading ?? <Icon name={props.icon} size={12} color={token("muted-foreground")} />}
          <Text class="shrink-0 text-xs text-muted-foreground">{props.label}</Text>
        </Button>
      }
    >
      <View testID={`${props.id}-panel`} class="gap-2 p-3">
        <View class="w-full flex-row items-center gap-2">
          <Icon name={props.icon} size={16} color={token("muted-foreground")} />
          <Text class="shrink-0 text-sm font-medium text-foreground">{props.title}</Text>
          <View class="flex-1" />
          <If when={props.total}>
            <Text class="shrink-0 font-mono text-sm font-medium text-foreground">{props.total}</Text>
          </If>
        </View>
        <View class="h-px w-full bg-border" />
        <Each of={props.rows} key={(row) => row.label}>
          {(row) => (
            <View class="w-full flex-row items-center gap-2">
              <Text class="shrink-0 text-xs text-muted-foreground">{row().label}</Text>
              <View class="flex-1" />
              <Text class="shrink-0 font-mono text-xs text-foreground">{row().value}</Text>
            </View>
          )}
        </Each>
      </View>
    </Popover>
  );
}

function ContextRing(props: { ratio: number; label: string }) {
  const ratio = () => Math.max(0, Math.min(1, props.ratio));
  return (
    <Plot
      testID="context-usage-ring"
      data={[]}
      class="h-3.5 w-3.5 shrink-0"
      interactive={false}
      accessibilityLabel={props.label}
      accessibilityValue={{ min: 0, max: 100, now: ratio() * 100 }}
    >
      <Arc innerRadius="65%" style={{ fill: token("border") }} />
      <If when={ratio() > 0}>
        <Arc
          innerRadius="65%"
          endAngle={ratio() * Math.PI * 2}
          cornerRadius={1}
          style={{ fill: token(ratio() > 0.8 ? "warning" : "muted-foreground") }}
        />
      </If>
    </Plot>
  );
}

export function SessionStatusBar(props: UiContext) {
  const totals = () => props.store.sessionTotals();
  const usage = () => props.store.currentUsage();
  const speed = () => {
    const current = totals();
    return current.decodeMs > 0 && current.output > 0
      ? current.output / (current.decodeMs / 1000)
      : null;
  };
  const activityLabel = () => {
    const current = totals();
    const activity = t("status.turns_steps", { turns: current.turns, steps: current.steps });
    return speed() === null ? activity : `${activity} · ${formatSpeedReading(speed(), 0)}`;
  };
  const activityRows = () => {
    const current = totals();
    const hasBreakdown = current.toolExecMs > 0 || current.toolWallMs > 0 || current.userWaitMs > 0 ||
      current.compactMs > 0 || current.hookMs > 0 || current.reviewMs > 0 || current.overheadMs > 0;
    const timingRows = hasBreakdown ? [
      { key: "status.tool_exec", value: current.toolExecMs },
      { key: "status.tool_wall", value: current.toolWallMs },
      { key: "status.user_wait", value: current.userWaitMs },
      { key: "status.review", value: current.reviewMs },
      { key: "status.hook_time", value: current.hookMs },
      { key: "status.compact", value: current.compactMs },
      { key: "status.overhead", value: current.overheadMs },
    ].filter((row) => row.value > 0).map((row) => ({
      label: t(row.key as MessageKey),
      value: formatStatDuration(row.value),
    })) : (current.toolMs > 0 ? [{ label: t("status.tool_time"), value: formatStatDuration(current.toolMs) }] : []);
    return [
      { label: t("status.completed_turns"), value: String(current.turns) },
      { label: t("status.api_steps"), value: String(current.steps) },
      { label: t("status.model_time"), value: formatStatDuration(current.apiMs) },
      ...timingRows,
      {
        label: t("status.first_token"),
        value: current.ttftMs > 0 && current.ttftSteps > 0
          ? formatStatDuration(current.ttftMs / current.ttftSteps)
          : "—",
      },
      { label: t("status.decode_speed"), value: formatSpeedReading(speed(), 0) },
    ];
  };
  const promptTokens = () => {
    const current = usage();
    return current ? current.cacheRead + current.input : 0;
  };
  const cacheHit = () => {
    const current = usage();
    const prompt = promptTokens();
    return current && prompt > 0 ? current.cacheRead / prompt * 100 : null;
  };
  const usageLabel = () => {
    const total = formatTokenReading(promptTokens() + totals().output);
    return cacheHit() === null
      ? total
      : `${total} · ${t("status.cache_suffix", { percent: cacheHit()!.toFixed(1) })}`;
  };
  const usageRows = () => {
    const current = usage()!;
    return [
      ...(cacheHit() === null ? [] : [{ label: t("status.cache_hit"), value: `${cacheHit()!.toFixed(1)}%` }]),
      { label: t("status.input"), value: formatTokenReading(current.input) },
      { label: t("status.cache_read"), value: formatTokenReading(current.cacheRead) },
      { label: t("status.output"), value: formatTokenReading(totals().output) },
    ];
  };
  const contextRatio = () => {
    const current = usage();
    return current ? Math.max(0, Math.min(1, current.used / Math.max(1, current.total))) : 0;
  };
  const contextPercent = () => Math.round(contextRatio() * 100);
  const contextRows = () => {
    const breakdown = props.store.currentBreakdown();
    if (!breakdown) return [];
    return [
      { label: t("status.system_prompt"), value: formatTokenReading(breakdown.system) },
      { label: t("status.tool_schema"), value: formatTokenReading(breakdown.tools) },
      { label: t("status.messages"), value: formatTokenReading(breakdown.messages) },
    ];
  };
  return (
    <If when={totals().turns > 0 || usage()}>
      <View testID="session-status" class="w-full flex-row flex-wrap items-center justify-center gap-2 px-1 pt-1">
        <If when={totals().turns > 0}>
          <StatsPopover
            id="session-activity"
            icon="gauge"
            label={activityLabel()}
            title={t("status.session_activity")}
            rows={activityRows()}
            align="center"
          />
        </If>
        <If when={usage()}>
          <StatsPopover
            id="session-usage"
            icon="database"
            label={usageLabel()}
            title={t("status.token_usage")}
            total={formatTokenReading(promptTokens() + totals().output)}
            rows={usageRows()}
            align="center"
          />
        </If>
        <If when={usage()}>
          {(current) => (
            <StatsPopover
              id="context-usage"
              icon="circle-gauge"
              label={`${contextPercent()}%`}
              title={t("status.context_used", { percent: contextPercent() })}
              total={`~${formatTokens(current().used)} / ${formatTokens(current().total)} tok`}
              rows={contextRows()}
              leading={<ContextRing ratio={contextRatio()} label={t("status.context_used", { percent: contextPercent() })} />}
              align="center"
            />
          )}
        </If>
      </View>
    </If>
  );
}
