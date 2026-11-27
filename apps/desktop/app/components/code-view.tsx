import {
  Accessor,
  colorScheme,
  computed,
  effect,
  List,
  Scroll,
  signal,
  Text,
  View,
  type ScrollRef,
} from "nsh";
import { Scrollbar, type EditorToken } from "nsh/base";
import { getActiveDaemonClient } from "../daemon-client";

export type CodeViewLine = { number?: number; text: string };

type CodeRow = CodeViewLine & { id: string; start: number; end: number };
type TokenSpan = { start: number; end: number; token: EditorToken };
type Segment = { id: string; text: string; token?: EditorToken };

type CodeViewProps = {
  code: Accessor<string>;
  lines?: Accessor<CodeViewLine[]>;
  language: string;
  wrap: Accessor<boolean>;
  lineNumbers?: boolean;
  maxHeight?: number;
  fill?: boolean;
  error?: boolean;
  testID?: string;
};

const LINE_HEIGHT = 19;
const CHARACTER_WIDTH = 7.2;
const COLORS: Record<"light" | "dark", Record<EditorToken, string>> = {
  light: {
    comment: "#6e7781",
    keyword: "#8250df",
    string: "#116329",
    number: "#953800",
    title: "#0550ae",
    attr: "#7d4e00",
    built: "#0a7285",
    deletion: "#cf222e",
  },
  dark: {
    comment: "#7d7a72",
    keyword: "#c4a7ff",
    string: "#9fd68a",
    number: "#f5b27a",
    title: "#82b8ff",
    attr: "#f0c674",
    built: "#6fd3d8",
    deletion: "#f87171",
  },
};

function splitLines(code: string): CodeViewLine[] {
  return code.replace(/\r\n?/g, "\n").split("\n").map((text, index) => ({ number: index + 1, text }));
}

function segments(row: CodeRow, spans: TokenSpan[]): Segment[] {
  const clipped = spans.flatMap((span) => {
    const start = Math.max(span.start, row.start) - row.start;
    const end = Math.min(span.end, row.end) - row.start;
    return start < end && end > 0 && start < row.text.length
      ? [{ start, end, token: span.token }]
      : [];
  }).sort((a, b) => a.start - b.start);
  const output: Segment[] = [];
  let offset = 0;
  const push = (text: string, token?: EditorToken) => {
    if (!text) return;
    output.push({ id: `${offset}-${offset + text.length}-${token ?? "plain"}`, text, token });
    offset += text.length;
  };
  for (const span of clipped) {
    push(row.text.slice(offset, span.start));
    push(row.text.slice(span.start, span.end), span.token);
  }
  push(row.text.slice(offset));
  return output.length ? output : [{ id: "plain", text: row.text }];
}

function CodeLine(props: {
  row: Accessor<CodeRow>;
  spans: Accessor<TokenSpan[]>;
  wrap: boolean;
  lineNumbers: boolean;
  gutterWidth: number;
  error: boolean;
  top?: number;
}) {
  const row = () => props.row();
  const content = () => segments(row(), props.spans());
  return (
    <View
      class={props.wrap ? "w-full flex-row items-start" : "flex-row items-start"}
      style={{
        ...(props.top !== undefined
          ? { position: "absolute", left: 0, right: 0, top: props.top, height: LINE_HEIGHT }
          : { minHeight: LINE_HEIGHT }),
        cursor: props.wrap ? "default" : "text",
      }}
    >
      <If when={props.lineNumbers && row().number !== undefined}>
        <Text
          class="shrink-0 pr-2 font-mono text-xs text-muted-foreground"
          style={{ width: props.gutterWidth, lineHeight: LINE_HEIGHT, textAlign: "right", userSelect: "text" }}
        >
          {row().number}
        </Text>
      </If>
      <View class={props.wrap ? "min-w-0 flex-1 px-3" : "shrink-0 px-3"}>
        <Text
          class={props.error ? "font-mono text-xs text-destructive" : "font-mono text-xs text-foreground"}
          style={{ lineHeight: LINE_HEIGHT, userSelect: "text", ...(props.wrap ? {} : { numberOfLines: 1 }) }}
        >
          <Each of={content()} key={(segment) => segment.id}>
            {(segment) => (
              <Text
                style={{
                  color: segment().token ? COLORS[colorScheme()][segment().token!] : undefined,
                  fontStyle: segment().token === "comment" ? "italic" : undefined,
                }}
              >
                {segment().text}
              </Text>
            )}
          </Each>
        </Text>
      </View>
    </View>
  );
}

