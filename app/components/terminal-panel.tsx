import { Canvas, Clipboard, PanResponder, Scroll, Text, View, colorScheme, effect, keymap, signal, token } from "nsh";
import type { InputRef, ScrollRef, Signal } from "nsh";
import { Button, Input } from "nsh/base";
import { Terminal as NativeTerminal } from "pig-code-ts/native";
import { t } from "../i18n";
import type { UiContext } from "../ui-context";
import { Icon } from "./icon";

const ANSI_DARK = ["#2c2a26", "#ec6a78", "#8fbf6e", "#e0b072", "#6fa8e6", "#c08adf", "#5fc2c9", "#d2cfc8", "#6b665d", "#f5868f", "#a8d98a", "#efc78a", "#8fc0f5", "#d2a6ec", "#84d6dc", "#f6f3ec"];
const ANSI_LIGHT = ["#24292e", "#d1242f", "#1a7f37", "#9a6700", "#0969da", "#8250df", "#1b7c83", "#6e7781", "#57606a", "#cf222e", "#1f883d", "#bf8700", "#218bff", "#a475f9", "#3192aa", "#8c959f"];
const ANSI_STEPS = [0, 95, 135, 175, 215, 255];
const LINE_HEIGHT = 18;
const FONT_SIZE = 14;
const hex = (value: number) => value.toString(16).padStart(2, "0");

type AnsiValue = string | number | null;
type TerminalSpan = { text: string; col: number; cells: number; fg: AnsiValue; bg: AnsiValue; dim: boolean; bold: boolean; inverse: boolean };
type TerminalLine = { spans: TerminalSpan[]; columns: string };
type SelectionPoint = { row: number; col: number };
type SelectionRange = { start: SelectionPoint; end: SelectionPoint };
type TerminalCursor = { row: number; col: number; text: string; shape: "block" | "bar" | "underline" };

function ansiColor(value: AnsiValue, fallback: string, dark: boolean): string {
  if (value === null) return fallback;
  if (typeof value === "string") return value;
  if (value < 16) return (dark ? ANSI_DARK : ANSI_LIGHT)[value];
  if (value < 232) {
    const index = value - 16;
    const r = ANSI_STEPS[Math.floor(index / 36)];
    const g = ANSI_STEPS[Math.floor(index / 6) % 6];
    const b = ANSI_STEPS[index % 6];
    return `#${hex(r)}${hex(g)}${hex(b)}`;
  }
  if (value < 256) {
    const gray = 8 + (value - 232) * 10;
    return `#${hex(gray)}${hex(gray)}${hex(gray)}`;
  }
  return fallback;
}

type TerminalTab = {
  id: number;
  terminal: NativeTerminal;
  output: Signal<string>;
  lines: Signal<TerminalLine[]>;
  serializedLines: string;
  cursor: Signal<TerminalCursor | null>;
  selection: Signal<SelectionRange | null>;
  serializedSelection: string;
  shell: Signal<string>;
  exited: Signal<boolean>;
};

