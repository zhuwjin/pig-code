import {
  Canvas,
  Text,
  View,
  host,
  mount,
  signal,
  token,
} from "nsh";
import { Button, Input } from "nsh/base";

function metricNumber(data: unknown, key: string): number {
  if (!data || typeof data !== "object" || Array.isArray(data)) return 0;
  const value = (data as Record<string, unknown>)[key];
  return typeof value === "number" ? value : 0;
}

function metricRunning(data: unknown): boolean {
  return Boolean(data && typeof data === "object" && !Array.isArray(data)
    ? (data as Record<string, unknown>).running === true
    : false);
}

function PetalComposer() {
  const draft = signal("");
  const running = signal(false);
  const rate = signal(0);
  host.on("metrics.changed", (data) => {
    running.set(metricRunning(data) === true);
    rate.set(metricNumber(data, "tokensPerSecond") || 0);
  });
  const submit = () => {
    if (!running() && !draft().trim()) return;
    void host.call(running() ? "composer.interrupt" : "composer.submit");
    if (!running()) {
      draft.set("");
      void host.call("composer.setDraft", { text: "" });
    }
  };
  return (
    <View class="relative h-44 bg-transparent">
      <Canvas
        class="absolute inset-0"
        draw={(cx, { width, height }) => {
          const centerX = width / 2;
          const centerY = height / 2;
          const radius = Math.min(width / 5, height / 2.1);
          cx.fillStyle = token("composer");
          cx.strokeStyle = token("primary");
          cx.lineWidth = 1.5;
          cx.beginPath();
          for (let index = 0; index < 8; index += 1) {
            const angle = (index * Math.PI * 2) / 8;
            cx.save();
            cx.translate(centerX, centerY);
            cx.rotate(angle);
            cx.ellipse(0, -radius * 0.55, radius * 0.3, radius * 0.58, 0, 0, Math.PI * 2);
            cx.restore();
          }
          cx.fill();
          cx.stroke();
        }}
      />
      <View class="absolute bottom-5 left-14 right-14 top-12">
        <Input
          testID="plugin-composer-input"
          value={draft()}
          onChange={(text) => {
            draft.set(text);
            void host.call("composer.setDraft", { text });
          }}
          onSubmit={submit}
          multiline
          submitBehavior="submit"
          placeholder="写一句话…"
          class="h-full bg-transparent px-3 py-2 text-sm leading-6 text-foreground placeholder:text-muted-foreground"
        />
      </View>
      <View class="absolute bottom-2 left-6">
        <Text class="text-xs text-muted-foreground">≈ {rate()} token/s</Text>
      </View>
      <Button
        testID="plugin-send-button"
        accessibilityLabel={running() ? "停止" : "发送"}
        disabled={!running() && !draft().trim()}
        class="absolute bottom-2 right-5 h-8 items-center justify-center rounded-full bg-primary px-4 active:bg-primary/90 disabled:opacity-30"
        onPress={submit}
      >
        <Text class="text-xs text-primary-foreground">{running() ? "停止" : "发送"}</Text>
      </Button>
    </View>
  );
}

function Pet() {
  return (
    <View class="flex-1 items-center justify-center bg-transparent">
      <Text class="animate-bounce text-5xl">🐷</Text>
    </View>
  );
}

function Rate() {
  const rate = signal(0);
  const running = signal(false);
  host.on("metrics.changed", (data) => {
    rate.set(metricNumber(data, "tokensPerSecond") || 0);
    running.set(metricRunning(data) === true);
  });
  return (
    <View class="flex-1 items-center justify-center">
      <Text class="text-xs text-muted-foreground">
        {running() ? `≈ ${rate()} token/s` : "待命"}
      </Text>
    </View>
  );
}

function App() {
  switch (host.params.slot) {
    case "pet":
      return <Pet />;
    case "rate":
      return <Rate />;
    default:
      return <PetalComposer />;
  }
}

mount(App);
