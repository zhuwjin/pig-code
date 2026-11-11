import { Canvas, Text, View, effect, signal } from "nsh";
import { t } from "../i18n";
import type { ProcessSample } from "../protocol";

const FPS_WINDOW_MS = 500;
const PROCESS_WINDOW_MS = 1000;
const INTERVAL_SAMPLES = 120;
const FRAME_WARMUP = 4;
const MEASURE_FRAMES = false;
const SHOW_TRACE = false;

const COLORS = {
  panel: "rgba(24, 24, 27, 0.94)",
  border: "#3f3f46",
  label: "#a1a1aa",
  value: "#f4f4f5",
  good: "#4ade80",
  warn: "#fbbf24",
  bad: "#f87171",
};

interface Readings {
  fps: number;
  interval: number | null;
  p95: number | null;
  drop: number;
  budget: number;
}

function percentile(sorted: number[], q: number): number {
  const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(sorted.length * q) - 1));
  return sorted[index];
}

function formatMs(value: number | null): string {
  return value === null
    ? t("settings.developer.hud.unavailable")
    : t("settings.developer.hud.ms", { value: value.toFixed(1) });
}

function formatPercent(value: number | null | undefined): string {
  return value === null || value === undefined
    ? t("settings.developer.hud.unavailable")
    : t("settings.developer.hud.percent", { value: Math.round(value) });
}

function formatMemory(value: number | null | undefined): string {
  return value === null || value === undefined
    ? t("settings.developer.hud.unavailable")
    : t("settings.developer.hud.mb", { value: Math.round(value / 1_000_000) });
}

function grade(value: number, budget: number): string {
  if (value <= budget) return COLORS.good;
  if (value <= budget * 2) return COLORS.warn;
  return COLORS.bad;
}

function label(text: string) {
  return <Text class="font-mono text-[10px]" style={{ color: COLORS.label, letterSpacing: 0.6 }}>{text}</Text>;
}