export function TerminalPanel(props: UiContext & { onClose: () => void }) {
  const tabs: TerminalTab[] = [];
  const tabIds = signal<number[]>([]);
  const activeId = signal(0);
  const focused = signal(false);
  const following = signal(true);
  const cursorVisible = signal(true);
  const cellWidth = signal(7.5);
  const columnCount = signal(80);
  const rowCount = signal(24);
  const viewport = signal<{ width: number; height: number } | null>(null);
  const error = signal("");
  const draft = signal("");
  let nextId = 1;
  let input: InputRef | null = null;
  let scroll: ScrollRef | null = null;
  const activeTab = () => tabs.find((tab) => tab.id === activeId());
  const focusInput = () => input?.focus();
  const selection = () => activeTab()?.selection() ?? null;
  const updateSelection = (tab: TerminalTab, value: string) => {
    if (tab.serializedSelection === value) return;
    tab.serializedSelection = value;
    tab.selection.set(JSON.parse(value) as SelectionRange | null);
  };
  const pointAt = (x: number, y: number) => {
    const horizontal = x / cellWidth();
    const col = Math.max(0, Math.min(columnCount() - 1, Math.floor(horizontal)));
    return {
      row: Math.max(0, Math.min(rowCount() - 1, Math.floor(y / LINE_HEIGHT))),
      col,
      left: horizontal - Math.floor(horizontal) <= 0.5,
    };
  };
  const selectAll = () => {
    const tab = activeTab();
    if (tab) updateSelection(tab, tab.terminal.selectAll());
  };
  const copySelection = () => {
    const text = activeTab()?.terminal.copySelection();
    if (!text) return;
    Clipboard.writeText(text).catch((cause) => {
      error.set(cause instanceof Error ? cause.message : String(cause));
    });
  };
  const finishSelection = () => {
    activeTab()?.terminal.selectEnd();
    focusInput();
  };
  const pan = PanResponder.create({
    onStartShouldSetPanResponderCapture: () => true,
    onPanResponderTerminationRequest: () => false,
    onShouldBlockNativeResponder: () => true,
    onPanResponderGrant: (event) => {
      const tab = activeTab();
      if (!tab) return;
      focusInput();
      const point = pointAt(event.locationX, event.locationY);
      updateSelection(tab, tab.terminal.selectStart(point.col, point.row, point.left, 1, false));
    },
    onPanResponderMove: (event) => {
      const tab = activeTab();
      if (!tab) return;
      const point = pointAt(event.locationX, event.locationY);
      updateSelection(tab, tab.terminal.selectUpdate(point.col, point.row, point.left));
    },
    onPanResponderRelease: finishSelection,
    onPanResponderTerminate: finishSelection,
  });
  const write = (text: string) => {
    const tab = activeTab();
    if (!tab || tab.exited()) return;
    try {
      tab.terminal.write(text);
      updateSelection(tab, "null");
      following.set(true);
      cursorVisible.set(true);
    } catch (cause) {
      error.set(cause instanceof Error ? cause.message : String(cause));
    }
  };
  const changeInput = (text: string) => {
    const before = [...draft()];
    const after = [...text];
    let index = 0;
    while (index < before.length && index < after.length && before[index] === after[index]) index += 1;
    draft.set(text);
    const deleted = "\x7f".repeat(before.length - index);
    const inserted = after.slice(index).join("");
    if (deleted || inserted) write(deleted + inserted);
  };
  const openTab = () => {
    try {
      const terminal = new NativeTerminal(
        props.store.cwd(),
        (props.store.config()?.terminal_shell as string | null | undefined) ?? null,
      );
      const id = nextId++;
      tabs.push({
        id,
        terminal,
        output: signal(""),
        lines: signal([]),
        serializedLines: "[]",
        cursor: signal(null),
        selection: signal(null),
        serializedSelection: "null",
        shell: signal("shell"),
        exited: signal(false),
      });
      activeId.set(id);
      tabIds.set(tabs.map((tab) => tab.id));
      error.set("");
      focusInput();
    } catch (cause) {
      error.set(cause instanceof Error ? cause.message : String(cause));
    }
  };
  const closeTab = (id: number) => {
    const index = tabs.findIndex((tab) => tab.id === id);
    if (index < 0) return;
    tabs[index].terminal.dispose();
    tabs.splice(index, 1);
    if (activeId() === id) activeId.set(tabs[Math.min(index, tabs.length - 1)]?.id ?? 0);
    tabIds.set(tabs.map((tab) => tab.id));
    focusInput();
  };
  const refresh = () => {
    for (const tab of tabs) {
      try {
        const snapshot = JSON.parse(tab.terminal.snapshot()) as {
          text: string;
          lines: TerminalLine[];
          cursor: TerminalCursor | null;
          selection: SelectionRange | null;
          exited: boolean;
          shell: string;
        };
        tab.output.set(snapshot.text);
        const serialized = JSON.stringify(snapshot.lines);
        if (tab.serializedLines !== serialized) {
          tab.serializedLines = serialized;
          tab.lines.set(snapshot.lines);
        }
        tab.cursor.set(snapshot.cursor);
        updateSelection(tab, JSON.stringify(snapshot.selection));
        tab.exited.set(snapshot.exited);
        tab.shell.set(snapshot.shell);
      } catch (cause) {
        error.set(cause instanceof Error ? cause.message : String(cause));
      }
    }
  };
  effect(() => {
    openTab();
    const timer = setInterval(refresh, 80);
    const blink = setInterval(() => {
      if (focused()) cursorVisible.update((visible) => !visible);
    }, 530);
    return () => {
      clearInterval(timer);
      clearInterval(blink);
      for (const tab of tabs) tab.terminal.dispose();
    };
  });
  effect(() => {
    const bounds = viewport();
    const width = cellWidth();
    if (!bounds) return;
    const columns = Math.max(1, Math.floor((bounds.width - 12) / width));
    const rows = Math.max(1, Math.floor((bounds.height - 8) / LINE_HEIGHT));
    columnCount.set(columns);
    rowCount.set(rows);
    for (const tab of tabs) {
      try {
        tab.terminal.resize(columns, rows);
      } catch (cause) {
        error.set(cause instanceof Error ? cause.message : String(cause));
      }
    }
  });
  effect(() => {
    activeTab()?.output();
    if (following()) scroll?.scrollToEnd({ animated: false });
  });
  effect(() => {
    activeId();
    draft.set("");
    focusInput();
  });
  effect(() => {
    if (!focused()) return;
    return keymap.bind({
      "mod+a": selectAll,
      "mod+c": () => {
        if (selection()) copySelection();
        else {
          write("\x03");
          draft.set("");
        }
      },
      "ctrl+c": () => {
        write("\x03");
        draft.set("");
      },
      "tab": () => write("\t"),
      "up": () => write("\x1b[A"),
      "down": () => write("\x1b[B"),
      "right": () => write("\x1b[C"),
      "left": () => write("\x1b[D"),
    }, { when: "always", repeat: true });
  });
  return (
    <View testID="terminal-panel" class="h-full w-full bg-background">
      <View class="h-[30px] shrink-0 flex-row items-center gap-1 border-b border-border px-2">
        <Each of={tabIds()} key={(id) => id}>
          {(id) => {
            const tab = () => tabs.find((item) => item.id === id());
            return (
              <View class={`h-6 shrink-0 flex-row items-center rounded-(--radius) pl-3 pr-1 ${activeId() === id() ? "bg-muted" : "hover:bg-accent/60"}`}>
                <Button
                  testID={`terminal-tab-${id()}`}
                  class="flex-row items-center gap-2"
                  onPress={() => {
                    activeId.set(id());
                    focusInput();
                  }}
                >
                  <Icon name="square-terminal" size={14} />
                  <Text class="text-sm text-foreground">
                    {`${tab()?.shell() ?? "shell"}${tab()?.exited() ? t("terminal.exited_suffix") : ""}`}
                  </Text>
                </Button>
                <Button
                  testID={`terminal-tab-close-${id()}`}
                  accessibilityLabel={t("terminal.close_tab")}
                  class="h-5 w-5 items-center justify-center rounded-(--radius) hover:bg-accent"
                  onPress={() => closeTab(id())}
                >
                  <Icon name="x" size={12} />
                </Button>
              </View>
            );
          }}
        </Each>
        <View class="flex-1" />
        <Button
          testID="terminal-new-tab"
          accessibilityLabel={t("terminal.new_tab")}
          class="h-6 w-6 items-center justify-center rounded-(--radius) hover:bg-accent"
          onPress={openTab}
        >
          <Icon name="plus" size={14} />
        </Button>
        <Button
          testID="terminal-close"
          accessibilityLabel={t("terminal.collapse")}
          class="h-6 w-6 items-center justify-center rounded-(--radius) hover:bg-accent"
          onPress={props.onClose}
        >
          <Icon name="chevron-down" size={14} />
        </Button>
      </View>
      <If when={error()}>
        <Text class="px-3 py-1 text-xs text-destructive">{error()}</Text>
      </If>
      <If when={tabIds().length === 0}>
        <View class="flex-1 items-center justify-center gap-2">
          <Text class="text-sm text-muted-foreground">{t("terminal.empty")}</Text>
          <Button
            testID="terminal-empty-new"
            class="rounded-(--radius) border border-border px-3 py-1 hover:bg-accent"
            onPress={openTab}
          >
            <Text class="text-xs text-foreground">{t("terminal.new")}</Text>
          </Button>
        </View>
      </If>
      <If when={tabIds().length > 0}>
        <View
          class="relative min-h-0 flex-1"
          onLayout={({ bounds }) => {
            viewport.set({ width: bounds.size.width, height: bounds.size.height });
          }}
        >
          <Scroll
            ref={(node) => { scroll = node; }}
            class="flex-1"
            contentClass="px-1.5 py-1"
            onScroll={(event) => {
              following.set(event.offset.y + event.size.height >= event.contentSize.height - 24);
            }}
          >
            <Canvas
              testID="terminal-output"
              class="cursor-text"
              style={{
                width: Math.max((viewport()?.width ?? 12) - 12, columnCount() * cellWidth()),
                height: Math.max(rowCount(), activeTab()?.lines().length ?? 0) * LINE_HEIGHT,
              }}
              {...pan.panHandlers}
              onPress={(event) => {
                const tab = activeTab();
                if (!tab || !event || (event.clickCount === 1 && !event.modifiers.shift)) return;
                const point = pointAt(event.position.x, event.position.y);
                updateSelection(tab, tab.terminal.selectStart(point.col, point.row, point.left, event.clickCount, event.modifiers.shift));
                tab.terminal.selectEnd();
                focusInput();
              }}
              draw={(cx) => {
                const tab = activeTab();
                if (!tab) return;
                const dark = colorScheme() === "dark";
                const foreground = token("foreground");
                const background = token("background");
                const mono = props.themeStore.fonts().mono ?? "monospace";
                cx.font = `400 ${FONT_SIZE}px ${mono}`;
                const measured = cx.measureText("M").width;
                if (measured > 0 && measured !== cellWidth()) cellWidth.set(measured);
                const cell = measured > 0 ? measured : cellWidth();
                const lines = tab.lines();
                const selected = selection();
                cx.textBaseline = "top";
                for (const [row, line] of lines.entries()) {
                  for (const span of line.spans) {
                    const value = span.inverse ? span.fg : span.bg;
                    if (value === null) continue;
                    cx.fillStyle = ansiColor(value, span.inverse ? foreground : background, dark);
                    cx.fillRect(span.col * cell, row * LINE_HEIGHT, span.cells * cell, LINE_HEIGHT);
                  }
                }
                if (selected) {
                  cx.fillStyle = token("selection");
                  cx.globalAlpha = 0.4;
                  for (let row = selected.start.row; row <= selected.end.row; row += 1) {
                    const from = row === selected.start.row ? selected.start.col : 0;
                    const to = row === selected.end.row ? selected.end.col : columnCount();
                    cx.fillRect(from * cell, row * LINE_HEIGHT, Math.max(0, to - from) * cell, LINE_HEIGHT);
                  }
                  cx.globalAlpha = 1;
                }
                for (const [row, line] of lines.entries()) {
                  for (const span of line.spans) {
                    if (!span.text.trim()) continue;
                    cx.font = `${span.bold ? 600 : 400} ${FONT_SIZE}px ${mono}`;
                    cx.fillStyle = ansiColor(span.inverse ? span.bg : span.fg, span.inverse ? background : foreground, dark);
                    cx.globalAlpha = span.dim ? 0.66 : 1;
                    cx.fillText(span.text, span.col * cell, row * LINE_HEIGHT, span.cells * cell);
                  }
                }
                cx.globalAlpha = 1;
                const cursor = tab.cursor();
                if (!cursor) return;
                const x = cursor.col * cell;
                const y = cursor.row * LINE_HEIGHT;
                const visible = focused() && cursorVisible();
                const block = visible && cursor.shape === "block";
                if (block) {
                  cx.fillStyle = foreground;
                  cx.globalAlpha = 0.7;
                  cx.fillRect(x, y, cell, LINE_HEIGHT);
                  cx.globalAlpha = 1;
                } else if (visible) {
                  cx.fillStyle = foreground;
                  if (cursor.shape === "bar") cx.fillRect(x, y, Math.max(1, cell * 0.15), LINE_HEIGHT);
                  else cx.fillRect(x, y + LINE_HEIGHT - 2, cell, 2);
                } else if (!focused()) {
                  cx.strokeStyle = foreground;
                  cx.lineWidth = 1;
                  cx.strokeRect(x + 0.5, y + 0.5, cell - 1, LINE_HEIGHT - 1);
                }
                if (block && cursor.text.trim()) {
                  cx.font = `400 ${FONT_SIZE}px ${mono}`;
                  cx.fillStyle = background;
                  cx.save();
                  cx.beginPath();
                  cx.rect(x, y, cell, LINE_HEIGHT);
                  cx.clip();
                  cx.fillText(cursor.text, x, y);
                  cx.restore();
                }
              }}
            />
            <Text class="absolute bottom-0 left-0 h-px w-px overflow-hidden opacity-0 pointer-events-none">
              {activeTab()?.output() ?? ""}
            </Text>
            <If when={selection()}>
              <Text testID="terminal-selection" class="absolute bottom-0 left-0 h-px w-px overflow-hidden opacity-0 pointer-events-none">
                {t("terminal.text_selected")}
              </Text>
            </If>
          </Scroll>
          <Input
            ref={(node) => { input = node; }}
            testID="terminal-input"
            accessibilityLabel={t("terminal.input_label")}
            value={draft()}
            onChange={({ value }) => changeInput(value)}
            onKeyPress={(event) => {
              if (event.key === "Enter") {
                write("\r");
                draft.set("");
              } else if (event.key === "Escape") {
                write("\x1b");
                draft.set("");
              }
            }}
            onFocus={() => {
              focused.set(true);
              cursorVisible.set(true);
            }}
            onBlur={() => focused.set(false)}
            disabled={!activeTab() || activeTab()!.exited()}
            submitBehavior="submit"
            autoFocus
            caret={false}
            class="absolute bottom-0 left-0 h-px w-px opacity-0"
          />
        </View>
      </If>
    </View>
  );
}
