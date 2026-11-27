import {
  Animated,
  colorScheme,
  createAnimatedValue,
  Easing,
  effect,
  reduceMotion,
  signal,
  Text,
  token,
  View,
} from "nsh";
import { shimmerHighlightColor, shimmerLayerOpacity } from "./shimmer-color";

const LAYER_COUNT = 12;

export function shimmerLayerX(progress: number, width: number, padding: number, radius: number): number {
  return progress * (width + 2 * padding) - padding - radius;
}

export function ShimmerText(props: {
  children: string;
  active?: boolean;
  class?: string;
  textClass?: string;
  color?: string;
  highlightColor?: string;
  baseOpacity?: number;
  duration?: number;
  spread?: number;
  bandWidth?: number;
  testID?: string;
  accessibilityLabel?: string;
}) {
  const active = () => props.active ?? true;
  const duration = () => Math.max(1, props.duration ?? 2000);
  const spread = () => Math.min(1, Math.max(0.05, props.spread ?? 0.3));
  const baseOpacity = () => Math.min(1, Math.max(0, props.baseOpacity ?? 1));
  const progress = createAnimatedValue(0);
  const halfWidth = createAnimatedValue(0);
  const travel = createAnimatedValue(0);
  const padding = createAnimatedValue(0);
  const textWidth = signal(0);
  const halfWidthPx = signal(0);
  const measured = signal(false);
  const layers = Array.from({ length: LAYER_COUNT }, (_, index) => {
    const radius = halfWidth.mul(1 - index / LAYER_COUNT);
    const x = travel.mul(progress).sub(padding).sub(radius);
    return { index, x, counterX: x.mul(-1) };
  });
  let loop: ReturnType<typeof Animated.loop> | undefined;

  const highlightColor = () => shimmerHighlightColor(
    props.color ?? token("muted-foreground"),
    token("background"),
    token("foreground"),
    colorScheme() === "dark",
    props.highlightColor,
  );
  const layerOpacity = () => shimmerLayerOpacity(colorScheme() === "dark", LAYER_COUNT);

  effect(() => {
    if (!active() || reduceMotion()) return;
    loop?.stop();
    progress.set(0);
    loop = Animated.loop(
      Animated.timing(progress, {
        toValue: 1,
        duration: duration(),
        easing: Easing.linear,
      }),
      { resetBeforeIteration: true },
    );
    loop.start();
    return () => loop?.stop();
  });

  return (
    <View
      testID={props.testID}
      accessible
      accessibilityLabel={props.accessibilityLabel ?? props.children}
      class={props.class ?? "shrink-0"}
      style={{ position: "relative", overflow: "hidden" }}
      onLayout={({ bounds }) => {
        const width = bounds.size.width;
        const band = props.bandWidth === undefined ? width * spread() : Math.max(1, props.bandWidth);
        textWidth.set(width);
        halfWidthPx.set(band);
        halfWidth.set(band);
        padding.set(band + width * 0.05);
        travel.set(width + 2 * (band + width * 0.05));
        measured.set(width > 0);
      }}
    >
      <Text
        class={props.textClass ?? "text-sm leading-5"}
        style={{ color: props.color ?? token("muted-foreground"), numberOfLines: 1, opacity: baseOpacity() }}
      >
        {props.children}
      </Text>
      <If when={measured() && active() && !reduceMotion()}>
        <Each of={layers} key={(layer) => layer.index}>
          {(layer) => (
            <View
              testID={layer().index === 0 && props.testID ? `${props.testID}-highlight` : undefined}
              class="absolute left-0 top-0 h-full overflow-hidden"
              importantForAccessibility="no-hide-descendants"
              style={{
                width: halfWidthPx() * 2 * (1 - layer().index / LAYER_COUNT),
                opacity: layerOpacity(),
                pointerEvents: "none",
                transform: [{ translateX: layer().x }],
              }}
            >
              <View style={{ width: textWidth(), transform: [{ translateX: layer().counterX }] }}>
                <Text
                  class={props.textClass ?? "text-sm leading-5"}
                  style={{ width: textWidth(), color: highlightColor(), numberOfLines: 1 }}
                >
                  {props.children}
                </Text>
              </View>
            </View>
          )}
        </Each>
      </If>
    </View>
  );
}
