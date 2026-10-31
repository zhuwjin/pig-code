import { Scroll, Text, View, effect, signal } from "nsh";
import type { YNode } from "nsh";
import { Button, Input } from "nsh/base";
import { Terminal as NativeTerminal } from "pig-code-ts/native";
import type { UiContext } from "../ui-context";
import { Icon } from "./icon";

export function TerminalPanel(props: UiContext & { onClose: () => void }) {
  const output = signal("");
  const exited = signal(false);
  const error = signal("");
  const draft = signal("");
  let scroll: YNode | null = null;
  let terminal: NativeTerminal | null = null;
  try {
    terminal = new NativeTerminal(
      props.store.cwd(),
      (props.store.config()?.terminal_shell as string | null | undefined) ?? null,
    );
  } catch (cause) {
    error.set(cause instanceof Error ? cause.message : String(cause));
  }
  const refresh = () => {
    if (!terminal) return;
    try {
      const snapshot = JSON.parse(terminal.snapshot()) as { text: string; exited: boolean };
      output.set(snapshot.text);
      exited.set(snapshot.exited);
    } catch (cause) {
      error.set(cause instanceof Error ? cause.message : String(cause));
    }
  };
  effect(() => {
    if (!terminal) return;
    const timer = setInterval(refresh, 80);
    refresh();
    return () => {
      clearInterval(timer);
      terminal?.dispose();
    };
  });
  effect(() => {
    output();
    scroll?.scrollToEnd({ animated: false });
  });
  const send = () => {
    if (!terminal || exited()) return;
    try {
      terminal.write(`${draft()}\r`);
      draft.set("");
    } catch (cause) {
      error.set(cause instanceof Error ? cause.message : String(cause));
    }
  };
  return (
    <View testID="terminal-panel" class="h-[300px] shrink-0 border-t border-border bg-background">
      <View class="h-9 shrink-0 flex-row items-center gap-2 border-b border-border px-3">
        <Icon name="square-terminal" size={14} />
        <Text class="flex-1 text-sm text-foreground">终端</Text>
        <If when={exited()}><Text class="text-xs text-muted-foreground">进程已退出</Text></If>
        <Button
          testID="terminal-interrupt"
          accessibilityLabel="发送 Ctrl+C"
          class="rounded-(--radius) px-2 py-1 hover:bg-accent"
          onPress={() => {
            try {
              terminal?.write("\x03");
            } catch (cause) {
              error.set(cause instanceof Error ? cause.message : String(cause));
            }
          }}
        >
          <Text class="text-xs text-muted-foreground">Ctrl+C</Text>
        </Button>
        <Button
          testID="terminal-close"
          accessibilityLabel="关闭终端"
          class="h-7 w-7 items-center justify-center rounded-(--radius) hover:bg-accent"
          onPress={props.onClose}
        >
          <Icon name="x" size={14} />
        </Button>
      </View>
      <View
        class="min-h-0 flex-1"
        onLayout={(bounds) => {
          try {
            terminal?.resize(
              Math.max(1, Math.floor((bounds.size.width - 16) / 7.5)),
              Math.max(1, Math.floor((bounds.size.height - 64) / 18)),
            );
          } catch (cause) {
            error.set(cause instanceof Error ? cause.message : String(cause));
          }
        }}
      >
        <Scroll
          ref={(node) => { scroll = node; }}
          class="flex-1"
          contentClass="p-2"
        >
          <Text testID="terminal-output" selectable class="font-mono text-xs leading-[18px] text-foreground">
            {output()}
          </Text>
        </Scroll>
      </View>
      <If when={error()}>
        <Text class="px-3 pb-1 text-xs text-destructive">{error()}</Text>
      </If>
      <View class="shrink-0 flex-row items-center gap-2 border-t border-border px-3 py-2">
        <Input
          testID="terminal-input"
          value={draft()}
          onChange={(value) => draft.set(value)}
          onSubmit={send}
          submitBehavior="submit"
          placeholder={exited() ? "进程已退出" : "输入命令，Enter 执行"}
          disabled={!terminal || exited()}
          class="min-w-0 flex-1 font-mono text-xs text-foreground placeholder:text-muted-foreground"
        />
        <Button
          testID="terminal-run"
          disabled={!terminal || exited()}
          class="rounded-(--radius) bg-primary px-3 py-1 disabled:opacity-30"
          onPress={send}
        >
          <Text class="text-xs text-primary-foreground">执行</Text>
        </Button>
      </View>
    </View>
  );
}
