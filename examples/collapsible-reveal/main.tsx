import { LayoutAnimation, List, type ListRef, mount, signal, Text, View } from "nsh";
import { Button, Collapsible } from "nsh/base";

type Row = { id: string; kind: "history" | "work" | "answer"; text?: string };

const tools = Array.from({ length: 40 }, (_, index) => ({
  id: `tool-${index}`,
  command: index % 3 === 0 ? "Get-ChildItem -Recurse" : `Read module-${index}.rs`,
  lines: 20 + index,
}));
const baseRows: Row[] = [
  { id: "intro", kind: "history", text: "Inspect the project structure and compare native reveal with list anchoring." },
  { id: "work", kind: "work" },
  { id: "answer", kind: "answer", text: "Final answer" },
];
const rows = signal<Row[]>(baseRows);
const expanded = signal(false);
const latestVisible = signal(false);
let list: ListRef | undefined;

function loadHistory(): void {
  rows.set([
    ...Array.from({ length: 80 }, (_, index): Row => ({
      id: `history-${index}`,
      kind: "history",
      text: `History answer ${index + 1}`,
    })),
    ...baseRows,
  ]);
  setTimeout(() => list?.scrollToEnd({ animated: false }), 0);
}

function showWork(): void {
  const index = rows().findIndex((row) => row.kind === "work");
  if (index >= 0) list?.scrollToIndex({ index, viewPosition: 0, animated: false });
}

mount(() => (
  <View class="h-full w-full gap-2 p-3">
    <View class="flex-row gap-2">
      <Button testID="load-history" class="rounded border border-border px-3 py-1" onPress={loadHistory}>
        <Text>Load 80 history rows</Text>
      </Button>
      <Button testID="show-work" class="rounded border border-border px-3 py-1" onPress={showWork}>
        <Text>Show work</Text>
      </Button>
      <View
        role="button"
        testID="toggle-work"
        accessibilityState={{ expanded: expanded() }}
        class="rounded border border-border px-3 py-1"
        onPress={() => {
          LayoutAnimation.configureNext(LayoutAnimation.Presets.easeInEaseOut);
          expanded.update((value) => !value);
        }}
      >
        <Text>{expanded() ? "Collapse work" : "Expand work"}</Text>
      </View>
    </View>
    <List
      ref={(node) => { list = node; }}
      testID="message-list"
      each={rows()}
      keyExtractor={(row) => row.id}
      estimatedItemSize={24}
      maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 120 }}
      scrollEventThrottle={100}
      class="min-h-0 flex-1"
      contentClass="gap-4"
      onScroll={(event) => {
        latestVisible.update(() => event.contentSize.height - event.offset.y - event.size.height > 1);
      }}
    >
      {(row) => {
        if (row().kind === "work") {
          return (
            <View>
              <Text class="text-sm text-muted-foreground">Worked for 1s</Text>
              <If when={expanded()}>
                <View class="gap-2 pt-2">
                  <Each of={tools} key={(tool) => tool.id}>
                    {(tool) => (
                      <View testID={tool().id} class="flex-row items-center gap-2 py-1">
                        <Text class="text-sm text-muted-foreground">{tool().command}</Text>
                        <Text class="text-xs text-muted-foreground">{tool().lines} lines</Text>
                      </View>
                    )}
                  </Each>
                </View>
              </If>
            </View>
          );
        }
        return <Text class="text-sm leading-6">{row().text}</Text>;
      }}
    </List>
    <Collapsible open={expanded()} reveal content={<Text testID="outside-content">Outside content</Text>} />
    <If when={latestVisible()}>
      <Button
        testID="latest-messages"
        class="absolute bottom-4 right-4 rounded-full border border-border bg-popover px-4 py-2 shadow-lg"
        onPress={() => list?.scrollToEnd({ animated: false })}
      >
        <Text>Latest messages</Text>
      </Button>
    </If>
  </View>
));
