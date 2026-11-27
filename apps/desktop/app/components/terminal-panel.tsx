import { Animated, Easing, effect, reduceMotion, signal, Text, untracked, View, createAnimatedValue } from "nsh";
import type { Signal } from "nsh";
import { Button, Terminal, type TerminalRef } from "nsh/base";
import { t } from "../i18n";
import type { UiContext } from "../ui-context";
import { Icon } from "./icon";

const FONT_SIZE = 14;
const LINE_HEIGHT = 18;
const SCROLLBACK = 10_000;

type TerminalTab = {
  id: number;
  remoteId: string;
  ref: TerminalRef | null;
  pendingOutput: string;
  shell: string;
  exited: Signal<boolean>;
};

export function TerminalPanel(props: UiContext & { onClose: () => void }) {
  const tabs: TerminalTab[] = [];
  const tabIds = signal<number[]>([]);
  const activeId = signal(0);
  const error = signal("");
  const cursorBlink = createAnimatedValue(1);
  let nextId = 1;
  let disposed = false;

  const activeTab = () => tabs.find((tab) => tab.id === activeId());
  const report = (cause: unknown) => {
    const message = cause instanceof Error ? cause.message : String(cause);
    if (error() !== message) error.set(message);
  };
  const scheduleFocus = (tab?: TerminalTab) => {
    setTimeout(() => (tab ?? activeTab())?.ref?.focus(), 0);
  };
  const write = (tab: TerminalTab, text: string) => {
    if (!text || tab.exited()) return;
    void props.store.agent.writeTerminal(tab.remoteId, text).catch(report);
  };
  const openTab = () => {
    const cwd = props.store.cwd();
    const shell = (props.store.config()?.terminal_shell as string | null | undefined) ?? null;
    void props.store.agent.openTerminal(cwd, shell).then((info) => {
      if (disposed) {
        void props.store.agent.closeTerminal(info.terminal_id).catch(() => {});
        return;
      }
      const id = nextId++;
      const tab: TerminalTab = {
        id,
        remoteId: info.terminal_id,
        ref: null,
        pendingOutput: "",
        shell: info.shell,
        exited: signal(Boolean(info.exited)),
      };
      tabs.push(tab);
      activeId.set(id);
      tabIds.set(tabs.map((item) => item.id));
      error.set("");
      scheduleFocus(tab);
    }, report);
  };
  const closeTab = (id: number) => {
    const index = tabs.findIndex((tab) => tab.id === id);
    if (index < 0) return;
    const removed = tabs[index];
    tabs.splice(index, 1);
    void props.store.agent.closeTerminal(removed.remoteId).catch(() => {});
    if (activeId() === id) activeId.set(tabs[Math.min(index, tabs.length - 1)]?.id ?? 0);
    tabIds.set(tabs.map((tab) => tab.id));
    scheduleFocus();
  };

  effect(() => {
    disposed = false;
    const unsubOutput = props.store.agent.onTerminalOutput((remoteId, data) => {
      const tab = tabs.find((item) => item.remoteId === remoteId);
      if (!tab) return;
      if (tab.ref) {
        tab.ref.write(data);
      } else {
        tab.pendingOutput += data;
      }
    });
    const unsubExit = props.store.agent.onTerminalExit((remoteId) => {
      const tab = tabs.find((item) => item.remoteId === remoteId);
      if (tab && !tab.exited()) tab.exited.set(true);
    });
    untracked(openTab);
    return () => {
      disposed = true;
      unsubOutput();
      unsubExit();
      for (const tab of tabs) {
        void props.store.agent.closeTerminal(tab.remoteId).catch(() => {});
      }
    };
  });
  effect(() => {
    activeId();
    scheduleFocus();
  });
  effect(() => {
    if (reduceMotion()) {
      cursorBlink.set(1);
      return;
    }
    const loop = Animated.loop(Animated.sequence([
      Animated.delay(530),
      Animated.timing(cursorBlink, { toValue: 0, duration: 1, easing: Easing.linear }),
      Animated.delay(530),
      Animated.timing(cursorBlink, { toValue: 1, duration: 1, easing: Easing.linear }),
    ]));
    loop.start();
    return () => loop.stop();
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
                    scheduleFocus(tab());
                  }}
                >
                  <Icon name="square-terminal" size={14} />
                  <Text class="text-sm text-foreground">
                    {`${tab()?.shell ?? "shell"}${tab()?.exited() ? t("terminal.exited_suffix") : ""}`}
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
        <View class="relative min-h-0 flex-1">
          <Each of={tabIds()} key={(id) => id}>
            {(id) => {
              const tab = () => tabs.find((item) => item.id === id());
              const active = () => activeId() === id();
              return (
                <View
                  testID={active() ? "terminal-output" : undefined}
                  class="absolute inset-0"
                  style={{
                    visibility: active() ? "visible" : "hidden",
                    pointerEvents: active() ? "auto" : "none",
                  }}
                >
                  <Terminal
                    testID={`terminal-emulator-${id()}`}
                    ref={(node) => {
                      const current = tab();
                      if (current) {
                        current.ref = node;
                        if (node && current.pendingOutput) {
                          const buffered = current.pendingOutput;
                          current.pendingOutput = "";
                          node.write(buffered);
                        }
                      }
                    }}
                    class="h-full w-full bg-background px-1.5 py-1"
                    scrollback={SCROLLBACK}
                    fontSize={FONT_SIZE}
                    lineHeight={LINE_HEIGHT}
                    cursorClass="bg-foreground/50"
                    cursorStyle={{ opacity: cursorBlink }}
                    accessibilityLabel={t("terminal.output_label")}
                    onData={({ data }) => {
                      const current = tab();
                      if (current) write(current, data);
                    }}
                    onResize={({ cols, rows }) => {
                      if (cols < 2 || rows < 2) return;
                      const current = tab();
                      if (!current) return;
                      void props.store.agent.resizeTerminal(current.remoteId, cols, rows).catch(report);
                    }}
                  />
                </View>
              );
            }}
          </Each>
        </View>
      </If>
    </View>
  );
}
