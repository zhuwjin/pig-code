import { Easing, effect, reduceMotion, signal, untracked, View, type JSX } from "nsh";

const FRAME_MS = 20;
const PROGRESS = Easing.sample(Easing.out(Easing.cubic), 15);
const MAX_MEASUREMENTS = 500;
const measurements = new Map<string, number>();

function readMeasurement(key: string | undefined): number {
  return key ? measurements.get(key) ?? 0 : 0;
}

function writeMeasurement(key: string | undefined, height: number): void {
  if (!key) return;
  measurements.delete(key);
  measurements.set(key, height);
  if (measurements.size > MAX_MEASUREMENTS) {
    measurements.delete(measurements.keys().next().value!);
  }
}

export function ListCollapsible(props: {
  open: boolean;
  content: JSX.Element;
  children: JSX.Element;
  class?: string;
  testID?: string;
  layoutVersion?: string | number;
  version?: string | number;
  onTransition?: (active: boolean) => void;
  onHeightChange?: (delta: number) => void;
}) {
  const open = () => props.open;
  const identity = () => props.testID;
  const measurementKey = () => identity()
    ? `${identity()}:${props.layoutVersion ?? ""}:${props.version ?? ""}`
    : undefined;
  const initialKey = measurementKey();
  const initialHeight = readMeasurement(initialKey);
  const shown = signal(open());
  const contentHeight = signal(initialHeight);
  const height = signal(open() ? initialHeight : 0);
  let initialized = false;
  let previousOpen = open();
  let previousIdentity = identity();
  let previousMeasurementKey = initialKey;
  let pending: boolean | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let generation = 0;
  let transitioning = false;

  const stop = () => {
    generation += 1;
    if (timer !== null) clearTimeout(timer);
    timer = null;
    if (transitioning) {
      transitioning = false;
      props.onTransition?.(false);
    }
  };

  const transition = (target: number) => {
    stop();
    const start = untracked(height);
    if (reduceMotion() || Math.abs(target - start) < 0.5) {
      pending = null;
      height.set(target);
      shown.set(open());
      return;
    }
    const currentGeneration = generation;
    transitioning = true;
    props.onTransition?.(true);
    const tick = (step: number) => {
      if (generation !== currentGeneration) return;
      const previous = untracked(height);
      const next = start + (target - start) * PROGRESS[step];
      height.set(next);
      props.onHeightChange?.(next - previous);
      if (step < PROGRESS.length - 1) {
        timer = setTimeout(() => tick(step + 1), FRAME_MS);
        return;
      }
      timer = null;
      pending = null;
      height.set(target);
      shown.set(open());
      stop();
    };
    timer = setTimeout(() => tick(1), FRAME_MS);
  };

  effect(() => {
    stop();
    const expanded = open();
    const currentIdentity = identity();
    const currentKey = measurementKey();
    if (currentIdentity !== previousIdentity) {
      previousIdentity = currentIdentity;
      previousMeasurementKey = currentKey;
      initialized = false;
      pending = null;
      const cached = readMeasurement(currentKey);
      contentHeight.set(cached);
      height.set(expanded ? cached : 0);
      shown.set(expanded);
      previousOpen = expanded;
    }
    if (currentKey !== previousMeasurementKey) {
      previousMeasurementKey = currentKey;
      if (initialized) {
        const cached = readMeasurement(currentKey);
        if (cached > 0) contentHeight.set(cached);
      }
    }
    const target = expanded ? contentHeight() : 0;
    if (!initialized) {
      initialized = true;
      previousOpen = expanded;
      shown.set(expanded);
      height.set(target);
      return stop;
    }
    if (expanded !== previousOpen) {
      previousOpen = expanded;
      pending = expanded;
      if (expanded) shown.set(true);
    }
    if (expanded && target === 0) return stop;
    if (pending !== null) transition(target);
    else {
      height.set(target);
      shown.set(expanded);
    }
    return stop;
  });

  return (
    <View class={props.class}>
      {props.children}
      <View
        testID={props.testID ? `${props.testID}-window` : undefined}
        class="overflow-hidden"
        style={{ height: height(), pointerEvents: shown() ? "auto" : "none" }}
        importantForAccessibility={shown() ? "yes" : "no-hide-descendants"}
      >
        <If when={shown()}>
          <View
            testID={props.testID ? `${props.testID}-content` : undefined}
            onLayout={({ bounds }) => {
              contentHeight.set(bounds.size.height);
              writeMeasurement(measurementKey(), bounds.size.height);
            }}
          >
            {props.content}
          </View>
        </If>
      </View>
    </View>
  );
}