function textUnits(text: string): number {
  let width = 0;
  for (const char of text) width += char === "\t" ? 4 : char.codePointAt(0)! > 0xff ? 2 : 1;
  return width;
}

export function CodeView(props: CodeViewProps) {
  const spans = signal<TokenSpan[]>([]);
  const measuredHeight = signal<number | null>(null);
  const viewportWidth = signal(0);
  const viewportHeight = signal(0);
  const textViewportWidth = signal(0);
  const scrollY = signal(0);
  let horizontal: ScrollRef | undefined;
  let highlightRevision = 0;
  let lastSlots: { slot: number; row: number }[] = [];
  effect(() => {
    const code = props.code();
    const language = props.language;
    const revision = ++highlightRevision;
    spans.set([]);
    const daemon = getActiveDaemonClient();
    if (!daemon) {
      return () => { highlightRevision += 1; };
    }
    void daemon.highlight(code, language).then((result) => {
      if (revision !== highlightRevision) return;
      spans.set(result as TokenSpan[]);
    }, () => {
      if (revision === highlightRevision) spans.set([]);
    });
    return () => { highlightRevision += 1; };
  });
  const rows = computed((): CodeRow[] => {
    let offset = 0;
    return (props.lines?.() ?? splitLines(props.code())).map((line, index) => {
      const start = offset;
      offset += line.text.length + 1;
      return { ...line, id: `line-${index}`, start, end: start + line.text.length };
    });
  });
  effect(() => {
    rows();
    props.wrap();
    viewportWidth();
    measuredHeight.set(null);
  });
  const maxHeight = () => props.maxHeight ?? 320;
  const contentHeight = () => Math.max(LINE_HEIGHT, rows().length * LINE_HEIGHT);
  const gutterWidth = () => {
    if (!props.lineNumbers) return 0;
    const digits = Math.max(1, ...rows().map((row) => row.number ?? 0)).toString().length;
    return 18 + digits * 8;
  };
  const wrappedContentHeight = () => {
    const width = viewportWidth() - gutterWidth() - 24;
    if (width <= 0) return contentHeight();
    let height = 0;
    for (const row of rows()) {
      height += Math.max(1, Math.ceil(textUnits(row.text) * CHARACTER_WIDTH / width)) * LINE_HEIGHT;
      if (height >= maxHeight()) return maxHeight();
    }
    return Math.max(LINE_HEIGHT, height);
  };
  const height = () => props.fill
    ? Math.max(LINE_HEIGHT, viewportHeight())
    : Math.max(
      LINE_HEIGHT,
      Math.min(props.wrap() ? measuredHeight() ?? wrappedContentHeight() : contentHeight(), maxHeight()),
    );
  const textWidth = () => Math.max(1, textViewportWidth() || viewportWidth() - gutterWidth());
  const contentWidth = () => {
    const text = Math.max(1, ...rows().map((row) => textUnits(row.text)));
    return Math.max(textWidth(), Math.ceil(24 + text * CHARACTER_WIDTH));
  };
  const list = () => (
    <View selectable class="h-full w-full">
      <List
        testID={props.testID}
        each={rows()}
        keyExtractor={(row) => row.id}
        estimatedItemSize={LINE_HEIGHT}
        class="h-full w-full"
        scrollbar
        bounces={false}
        onScroll={(event) => {
          const next = Math.ceil(event.contentSize.height);
          if (measuredHeight() !== next) measuredHeight.set(next);
        }}
      >
        {(row) => (
          <CodeLine
            row={row}
            spans={spans}
            wrap={props.wrap()}
            lineNumbers={props.lineNumbers ?? false}
            gutterWidth={gutterWidth()}
            error={props.error ?? false}
          />
        )}
      </List>
    </View>
  );
  const visible = computed(() => {
    const first = Math.max(0, Math.floor(scrollY() / LINE_HEIGHT) - 6);
    const last = Math.min(rows().length, Math.ceil((scrollY() + height()) / LINE_HEIGHT) + 6);
    return { first, last };
  });
  // Reuse each visible row's element slot while it stays in view, as the official Editor does.
  const slots = computed(() => {
    const { first, last } = visible();
    const inView = Array.from({ length: Math.max(0, last - first) }, (_, index) => first + index);
    const wanted = new Set(inView);
    const kept = new Map<number, number>();
    const free: number[] = [];
    for (const entry of lastSlots) {
      if (wanted.has(entry.row)) kept.set(entry.row, entry.slot);
      else free.push(entry.slot);
    }
    free.sort((a, b) => a - b);
    let next = lastSlots.reduce((max, entry) => Math.max(max, entry.slot + 1), 0);
    const result = inView.map((row) => {
      const slot = kept.get(row) ?? free.shift() ?? next++;
      return { slot, row };
    });
    result.sort((a, b) => a.row - b.row);
    lastSlots = result;
    return result;
  });
  const scrolling = () => (
    <View class="h-full w-full">
      <Scroll
        testID={props.testID}
        class="h-full w-full cursor-default"
        scrollbar
        bounces={false}
        onScroll={(event) => {
          const max = Math.max(0, contentHeight() - height());
          scrollY.set(Math.max(0, Math.min(event.offset.y, max)));
        }}
      >
        <View class="relative flex-row" style={{ height: contentHeight() }}>
          <If when={props.lineNumbers}>
            <View class="relative shrink-0" style={{ width: gutterWidth(), height: contentHeight() }}>
              <Each of={slots()} key={(entry) => entry.slot}>
                {(entry) => (
                  <Text
                    class="absolute left-0 pr-2 font-mono text-xs text-muted-foreground"
                    style={{ top: entry().row * LINE_HEIGHT, width: gutterWidth(), lineHeight: LINE_HEIGHT, textAlign: "right", userSelect: "text", cursor: "default" }}
                  >
                    {rows()[entry().row]?.number}
                  </Text>
                )}
              </Each>
            </View>
          </If>
          <Scroll
            ref={horizontal}
            horizontal
            testID={props.testID ? `${props.testID}-horizontal` : undefined}
            class="min-w-0 flex-1 cursor-default"
            scrollbar={false}
            bounces={false}
            onLayout={({ bounds }) => textViewportWidth.set(bounds.size.width)}
          >
            <View selectable class="relative" style={{ width: contentWidth(), height: contentHeight() }}>
              <Each of={slots()} key={(entry) => entry.slot}>
                {(entry) => (
                  <CodeLine
                    row={() => rows()[entry().row]}
                    top={entry().row * LINE_HEIGHT}
                    spans={spans}
                    wrap={false}
                    lineNumbers={false}
                    gutterWidth={0}
                    error={props.error ?? false}
                  />
                )}
              </Each>
            </View>
          </Scroll>
          <If when={contentHeight() > height()}>
            <View
              class="absolute right-0 cursor-default"
              style={{ top: scrollY(), width: 12, height: height() }}
            />
          </If>
        </View>
      </Scroll>
      <If when={contentWidth() > textWidth()}>
        <Scrollbar
          testID={props.testID ? `${props.testID}-horizontal-scrollbar` : undefined}
          scroll={horizontal}
          axis="horizontal"
          mode="hover"
          motion={{ idle: 700, enter: 0, exit: 300, entrance: "fade", thumbHoverEntrance: "fade" }}
          minThumbLength={32}
          class="absolute bottom-0 h-3 items-end pb-[2px]"
          style={{ left: gutterWidth(), right: 0 }}
          thumbClass="h-1.5 rounded-full"
          thumbStyle={{ backgroundColor: colorScheme() === "dark" ? "rgba(255,255,255,0.55)" : "rgba(0,0,0,0.45)" }}
        />
      </If>
    </View>
  );
  return (
    <View
      class={`relative w-full overflow-hidden ${props.fill ? "min-h-0 flex-1" : ""}`}
      style={props.fill ? undefined : { height: height() }}
      onLayout={({ bounds }) => {
        viewportWidth.set(bounds.size.width);
        viewportHeight.set(bounds.size.height);
      }}
    >
      <If when={props.wrap()}>{list()}</If>
      <If when={!props.wrap()}>{scrolling()}</If>
    </View>
  );
}