export function FpsMeter(props: { sample: () => ProcessSample }) {
  const readings = signal<Readings>({ fps: 0, interval: null, p95: null, drop: 0, budget: 16.7 });
  const trace = signal<number[]>([]);
  const process = signal<ProcessSample>({});
  const intervals: number[] = [];
  let frame: number | undefined;
  let lastFrame: number | null = null;
  let warmup = FRAME_WARMUP;
  let windowStart: number | null = null;
  let windowFrames = 0;

  effect(() => {
    process.set(props.sample());
    const timer = setInterval(() => process.set(props.sample()), PROCESS_WINDOW_MS);
    return () => clearInterval(timer);
  });

  effect(() => {
    if (!MEASURE_FRAMES) return;
    const tick = (time: number) => {
      if (lastFrame !== null) {
        const gap = time - lastFrame;
        if (gap >= 4 && gap <= 100) {
          if (warmup > 0) warmup -= 1;
          else {
            intervals.push(gap);
            if (intervals.length > INTERVAL_SAMPLES) intervals.shift();
          }
        }
      }
      lastFrame = time;

      if (windowStart === null) windowStart = time;
      windowFrames += 1;
      const elapsed = time - windowStart;
      if (elapsed >= FPS_WINDOW_MS) {
        const fps = (windowFrames * 1000) / elapsed;
        const sorted = [...intervals].sort((a, b) => a - b);
        const budget = sorted.length > 0
          ? Math.max(4, Math.min(16.7, percentile(sorted, 0.5)))
          : 16.7;
        const p95 = sorted.length > 0 ? percentile(sorted, 0.95) : null;
        const dropped = intervals.filter((value) => value > budget * 1.5).length;

        trace.set([...intervals]);
        readings.set({
          fps,
          interval: fps > 0 ? 1000 / fps : null,
          p95,
          drop: intervals.length > 0 ? (dropped / intervals.length) * 100 : 0,
          budget,
        });

        windowStart = time;
        windowFrames = 0;
      }

      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => {
      if (frame !== undefined) cancelAnimationFrame(frame);
      frame = undefined;
    };
  });

  const p95Color = () => {
    const value = readings().p95;
    return value === null ? COLORS.label : grade(value, readings().budget);
  };
  const dropColor = () => {
    const value = readings().drop;
    return value > 5 ? COLORS.bad : value > 0 ? COLORS.warn : COLORS.good;
  };

  return (
    <View
      testID="fps-meter"
      class={`pointer-events-none absolute bottom-2 right-2 z-50 gap-1 rounded-lg border px-3 py-2.5 shadow-xl ${MEASURE_FRAMES ? "w-[268px]" : "w-[196px]"}`}
      style={{ backgroundColor: COLORS.panel, borderColor: COLORS.border }}
    >
      <If when={MEASURE_FRAMES}>
        <View class="relative h-[68px] w-full">
          <If when={SHOW_TRACE}>
            <Canvas
              testID="fps-meter-trace"
              class="absolute inset-0"
              style={{ opacity: 0.75 }}
              draw={(cx, { width, height }) => {
                const values = trace();
                const budget = readings().budget;
                if (values.length < 2 || width <= 0 || height <= 0) return;
                const max = Math.max(budget * 2, ...values);
                const bottom = height - 7;
                const top = 9;
                const step = width / INTERVAL_SAMPLES;
                const leading = INTERVAL_SAMPLES - values.length;
                let previousX = leading * step;
                let previousY = bottom - (Math.min(values[0], max) / max) * (bottom - top);
                cx.lineWidth = 1;
                for (let index = 1; index < values.length; index += 1) {
                  const value = values[index];
                  const x = (leading + index) * step;
                  const y = bottom - (Math.min(value, max) / max) * (bottom - top);
                  cx.strokeStyle = grade(value, budget);
                  cx.beginPath();
                  cx.moveTo(previousX, previousY);
                  cx.lineTo(x, y);
                  cx.stroke();
                  previousX = x;
                  previousY = y;
                }
              }}
            />
          </If>
          <View class="absolute inset-0 flex-row items-center justify-center gap-1.5">
            <Text
              testID="fps-meter-fps"
              class="font-mono text-[36px]"
              style={{ color: COLORS.value, lineHeight: 36 }}
            >
              {Math.round(readings().fps)}
            </Text>
            {label(t("settings.developer.hud.fps"))}
          </View>
        </View>
        <View class="gap-1 pt-1">
          <View class="flex-row items-baseline justify-between">
            {label(t("settings.developer.hud.interval"))}
            <Text testID="fps-meter-interval" class="font-mono text-[11px]" style={{ color: COLORS.value }}>
              {formatMs(readings().interval)}
            </Text>
          </View>
          <View class="flex-row items-baseline justify-between">
            {label(t("settings.developer.hud.p95"))}
            <Text testID="fps-meter-p95" class="font-mono text-[11px]" style={{ color: p95Color() }}>
              {formatMs(readings().p95)}
            </Text>
          </View>
          <View class="flex-row items-baseline justify-between">
            {label(t("settings.developer.hud.drop"))}
            <Text testID="fps-meter-drop" class="font-mono text-[11px]" style={{ color: dropColor() }}>
              {t("settings.developer.hud.percent", { value: readings().drop.toFixed(1) })}
            </Text>
          </View>
        </View>
      </If>
      <View class="flex-row items-baseline justify-between">
        <View class="flex-row items-baseline gap-1.5">
          {label(t("settings.developer.hud.cpu"))}
          <Text testID="fps-meter-cpu" class="font-mono text-[11px]" style={{ color: COLORS.value }}>
            {formatPercent(process().cpu)}
          </Text>
        </View>
        <View class="flex-row items-baseline gap-1.5">
          {label(t("settings.developer.hud.memory"))}
          <Text testID="fps-meter-memory" class="font-mono text-[11px]" style={{ color: COLORS.value }}>
            {formatMemory(process().memory)}
          </Text>
        </View>
      </View>
    </View>
  );
}
