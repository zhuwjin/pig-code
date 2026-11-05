import { effect, host, keymap, mount, setColorScheme, signal, Text, View } from "nsh";
import { Button } from "nsh/base";

const params = (host as any).params.plugin;
const appearance = signal(params.appearance ?? "system");
host.on("pig.host.environment", (message: any) => appearance.set(message.appearance));
effect(() => setColorScheme(appearance()));

let request = 0;

function value(response: any): any {
  if (response?.result?.Err) throw new Error(response.result.Err.message);
  if (response?.result && "Ok" in response.result) return response.result.Ok;
  throw new Error("Invalid Host API response");
}

function call(method: string, payload: unknown): Promise<any> {
  request += 1;
  return (host as any).call("pig.host", {
    apiVersion: 1,
    instance: params.instance,
    surface: params.surface,
    contribution: params.contribution,
    revision: params.revision,
    requestId: `moon-${request}`,
    method,
    payload,
  });
}

effect(() => {
  if (params.contribution !== "moon-popover") return;
  return keymap.bind("escape", () => {
    void call("overlay.close", null).catch(() => {});
  }, { when: "always" });
});

function SessionSummary() {
  const snapshot = signal<any>(null);
  const failure = signal("");
  const subscription = signal("");
  effect(() => {
    let disposed = false;
    const start = async () => {
      try {
        if (!params.context.sessionId) return;
        const subscribed = await call("session.subscribe", {});
        const initial = value(subscribed);
        if (disposed) {
          await call("session.unsubscribe", { subscriptionId: initial.subscriptionId });
          return;
        }
        subscription.set(initial.subscriptionId);
        snapshot.set(initial.session);
      } catch (error) {
        if (!disposed) failure.set(String(error));
      }
    };
    void start();
    return () => {
      disposed = true;
      if (subscription()) {
        void call("session.unsubscribe", { subscriptionId: subscription() }).catch(() => {});
      }
    };
  });
  host.on("pig.host", (message: any) => {
    if (message?.surface !== params.surface || message?.subscriptionId !== subscription()) return;
    if (message.kind === "session.snapshot") snapshot.set(message.session);
    if (message.kind === "session.error") failure.set(message.error?.message ?? "Session subscription failed");
  });
  return (
    <View class="gap-1">
      <Text class="text-sm text-muted-foreground">{`Turns ${snapshot()?.stats?.turns ?? 0}`}</Text>
      <Text class="text-sm text-muted-foreground">{`Tokens ${snapshot()?.stats?.usage?.used ?? 0}`}</Text>
      {failure() ? <Text role="alert">{failure()}</Text> : null}
    </View>
  );
}

function MoonPage() {
  return (
    <View testID="moon-page" class="min-h-0 flex-1 gap-4 bg-background p-6">
      <Text class="text-2xl font-semibold text-foreground">Moon workspace</Text>
      <Text class="text-sm text-muted-foreground">This page is rendered by the plugin Surface.</Text>
      <SessionSummary />
      <Button
        testID="moon-open-main"
        class="self-start rounded-(--radius) px-3 py-2 hover:bg-accent"
        onPress={() => void call("navigation.open", { pageId: "main" }).catch(() => {})}
      >
        <Text class="text-foreground">Open conversation</Text>
      </Button>
    </View>
  );
}

function MoonSettings() {
  const stored = signal("Pending");
  effect(() => {
    void call("storage.read", null).then((response) => stored.set(JSON.stringify(value(response)))).catch(() => stored.set("Unavailable"));
  });
  return (
    <View testID="moon-settings" class="min-h-0 flex-1 gap-3 bg-background p-6">
      <Text class="text-xl font-semibold text-foreground">Moon settings</Text>
      <Text class="text-sm text-muted-foreground">{`Storage ${stored()}`}</Text>
      <Button class="self-start rounded-(--radius) px-3 py-2 hover:bg-accent" onPress={() => void call("storage.write", { owner: params.surface })}>
        <Text class="text-foreground">Write private value</Text>
      </Button>
    </View>
  );
}

function MoonPopover() {
  return (
    <View testID="moon-popover" class="gap-3 bg-popover p-3">
      <Text class="text-sm font-semibold text-popover-foreground">Moon details</Text>
      <SessionSummary />
      <Button
        testID="moon-close"
        class="self-start rounded-(--radius) px-2 py-1 hover:bg-accent"
        onPress={() => void call("overlay.close", null).catch(() => {})}
      >
        <Text class="text-foreground">Close</Text>
      </Button>
    </View>
  );
}

export default function App() {
  if (params.contribution === "moon-page") return <MoonPage />;
  if (params.contribution === "moon-settings") return <MoonSettings />;
  return <MoonPopover />;
}

mount(App);
