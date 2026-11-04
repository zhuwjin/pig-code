import { PanResponder, Scroll, signal, Text, View } from "nsh";
import { Button } from "nsh/base";
import { t, tCount } from "../i18n";
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
          accessibilityLabel={t("queue.drag")}
          class="w-4 cursor-grab items-center"
          {...pan.panHandlers}
        >
          <Text class="text-xs text-muted-foreground">⋮⋮</Text>
        </View>
        <Text style={{ numberOfLines: 2 }} class="min-w-0 flex-1 text-xs leading-5 text-foreground">{item().text}</Text>
        <If when={(item().images?.length ?? 0) > 0}>
          <Text class="text-xs text-muted-foreground">{tCount("queue.image_count", item().images?.length ?? 0)}</Text>
        </If>
      </View>
      <View class="flex-row flex-wrap items-center gap-1">
        <Button
          testID={`queue-up-${item().id}`}
          accessibilityLabel={t("queue.move_up_aria", { text: item().text })}
          disabled={props.index() === 0}
          class="rounded px-2 py-0.5 hover:bg-muted disabled:opacity-30"
          onPress={() => props.store.reorderQueued(item().id, -1)}
        >
          <Text class="text-xs text-muted-foreground">{t("queue.move_up")}</Text>
        </Button>
        <Button
          testID={`queue-down-${item().id}`}
          accessibilityLabel={t("queue.move_down_aria", { text: item().text })}
          disabled={props.index() + 1 >= props.count}
          class="rounded px-2 py-0.5 hover:bg-muted disabled:opacity-30"
          onPress={() => props.store.reorderQueued(item().id, 1)}
        >
          <Text class="text-xs text-muted-foreground">{t("queue.move_down")}</Text>
        </Button>
        <Button
          testID={`queue-lane-${item().id}`}
          accessibilityLabel={t(props.steer ? "queue.move_to_queue_aria" : "queue.convert_to_steer_aria", { text: item().text })}
          class="rounded px-2 py-0.5 hover:bg-muted"
          onPress={() => props.store.moveQueuedLane(item().id, !props.steer)}
        >
          <Text class="text-xs text-muted-foreground">{t(props.steer ? "queue.move_to_queue" : "queue.convert_to_steer")}</Text>
        </Button>
        <Button
          testID={`queue-edit-${item().id}`}
          accessibilityLabel={t("queue.edit_aria", { text: item().text })}
          class="rounded px-2 py-0.5 hover:bg-muted"
          onPress={() => props.store.editQueued(item())}
        >
          <Text class="text-xs text-muted-foreground">{t("common.edit")}</Text>
        </Button>
        <View class="flex-1" />
        <Button
          testID={`queue-send-${item().id}`}
          accessibilityLabel={t("queue.send_now_aria", { text: item().text })}
          class="rounded px-2 py-0.5 hover:bg-muted"
          onPress={() => props.store.sendQueuedNow(item().id)}
        >
          <Text class="text-xs text-primary">{t("queue.send_now")}</Text>
        </Button>
        <Button
          testID={`queue-cancel-${item().id}`}
          accessibilityLabel={t("queue.cancel_aria", { text: item().text })}
          class="rounded px-2 py-0.5 hover:bg-muted"
          onPress={() => props.store.cancelQueued(item().id)}
        >
          <Text class="text-xs text-destructive">{t("common.cancel")}</Text>
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
      case "stopped": return t("queue.pause.stopped");
      case "restarted": return t("queue.pause.restarted");
      case "error": return t("queue.pause.error");
      default: return "";
    }
  };
  return (
    <View class={props.themeStore.classes("queue.root", "mb-2 gap-3 rounded-t-xl border border-input bg-composer p-3")}>
      <If when={queue().paused}>
        <View class="flex-row items-center gap-3 rounded-(--radius) border border-warning/40 bg-warning/10 px-3 py-2">
          <Text class="min-w-0 flex-1 text-xs leading-5 text-foreground">{pausedText()}</Text>
          <Button testID="queue-resume" class="rounded px-2 py-1 hover:bg-muted" onPress={() => props.store.resumeQueue()}>
            <Text class="text-xs font-medium text-foreground">{t("queue.resume")}</Text>
          </Button>
        </View>
      </If>
      <If when={queue().held}>
        <View class="gap-2 rounded-(--radius) border border-warning/40 bg-warning/10 p-3">
          <Text class="text-xs font-medium text-foreground">{t("queue.paused_title")}</Text>
          <Text style={{ numberOfLines: 2 }} class="text-xs leading-5 text-muted-foreground">{queue().held}</Text>
          <View class="flex-row flex-wrap gap-2">
            <Button testID="held-clear" class="rounded px-2 py-1 hover:bg-muted" onPress={() => props.store.resolveHeld("clear_queue_and_send")}>
              <Text class="text-xs text-destructive">{t("queue.clear_and_send")}</Text>
            </Button>
            <Button testID="held-keep" class="rounded px-2 py-1 hover:bg-muted" onPress={() => props.store.resolveHeld("keep_queue_and_send")}>
              <Text class="text-xs text-foreground">{t("queue.keep_and_enqueue")}</Text>
            </Button>
            <Button testID="held-cancel" class="rounded px-2 py-1 hover:bg-muted" onPress={() => props.store.resolveHeld("cancel")}>
              <Text class="text-xs text-muted-foreground">{t("queue.cancel_new")}</Text>
            </Button>
          </View>
        </View>
      </If>
      <Scroll class="max-h-56">
        <View class="gap-3 pr-1">
          <QueueLane {...props} title={t("queue.steer_lane")} items={queue().steer} steer />
          <QueueLane {...props} title={t("queue.pending_lane")} items={queue().pending} steer={false} />
        </View>
      </Scroll>
    </View>
  );
}
