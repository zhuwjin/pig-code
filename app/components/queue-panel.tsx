import { PanResponder, Scroll, signal, Text, View } from "nsh";
import { Button } from "nsh/base";
import type { QueueItem } from "../protocol";
import type { UiContext } from "../ui-context";

function QueueRow(props: UiContext & {
  item: () => QueueItem;
  index: () => number;
  count: number;
  steer: boolean;
}) {
  const item = () => props.item();
  const dragY = signal(0);
  const dragging = signal(false);
  const pan = PanResponder.create({
    onMoveShouldSetPanResponder: (_, gesture) => Math.abs(gesture.dy) > 3,
    onPanResponderGrant: () => dragging.set(true),
    onPanResponderMove: (_, gesture) => dragY.set(gesture.dy),
    onPanResponderRelease: (_, gesture) => {
      props.store.reorderQueuedBy(item().id, Math.round(gesture.dy / 72));
      dragY.set(0);
      dragging.set(false);
    },
    onPanResponderTerminate: () => {
      dragY.set(0);
      dragging.set(false);
    },
  });
  return (
    <View
      class={dragging()
        ? "z-10 gap-2 rounded-(--radius) border border-border bg-background px-3 py-2 opacity-90 shadow-xl"
        : "gap-2 rounded-(--radius) border border-border bg-background/70 px-3 py-2"}
      style={{ transform: [{ translateY: dragY() }] }}
    >
      <View class="flex-row items-center gap-2">
        <View
          testID={`queue-drag-${item().id}`}
          accessibilityLabel="拖动排序"
          class="w-4 cursor-grab items-center"
          {...pan.panHandlers}
        >
          <Text class="text-xs text-muted-foreground">⋮⋮</Text>
        </View>
        <Text numberOfLines={2} class="min-w-0 flex-1 text-xs leading-5 text-foreground">{item().text}</Text>
        <If when={(item().images?.length ?? 0) > 0}>
          <Text class="text-xs text-muted-foreground">{item().images?.length} 张图片</Text>
        </If>
      </View>
      <View class="flex-row flex-wrap items-center gap-1">
        <Button
          testID={`queue-up-${item().id}`}
          accessibilityLabel={`上移 ${item().text}`}
          disabled={props.index() === 0}
          class="rounded px-2 py-0.5 hover:bg-muted disabled:opacity-30"
          onPress={() => props.store.reorderQueued(item().id, -1)}
        >
          <Text class="text-xs text-muted-foreground">上移</Text>
        </Button>
        <Button
          testID={`queue-down-${item().id}`}
          accessibilityLabel={`下移 ${item().text}`}
          disabled={props.index() + 1 >= props.count}
          class="rounded px-2 py-0.5 hover:bg-muted disabled:opacity-30"
          onPress={() => props.store.reorderQueued(item().id, 1)}
        >
          <Text class="text-xs text-muted-foreground">下移</Text>
        </Button>
        <Button
          testID={`queue-lane-${item().id}`}
          accessibilityLabel={`${props.steer ? "移到队列" : "转为引导"} ${item().text}`}
          class="rounded px-2 py-0.5 hover:bg-muted"
          onPress={() => props.store.moveQueuedLane(item().id, !props.steer)}
        >
          <Text class="text-xs text-muted-foreground">{props.steer ? "移到队列" : "转为引导"}</Text>
        </Button>
        <Button
          testID={`queue-edit-${item().id}`}
          accessibilityLabel={`编辑 ${item().text}`}
          class="rounded px-2 py-0.5 hover:bg-muted"
          onPress={() => props.store.editQueued(item())}
        >
          <Text class="text-xs text-muted-foreground">编辑</Text>
        </Button>
        <View class="flex-1" />
        <Button
          testID={`queue-send-${item().id}`}
          accessibilityLabel={`立即发送 ${item().text}`}
          class="rounded px-2 py-0.5 hover:bg-muted"
          onPress={() => props.store.sendQueuedNow(item().id)}
        >
          <Text class="text-xs text-primary">立即发送</Text>
        </Button>
        <Button
          testID={`queue-cancel-${item().id}`}
          accessibilityLabel={`取消排队 ${item().text}`}
          class="rounded px-2 py-0.5 hover:bg-muted"
          onPress={() => props.store.cancelQueued(item().id)}
        >
          <Text class="text-xs text-destructive">取消</Text>
        </Button>
      </View>
    </View>
  );
}

function QueueLane(props: UiContext & { title: string; items: QueueItem[]; steer: boolean }) {
  return (
    <If when={props.items.length > 0}>
      <View class="gap-1.5">
        <Text class="px-1 text-xs text-muted-foreground">{props.title}</Text>
        <Each of={props.items} key={(item) => item.id}>
          {(item, index) => (
            <QueueRow {...props} item={item} index={index} count={props.items.length} />
          )}
        </Each>
      </View>
    </If>
  );
}

export function QueuePanel(props: UiContext) {
  const queue = () => props.store.currentQueue();
  const pausedText = () => {
    switch (queue().paused) {
      case "stopped": return "回合已停止，队列已暂停。";
      case "restarted": return "应用已重启，请确认后继续队列。";
      case "error": return "上一个回合出错，队列已暂停。";
      default: return "";
    }
  };
  return (
    <View class={props.themeStore.classes("queue.root", "mb-2 gap-3 rounded-t-xl border border-input bg-composer p-3")}>
      <If when={queue().paused}>
        <View class="flex-row items-center gap-3 rounded-(--radius) border border-warning/40 bg-warning/10 px-3 py-2">
          <Text class="min-w-0 flex-1 text-xs leading-5 text-foreground">{pausedText()}</Text>
          <Button testID="queue-resume" class="rounded px-2 py-1 hover:bg-muted" onPress={() => props.store.resumeQueue()}>
            <Text class="text-xs font-medium text-foreground">继续队列</Text>
          </Button>
        </View>
      </If>
      <If when={queue().held}>
        <View class="gap-2 rounded-(--radius) border border-warning/40 bg-warning/10 p-3">
          <Text class="text-xs font-medium text-foreground">队列已暂停，如何处理新消息？</Text>
          <Text numberOfLines={2} class="text-xs leading-5 text-muted-foreground">{queue().held}</Text>
          <View class="flex-row flex-wrap gap-2">
            <Button testID="held-clear" class="rounded px-2 py-1 hover:bg-muted" onPress={() => props.store.resolveHeld("clear_queue_and_send")}>
              <Text class="text-xs text-destructive">清空队列并发送</Text>
            </Button>
            <Button testID="held-keep" class="rounded px-2 py-1 hover:bg-muted" onPress={() => props.store.resolveHeld("keep_queue_and_send")}>
              <Text class="text-xs text-foreground">保留并加入队列</Text>
            </Button>
            <Button testID="held-cancel" class="rounded px-2 py-1 hover:bg-muted" onPress={() => props.store.resolveHeld("cancel")}>
              <Text class="text-xs text-muted-foreground">取消新消息</Text>
            </Button>
          </View>
        </View>
      </If>
      <Scroll class="max-h-56">
        <View class="gap-3 pr-1">
          <QueueLane {...props} title="当前回合引导" items={queue().steer} steer />
          <QueueLane {...props} title="后续消息队列" items={queue().pending} steer={false} />
        </View>
      </Scroll>
    </View>
  );
}
