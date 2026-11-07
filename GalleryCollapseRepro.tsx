import { Button, TextView } from "nsh/base";
import {
  effect,
  LayoutAnimation,
  List,
  type ListRef,
  mount,
  Scroll,
  signal,
  type Signal,
  Text,
  token,
  View,
} from "nsh";
import { Icon } from "./app/components/icon";

const workOpen = signal(true);
const reasoningOpen = signal(false);
const toolOpen = signal(false);
const following = signal(true);
let list: ListRef | undefined;
let dragging = false;

function toggle(open: Signal<boolean>): void {
  LayoutAnimation.configureNext(LayoutAnimation.Presets.easeInEaseOut);
  open.update((value) => !value);
}

const reasoning = Array.from({ length: 36 }, (_, index) =>
  `Reasoning section ${index + 1}: inspect the repository structure, compare the native list behavior with the layout animation, and keep the visible content anchored while the reasoning region opens and closes.`,
).join("\n\n");
const output = Array.from({ length: 110 }, (_, index) =>
  `${String(index + 1).padStart(3, "0")}  crates/pig-core/src/session/runner/plugin_sessions.rs  plugin capability checks and session output`,
).join("\n");
const finalText = [
  "## 一句话概括",
  "This page uses the Gallery AI Chat approach: conditional content, global LayoutAnimation, a virtual List, and bottom following.",
  ...Array.from({ length: 20 }, (_, index) => `Section ${index + 1}: the final answer follows the expanding and collapsing work region without using the custom ListCollapsible component.`),
  "## 当前状态",
  "Use the controls above or the row headers to compare the transition with the list-aware animated container.",
].join("\n\n");
const rows = [{ id: "work" }, { id: "final" }];

function WorkContent() {
  return (
    <View class="gap-4 pt-4">
      <View>
        <Button
          testID="gallery-reasoning"
          class="flex-row items-center gap-2 self-start py-1"
          onPress={() => toggle(reasoningOpen)}
        >
          <Icon name="brain" size={16} color={token("muted-foreground")} />
          <Text class="text-sm text-muted-foreground">Thought for 46 s</Text>
          <Icon name={reasoningOpen() ? "chevron-down" : "chevron-right"} size={13} color={token("muted-foreground")} />
        </Button>
        <If when={reasoningOpen()}>
          <View class="ml-2 mt-1 max-h-80 border-l border-border pl-3.5">
            <Scroll class="max-h-80">
              <Text class="text-sm leading-6 text-muted-foreground">{reasoning}</Text>
            </Scroll>
          </View>
        </If>
      </View>
      <Each of={Array.from({ length: 22 }, (_, index) => index)} key={(index) => index}>
        {(index) => (
          <View class="flex-row items-center gap-2 py-1">
            <Icon name={index() % 3 === 0 ? "square-terminal" : "eye"} size={16} color={token("muted-foreground")} />
            <Text class="text-sm text-muted-foreground">
              {index() % 3 === 0 ? "Terminal  ls -la && echo app" : `Read  crates/pig-core/src/module-${index()}.rs`}
            </Text>
          </View>
        )}
      </Each>
      <View>
        <Button
          testID="gallery-tool"
          class="flex-row items-center gap-2 self-start py-1"
          onPress={() => toggle(toolOpen)}
        >
          <Icon name="square-terminal" size={16} color={token("muted-foreground")} />
          <Text class="text-sm text-muted-foreground">Terminal  find crates -name '*.rs'</Text>
          <Icon name={toolOpen() ? "chevron-down" : "chevron-right"} size={13} color={token("muted-foreground")} />
        </Button>
        <If when={toolOpen()}>
          <View class="mt-2 h-80 overflow-hidden rounded-xl border border-border bg-tool-surface">
            <Scroll contentClass="p-3">
              <Text class="font-mono text-xs leading-5 text-foreground">{output}</Text>
            </Scroll>
          </View>
        </If>
      </View>
    </View>
  );
}

effect(() => {
  const timer = setTimeout(() => list?.scrollToEnd({ animated: false }), 0);
  return () => clearTimeout(timer);
});

mount(() => (
  <View class="h-full w-full">
    <View class="flex-row flex-wrap items-center gap-2 border-b border-border px-4 py-2">
      <Button testID="gallery-work" class="rounded-full border border-border px-3 py-1" onPress={() => toggle(workOpen)}>
        <Text class="text-xs text-foreground">{workOpen() ? "Hide work" : "Show work"}</Text>
      </Button>
      <Button class="rounded-full border border-border px-3 py-1" onPress={() => list?.scrollToIndex({ index: 0, viewPosition: 0, animated: true })}>
        <Text class="text-xs text-foreground">Go to work</Text>
      </Button>
      <Button class="rounded-full border border-border px-3 py-1" onPress={() => list?.scrollToEnd({ animated: true })}>
        <Text class="text-xs text-foreground">Latest</Text>
      </Button>
      <Button
        class="rounded-full border border-border px-3 py-1"
        onPress={() => {
          if (workOpen()) toggle(workOpen);
          setTimeout(() => {
            if (following()) list?.scrollToEnd({ animated: true });
          }, 360);
        }}
      >
        <Text class="text-xs text-foreground">Finish turn</Text>
      </Button>
    </View>
    <List
      ref={(node) => { list = node; }}
      testID="gallery-message-list"
      each={rows}
      keyExtractor={(row) => row.id}
      estimatedItemSize={140}
      maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 40 }}
      class="min-w-0 flex-1"
      contentClass="gap-4"
      scrollbar
      header={<View class="h-4" />}
      footer={<View class="h-4" />}
      onScrollDragStart={() => { dragging = true; }}
      onMomentumEnd={() => { dragging = false; }}
      onScrollEnd={() => { dragging = false; }}
      onScroll={({ contentSize, offset, size }) => {
        if (dragging) following.set(contentSize.height - offset.y - size.height <= 40);
      }}
    >
      {(row) => (
        <View class="mx-auto w-full max-w-[860px] px-4">
          <If when={row().id === "work"}>
            <View>
              <Button class="flex-row items-center gap-1 self-start py-1" onPress={() => toggle(workOpen)}>
                <Text class="text-xs text-muted-foreground">Worked for 57 s</Text>
                <Icon name={workOpen() ? "chevron-down" : "chevron-right"} size={13} color={token("muted-foreground")} />
              </Button>
              <If when={workOpen()}><WorkContent /></If>
            </View>
          </If>
          <If when={row().id === "final"}>
            <TextView markdown={finalText} selectable classes={{ root: "text-sm leading-6 text-foreground" }} />
          </If>
        </View>
      )}
    </List>
  </View>
));
