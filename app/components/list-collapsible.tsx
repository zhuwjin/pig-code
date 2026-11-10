import { Easing, effect, reduceMotion, signal, untracked, View, type JSX } from "nsh";

const FRAME_MS = 20;
const PROGRESS = Easing.sample(Easing.out(Easing.cubic), 15);

export function ListCollapsible(props: {
  open: boolean;
  content: JSX.Element;
  children: JSX.Element;
  class?: string;
  testID?: string;
  onTransition?: (active: boolean) => void;
  onHeightChange?: (delta: number) => void;
}) {
  const open = () => props.open;
  const shown = signal(open());
  const contentHeight = signal(0);
  const height = signal(0);
  let initialized = false;
  let previousOpen = open();
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
            onLayout={({ bounds }) => contentHeight.set(bounds.size.height)}
          >
            {props.content}
          </View>
        </If>
      </View>
    </View>
  );
}
