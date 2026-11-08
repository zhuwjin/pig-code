import { PanResponder, Scroll, signal, Text, token, View } from "nsh";
import { Button, Tooltip } from "nsh/base";
import { t, tCount } from "../i18n";
import type { QueueItem } from "../protocol";
import type { UiContext } from "../ui-context";
import { Icon, type IconName } from "./icon";

function QueueTooltip(props: { label: string }) {
  return (
    <Tooltip delay={300} class="max-w-80 rounded-md border border-border bg-popover px-2 py-1 shadow-lg">
      <Text class="text-xs text-foreground">{props.label}</Text>
    </Tooltip>
  );
}

function QueueAction(props: {
  testID: string;
  label: string;
  accessibilityLabel: string;
  icon: IconName;
  onPress: () => void;
  danger?: boolean;
}) {
  return (
    <Button
      testID={props.testID}
      accessibilityLabel={props.accessibilityLabel}
      tooltip={<QueueTooltip label={props.label} />}
      class={props.danger
        ? "h-7 w-7 shrink-0 items-center justify-center rounded-lg hover:bg-destructive/30"
        : "h-7 w-7 shrink-0 items-center justify-center rounded-lg hover:bg-accent/50"}
      onPress={props.onPress}
    >
      <Icon name={props.icon} size={16} color={token("muted-foreground")} />
    </Button>
  );
}

function QueueRow(props: UiContext & {
  item: () => QueueItem;
  steer: boolean;
}) {
  const item = () => props.item();
  const dragY = signal(0);
  const dragging = signal(false);
  const pan = PanResponder.create({
    onMoveShouldSetPanResponder: (_, gesture) => Math.abs(gesture.dy) > 3 && Math.abs(gesture.dy) > Math.abs(gesture.dx),
    onPanResponderGrant: () => dragging.set(true),
    onPanResponderMove: (_, gesture) => dragY.set(gesture.dy),
    onPanResponderRelease: (_, gesture) => {
      props.store.reorderQueuedBy(item().id, Math.round(gesture.dy / 38));
      dragY.set(0);
      dragging.set(false);
    },
    onPanResponderTerminate: () => {
      dragY.set(0);
      dragging.set(false);
    },
  });
  const laneLabel = () => t(props.steer ? "queue.steer_lane" : "queue.pending_lane");
  const switchLabel = () => t(props.steer ? "queue.move_to_queue" : "queue.convert_to_steer");
  return (
    <View
      testID={`queue-row-${item().id}`}
      class={dragging()
        ? "relative z-10 w-full flex-row items-center gap-2 rounded-lg bg-accent px-1.5 py-1 opacity-90 shadow-lg"
        : "relative w-full flex-row items-center gap-2 rounded-lg px-1.5 py-1 hover:bg-accent/30"}
      style={{ transform: [{ translateY: dragY() }] }}
    >
      <View
        testID={`queue-drag-${item().id}`}
        accessibilityLabel={t("queue.drag_aria", { text: item().text })}
        tooltip={<QueueTooltip label={t("queue.drag")} />}
        class="h-7 w-7 shrink-0 cursor-grab items-center justify-center rounded-lg"
        {...pan.panHandlers}
      >
        <Icon name="grip-vertical" size={16} color={token("muted-foreground")} />
      </View>
      <If when={(item().images?.length ?? 0) > 0}>
        <View
          accessibilityLabel={tCount("queue.image_count", item().images?.length ?? 0)}
          class="shrink-0 flex-row items-center gap-1"
        >
          <Icon name="image" size={14} color={token("muted-foreground")} />
          <Text class="text-xs text-muted-foreground">{item().images?.length ?? 0}</Text>
        </View>
      </If>
      <View
        testID={`queue-lane-icon-${item().id}`}
        accessibilityLabel={laneLabel()}
        tooltip={<QueueTooltip label={laneLabel()} />}
        class="shrink-0"
      >
        <Icon name={props.steer ? "send" : "list-ordered"} size={16} color={token("muted-foreground")} />
      </View>
      <Text
        testID={`queue-preview-${item().id}`}
        style={{ numberOfLines: 1 }}
        class="min-w-0 flex-1 truncate text-sm text-foreground"
      >
        {item().text}
      </Text>
      <QueueAction
        testID={`queue-lane-${item().id}`}
        label={switchLabel()}
        accessibilityLabel={t(props.steer ? "queue.move_to_queue_aria" : "queue.convert_to_steer_aria", { text: item().text })}
        icon="arrow-right-left"
        onPress={() => props.store.moveQueuedLane(item().id, !props.steer)}
      />
      <Button
        testID={`queue-send-${item().id}`}
        accessibilityLabel={t("queue.send_now_aria", { text: item().text })}
        class="h-7 shrink-0 flex-row items-center gap-1 rounded-md bg-secondary px-2 hover:bg-secondary/80"
        onPress={() => props.store.sendQueuedNow(item().id)}
      >
        <Icon name="arrow-up" size={14} color={token("foreground")} />
        <Text class="text-xs text-foreground">{t("queue.send_now")}</Text>
      </Button>
      <QueueAction
        testID={`queue-edit-${item().id}`}
        label={t("queue.edit")}
        accessibilityLabel={t("queue.edit_aria", { text: item().text })}
        icon="square-pen"
        onPress={() => props.store.editQueued(item())}
      />
      <QueueAction
        testID={`queue-cancel-${item().id}`}
        label={t("queue.cancel")}
        accessibilityLabel={t("queue.cancel_aria", { text: item().text })}
        icon="trash"
        danger
        onPress={() => props.store.cancelQueued(item().id)}
      />
    </View>
  );
}

