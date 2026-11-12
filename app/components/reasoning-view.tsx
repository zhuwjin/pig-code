import { Accessor, Scroll, Text, View, computed, effect, signal, type ScrollRef } from "nsh";

const LINE_HEIGHT = 24;
const MAX_HEIGHT = 320;
const OVERSCAN = 2;
const CHARACTER_WIDTH = 7.2;

type Row = { index: number; text: string; top: number };
type Measurement = { text: string; height: number };

function textUnits(text: string): number {
  let width = 0;
  for (const character of text) {
    if (character === "\t") width += 4;
    else width += character.codePointAt(0)! > 0xff ? 2 : 1;
  }
  return width;
}

function estimateHeight(text: string, width: number): number {
  const columns = Math.max(1, Math.floor(width / CHARACTER_WIDTH));
  return Math.max(1, Math.ceil(textUnits(text) / columns)) * LINE_HEIGHT;
}

export function ReasoningView(props: { text: Accessor<string>; accessibilityLabel: string }) {
  const scrollY = signal(0);
  const viewportWidth = signal(0);
  const heights = signal<Record<number, Measurement>>({});
  let scroll: ScrollRef | undefined;

  effect(() => {
    viewportWidth();
    heights.set({});
  });

  const metrics = computed(() => {
    const width = viewportWidth() || 600;
    const measured = heights();
    const lines = props.text().split("\n");
    const tops: number[] = [];
    const bottoms: number[] = [];
    let total = 0;
    for (let index = 0; index < lines.length; index += 1) {
      const cached = measured[index];
      const height = cached && cached.text === lines[index] ? cached.height : estimateHeight(lines[index], width);
      tops.push(total);
      total += height;
      bottoms.push(total);
    }
    return { lines, tops, bottoms, total };
  });

  const layout = computed(() => {
    const { lines, tops, bottoms, total } = metrics();
    const viewport = Math.min(MAX_HEIGHT, Math.max(LINE_HEIGHT, total));
    const offset = Math.max(0, Math.min(scrollY(), Math.max(0, total - viewport)));
    let low = 0;
    let high = lines.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (bottoms[mid] >= offset) high = mid;
      else low = mid + 1;
    }
    const first = Math.max(0, low - OVERSCAN);
    const limit = offset + viewport + OVERSCAN * LINE_HEIGHT;
    low = first;
    high = lines.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (tops[mid] >= limit) high = mid;
      else low = mid + 1;
    }
    const last = low;
    const rows: Row[] = [];
    for (let index = first; index < last; index += 1) {
      rows.push({ index, text: lines[index], top: tops[index] });
    }
    return { rows, total, viewport };
  });

  effect(() => {
    const timer = setTimeout(() => scroll?.scrollTo({ y: 0, animated: false }), 0);
    return () => clearTimeout(timer);
  });

  return (
    <Scroll
      testID="reasoning-view"
      ref={(node) => { scroll = node; }}
      class="w-full"
      style={{ height: layout().viewport }}
      scrollbar
      bounces={false}
      accessibilityLabel={props.accessibilityLabel}
      onLayout={({ bounds }) => viewportWidth.set(bounds.size.width)}
      onScroll={(event) => {
        const { total, viewport } = layout();
        scrollY.set(Math.max(0, Math.min(event.offset.y, Math.max(0, total - viewport))));
      }}
    >
      <View selectable class="relative w-full" style={{ height: layout().total }}>
        <Each of={layout().rows} key={(row) => row.index}>
          {(row) => (
            <View
              style={{ position: "absolute", left: 0, right: 0, top: row().top }}
              onLayout={({ bounds }) => {
                const next = Math.ceil(bounds.size.height);
                const cached = heights()[row().index];
                if (cached && cached.text === row().text && Math.abs(cached.height - next) <= 0.5) return;
                heights.update((all) => ({ ...all, [row().index]: { text: row().text, height: next } }));
              }}
            >
              <Text class="w-full text-sm leading-6 text-muted-foreground" style={{ userSelect: "text" }}>
                {row().text || " "}
              </Text>
            </View>
          )}
        </Each>
      </View>
    </Scroll>
  );
}
