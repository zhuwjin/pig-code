import {
  createAnimatedValue,
  effect,
  Image,
  keymap,
  reduceMotion,
  signal,
  Text,
  View,
} from "nsh";
import { Button, Dialog } from "nsh/base";
import { t } from "../i18n";
import { Icon } from "./icon";

export interface ImageViewerItem {
  src: string;
  label: string;
}

const MAX_ZOOM = 4;
const MIN_ZOOM = 0.25;
const DOUBLE_TAP_ZOOM = 2.5;
const DOUBLE_TAP_MS = 300;
const SWIPE_CLOSE = 120;

const clamp = (value: number, bound: number) => Math.max(-bound, Math.min(bound, value));

export function ImageViewer(props: {
  images: ImageViewerItem[];
  index: number | null;
  onIndexChange: (index: number) => void;
  onClose: () => void;
}) {
  const current = () => props.index === null ? undefined : props.images[props.index];
  const shown = signal<ImageViewerItem | null>(null);
  const natural = signal<{ width: number; height: number } | null>(null);
  const viewport = signal({ width: 0, height: 0 });
  const settledScale = signal(1);
  const scale = createAnimatedValue(1);
  const x = createAnimatedValue(0);
  const y = createAnimatedValue(0);
  const enter = createAnimatedValue(0);
  let at = { scale: 1, x: 0, y: 0 };
  let lastTap = 0;
  let tap = { x: 0, y: 0 };
  let src = "";

  effect(() => {
    if (props.index !== null && !current()) props.onClose();
  });

  effect(() => {
    const image = current();
    if (!image) return;
    if (image.src !== src) natural.set(null);
    src = image.src;
    shown.set(image);
    at = { scale: 1, x: 0, y: 0 };
    settledScale.set(1);
    scale.set(1);
    x.set(0);
    y.set(0);
    enter.set(reduceMotion() ? 1 : 0);
    if (!reduceMotion()) enter.spring(1, { stiffness: 280, damping: 28 });
  });

  effect(() => {
    if (props.index === null) return;
    return keymap.bind({
      left: () => navigate(-1),
      right: () => navigate(1),
    }, { when: "always" });
  });

  const fitScale = () => {
    const v = viewport();
    const n = natural();
    if (!n || !n.width || !n.height || !v.width || !v.height) return 1;
    return Math.min((v.width - 48) / n.width, (v.height - 88) / n.height);
  };

  const box = () => {
    const v = viewport();
    const n = natural();
    if (!n || !n.width || !n.height || !v.width) return v;
    const k = fitScale();
    return { width: n.width * k, height: n.height * k };
  };

  const settle = (next: { scale: number; x: number; y: number }) => {
    const s = Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, next.scale));
    const b = box();
    const v = viewport();
    at = {
      scale: s,
      x: clamp(next.x, Math.max(0, (b.width * s - v.width) / 2)),
      y: clamp(next.y, Math.max(0, (b.height * s - v.height) / 2)),
    };
    settledScale.set(at.scale);
    scale.spring(at.scale, { stiffness: 320, damping: 30 });
    x.spring(at.x, { stiffness: 320, damping: 30 });
    y.spring(at.y, { stiffness: 320, damping: 30 });
  };

  const navigate = (direction: -1 | 1) => {
    if (props.index === null) return;
    const next = props.index + direction;
    if (next >= 0 && next < props.images.length) props.onIndexChange(next);
  };

  const pressImage = () => {
    const now = Date.now();
    if (now - lastTap > DOUBLE_TAP_MS) {
      lastTap = now;
      return;
    }
    lastTap = 0;
    if (at.scale > 1.01) return settle({ scale: 1, x: 0, y: 0 });
    const b = box();
    const dx = tap.x - b.width / 2;
    const dy = tap.y - b.height / 2;
    settle({
      scale: DOUBLE_TAP_ZOOM,
      x: at.x + (at.scale - DOUBLE_TAP_ZOOM) * dx,
      y: at.y + (at.scale - DOUBLE_TAP_ZOOM) * dy,
    });
  };

  return (
    <Dialog
      open={Boolean(current())}
      animation="fade"
      style={{ flex: 1 }}
      onOpenChange={({ open }) => { if (!open) props.onClose(); }}
    >
      <View
        testID="image-viewer"
        focusable={false}
        onPress={props.onClose}
        onLayout={({ bounds }) => viewport.set({ width: bounds.size.width, height: bounds.size.height })}
        pinch={{ scale }}
        pan={{ x, y }}
        onPinchEnd={async () => settle({ ...at, scale: await scale.getValue() })}
        onPanEnd={(event) => {
          const dy = (event.value?.y ?? at.y) - at.y;
          if (at.scale <= 1.01 && (dy > SWIPE_CLOSE || event.velocity.y > 1200)) return props.onClose();
          settle({ scale: at.scale, x: event.value?.x ?? at.x, y: event.value?.y ?? at.y });
        }}
        style={{ flex: 1, alignItems: "center", justifyContent: "center", backgroundColor: "rgba(0,0,0,0.94)" }}
      >
        <View
          testID="image-viewer-frame"
          accessibilityLabel={shown()?.label}
          role="image"
          onTouchStart={(event) => { tap = { x: event.locationX, y: event.locationY }; }}
          onPress={pressImage}
          style={{
            width: box().width,
            height: box().height,
            opacity: enter,
            translate: [x, y],
            scale: scale.mul(enter.interpolate({ inputRange: [0, 1], outputRange: [0.94, 1] })),
          }}
        >
          <Image
            src={shown()?.src ?? ""}
            onLoad={({ size }) => natural.set({ width: size.width, height: size.height })}
            style={{ width: "100%", height: "100%", objectFit: "contain" }}
          />
        </View>
        <View
          class="absolute right-3 top-3 z-10 flex-row items-center gap-1 rounded-md border border-white/20 bg-black/70 px-2 py-1"
          onPress={() => {}}
        >
          <Text class="px-2 text-sm text-white/90">{shown()?.label ?? ""}</Text>
          <If when={props.images.length > 1 && props.index !== null}>
            <Button
              testID="image-viewer-prev"
              accessibilityLabel={t("image_viewer.previous")}
              disabled={props.index === 0}
              class="h-7 w-7 items-center justify-center rounded hover:bg-white/15 disabled:opacity-30"
              onPress={() => navigate(-1)}
            >
              <Icon name="chevron-left" size={16} color="#ffffff" />
            </Button>
            <Text class="px-1 text-xs text-white/60">{(props.index ?? 0) + 1}/{props.images.length}</Text>
            <Button
              testID="image-viewer-next"
              accessibilityLabel={t("image_viewer.next")}
              disabled={props.index === null || props.index + 1 >= props.images.length}
              class="h-7 w-7 items-center justify-center rounded hover:bg-white/15 disabled:opacity-30"
              onPress={() => navigate(1)}
            >
              <Icon name="chevron-right" size={16} color="#ffffff" />
            </Button>
          </If>
          <Text testID="image-viewer-zoom-label" class="px-1 text-xs text-white/60">{Math.round(fitScale() * settledScale() * 100)}%</Text>
          <Button
            testID="image-viewer-zoom-out"
            accessibilityLabel={t("image_viewer.zoom_out")}
            class="h-7 w-7 items-center justify-center rounded hover:bg-white/15"
            onPress={() => settle({ ...at, scale: at.scale * 0.8 })}
          >
            <Icon name="minus" size={15} color="#ffffff" />
          </Button>
          <Button
            testID="image-viewer-zoom-in"
            accessibilityLabel={t("image_viewer.zoom_in")}
            class="h-7 w-7 items-center justify-center rounded hover:bg-white/15"
            onPress={() => settle({ ...at, scale: at.scale * 1.25 })}
          >
            <Icon name="plus" size={15} color="#ffffff" />
          </Button>
          <Button
            testID="image-viewer-fit"
            class="rounded px-2 py-1 hover:bg-white/15"
            onPress={() => settle({ scale: 1, x: 0, y: 0 })}
          >
            <Text class="text-sm text-white/90">{t("image_viewer.fit")}</Text>
          </Button>
          <Button
            testID="image-viewer-close"
            accessibilityLabel={t("messages.close_image")}
            class="h-7 w-7 items-center justify-center rounded hover:bg-white/15"
            onPress={props.onClose}
          >
            <Icon name="x" size={16} color="#ffffff" />
          </Button>
        </View>
      </View>
    </Dialog>
  );
}