function QueueLane(props: UiContext & { items: QueueItem[]; steer: boolean }) {
  return (
    <Each of={props.items} key={(item) => item.id}>
      {(item) => <QueueRow {...props} item={item} />}
    </Each>
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
    <View
      testID="queue-panel"
      class={props.themeStore.classes("queue.root", "-mb-4 gap-1 rounded-t-xl border border-input bg-composer p-1 pb-5")}
    >
      <If when={queue().paused}>
        <View testID="queue-paused-banner" class="min-h-10 w-full flex-row items-center gap-3 rounded-lg border border-border/70 px-3 py-2">
          <Text class="min-w-0 flex-1 text-sm text-foreground">{pausedText()}</Text>
          <Button testID="queue-resume" class="shrink-0 rounded-md px-2 py-0.5 hover:bg-accent/50" onPress={() => props.store.resumeQueue()}>
            <Text class="text-sm text-muted-foreground">{t("queue.resume")}</Text>
          </Button>
        </View>
      </If>
      <If when={queue().held}>
        <View testID="held-queue-prompt" class="w-full flex-row items-center gap-2 rounded-lg border border-border/70 bg-warning/15 px-3 py-2">
          <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 truncate text-sm text-foreground">
            {t("queue.held_prompt", { text: Array.from(queue().held ?? "").slice(0, 60).join("") })}
          </Text>
          <View class="shrink-0 flex-row items-center gap-1">
            <Button testID="held-clear" class="rounded-md px-2 py-0.5 hover:bg-accent/50" onPress={() => props.store.resolveHeld("clear_queue_and_send")}>
              <Text class="text-xs text-muted-foreground">{t("queue.clear_and_send")}</Text>
            </Button>
            <Button testID="held-keep" class="rounded-md px-2 py-0.5 hover:bg-accent/50" onPress={() => props.store.resolveHeld("keep_queue_and_send")}>
              <Text class="text-xs text-muted-foreground">{t("queue.keep_and_enqueue")}</Text>
            </Button>
            <Button testID="held-cancel" class="rounded-md px-2 py-0.5 hover:bg-accent/50" onPress={() => props.store.resolveHeld("cancel")}>
              <Text class="text-xs text-muted-foreground">{t("common.cancel")}</Text>
            </Button>
          </View>
        </View>
      </If>
      <Scroll testID="queue-list" class="max-h-[122px]">
        <View class="w-full gap-0.5 pr-3">
          <QueueLane {...props} items={queue().steer} steer />
          <QueueLane {...props} items={queue().pending} steer={false} />
        </View>
      </Scroll>
    </View>
  );
}
