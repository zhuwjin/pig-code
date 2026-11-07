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
} from "nsh";
import { CodeHighlighter } from "pig-code-ts/native";
import type { EditorToken } from "nsh/base";

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
  error?: boolean;
  testID?: string;
};

const highlighter = new CodeHighlighter();
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
}) {
  const row = () => props.row();
  const content = () => segments(row(), props.spans());
  return (
    <View class={props.wrap ? "w-full flex-row items-start" : "flex-row items-start"} style={{ minHeight: LINE_HEIGHT }}>
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
  let highlightRevision = 0;
  effect(() => {
    const code = props.code();
    const language = props.language;
    const revision = ++highlightRevision;
    spans.set([]);
    void highlighter.highlight(code, language).then((json: string) => {
      if (revision !== highlightRevision) return;
      try {
        spans.set(JSON.parse(json) as TokenSpan[]);
      } catch {
        spans.set([]);
      }
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
    measuredHeight.set(null);
  });
  const maxHeight = () => props.maxHeight ?? 320;
  const height = () => Math.max(
    LINE_HEIGHT,
    Math.min(measuredHeight() ?? rows().length * LINE_HEIGHT, maxHeight()),
  );
  const gutterWidth = () => {
    if (!props.lineNumbers) return 0;
    const digits = Math.max(1, ...rows().map((row) => row.number ?? 0)).toString().length;
    return 18 + digits * 8;
  };
  const contentWidth = () => {
    const text = Math.max(1, ...rows().map((row) => textUnits(row.text)));
    return Math.max(viewportWidth(), Math.ceil(gutterWidth() + 24 + text * CHARACTER_WIDTH));
  };
  const list = () => (
    <List
      testID={props.testID}
      each={rows()}
      keyExtractor={(row) => row.id}
      estimatedItemSize={LINE_HEIGHT}
      class="h-full"
      style={props.wrap() ? { width: "100%" } : { width: contentWidth() }}
      scrollbar
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
  );
  return (
    <View
      class="w-full overflow-hidden"
      style={{ height: height() }}
      onLayout={({ bounds }) => viewportWidth.set(bounds.size.width)}
    >
      <If when={props.wrap()}>{list()}</If>
      <If when={!props.wrap()}>
        <Scroll horizontal class="h-full w-full" contentClass="h-full">
          {list()}
        </Scroll>
      </If>
    </View>
  );
}
