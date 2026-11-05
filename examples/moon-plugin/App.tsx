import { effect, host, mount, setColorScheme, signal, Text, View } from "nsh";
import { Button } from "nsh/base";

const params = (host as any).params.plugin;
const sequences: Record<string, number> = {};
let request = 0;
const acknowledged = signal(0);
const hostFocused = signal(false);

function send(kind: string, stream: string, body: unknown, requestId?: string): void {
  sequences[stream] = (sequences[stream] ?? 0) + 1;
  host.post("plugin.protocol", {
    protocol: 1,
    kind,
    instance: params.instance,
    epoch: params.epoch,
    stream,
    sequence: sequences[stream],
    requestId: requestId ?? null,
    generation: params.generation,
    body,
  });
}

function current(message: any): boolean {
  return message?.protocol === 1 &&
    message.instance === params.instance &&
    message.epoch === params.epoch &&
    message.generation === params.generation;
}

host.on("plugin.protocol", (message: any) => {
  if (!current(message)) return;
  if (message.kind === "lifecycle.start") {
    send("lifecycle.ready", "lifecycle", { accepted: true });
  } else if (message.kind === "ui.ack") {
    if (acknowledged() === 0) acknowledged.set(message.body.revision);
  } else if (message.kind === "ui.event") {
    if (message.body.kind === "focus" && message.body.contribution === "moon-host-focus") {
      hostFocused.set(true);
    }
  } else if (message.kind === "lifecycle.health") {
    send("lifecycle.health", "lifecycle", { token: message.body.token });
  } else if (message.kind === "tools.call") {
    send("tools.result", "tools", {
      request_id: message.body.request_id,
      result: { echo: message.body.args?.text ?? "", engine: "moon" },
    }, message.requestId);
  } else if (message.kind === "tools.select") {
    send("tools.result", "tools", {
      request_id: message.body.request_id,
      result: ["MoonEcho"],
    }, message.requestId);
  } else if (message.kind === "service.event" && message.body.name === "locale.changed") {
    send("log.event", "logs", { name: "locale.changed", locale: message.body.locale });
  }
});

send("lifecycle.hello", "lifecycle", {
  uiProtocol: 1,
  servicesProtocol: 1,
  requiredFeatures: (2 ** 0) + (2 ** 15),
  optionalFeatures: (2 ** 2) + (2 ** 3) + (2 ** 4) + (2 ** 5) + (2 ** 6),
});

send("contribution.register", "contributions", {
  ui: [
    {
      id: "moon-dynamic-note",
      kind: "insertAfter",
      target: "composer.sendButton",
      export: "MoonDynamicNote",
      preserve: [],
    },
    {
      id: "moon-host-focus",
      kind: "patch",
      target: "composer.input",
      props: { onFocus: true },
      preserve: ["onFocus"],
    },
  ],
});

function value(response: any): any {
  if (response?.result?.Err) throw new Error(response.result.Err.message);
  if (response?.result && "Ok" in response.result) return response.result.Ok;
  throw new Error("Invalid Host API response");
}

function call(method: string, payload: unknown, requestId: string): Promise<any> {
  return (host as any).call("pig.host", {
    apiVersion: 1,
    instance: params.instance,
    surface: params.surface,
    contribution: params.contribution,
    revision: params.revision,
    requestId,
    method,
    payload,
  });
}

function MoonSurface() {
  const appearance = signal(params.appearance ?? "system");
  host.on("pig.host.environment", (message: any) => appearance.set(message.appearance));
  effect(() => setColorScheme(appearance()));
  const snapshot = signal<any>(null);
  const storage = signal("pending");
  const failure = signal("");
  const subscription = signal("");
  let snapshotRevision = 0;
  let disposed = false;
  host.on("pig.host", (message: any) => {
    if (message?.surface !== params.surface) return;
    if (message.kind === "surface.disposed") {
      failure.set("Surface disposed");
      return;
    }
    if (message?.subscriptionId !== subscription()) return;
    if (message.kind === "session.snapshot" && message.revision > snapshotRevision) {
      snapshotRevision = message.revision;
      snapshot.set(message.session);
    } else if (message.kind === "session.error") {
      failure.set(message.error?.message ?? "Session subscription failed");
    }
  });
  effect(() => {
    disposed = false;
    const start = async () => {
      try {
        const stored = await call("storage.read", null, "moon-storage-read");
        if (!disposed) storage.set(JSON.stringify(value(stored)));
        await call("storage.write", { owner: params.surface }, "moon-storage-write");
        if (!disposed) storage.set(params.surface);
        const current = await call("session.snapshot", {}, "moon-session-snapshot");
        if (!disposed) snapshot.set(value(current).session);
        const subscribed = await call("session.subscribe", {}, "moon-session-subscribe");
        const initial = value(subscribed);
        if (disposed) {
          await call("session.unsubscribe", { subscriptionId: initial.subscriptionId }, "moon-session-unsubscribe");
          return;
        }
        subscription.set(initial.subscriptionId);
        snapshotRevision = initial.revision;
        snapshot.set(initial.session);
      } catch (error) {
        if (!disposed) failure.set(String(error));
      }
    };
    void start();
    return () => {
      disposed = true;
      if (subscription()) {
        void call("session.unsubscribe", { subscriptionId: subscription() }, "moon-session-unsubscribe").catch(() => {});
      }
    };
  });
  return (
    <View class="gap-2 bg-card p-3 text-foreground">
      <Text testID="moon-surface-storage">{`Storage ${storage()} · ${params.surface}`}</Text>
      <Text testID="moon-surface-session">{`Moon turns ${snapshot()?.stats?.turns ?? 0} · tokens ${snapshot()?.stats?.usage?.used ?? 0}`}</Text>
      <Text testID="moon-surface-running">{snapshot()?.stats?.running ? "Moon running" : "Moon idle"}</Text>
      <Text testID="moon-surface-subscription">{`${params.surface} · ${subscription() || "pending"}`}</Text>
      {failure() ? <Text role="alert">{failure()}</Text> : null}
      <Button
        testID="moon-surface-main"
        onPress={() => {
          void call("navigation.open", { pageId: "main" }, "moon-navigation").catch((error) => failure.set(String(error)));
        }}
      >
        <Text>Moon open main</Text>
      </Button>
    </View>
  );
}

function RemotePanel() {
  const submitted = signal(false);
  return (
    <View accessibilityLabel="MoonPanel" class="gap-2">
      <Text testID="moon-panel-ready">Moon engine is running</Text>
      <Text testID="moon-ui-ack">{`UI ack ${acknowledged()}`}</Text>
      <Text testID="moon-host-focus">{hostFocused() ? "Host focus received" : "Host focus pending"}</Text>
      <Button
        testID="moon-submit"
        onPress={() => {
          submitted.set(true);
          request += 1;
          send("service.request", "services", {
            service: "composer_submit",
            payload: {},
          }, `moon-submit-${request}`);
        }}
      >
        <Text>{submitted() ? "Submitted through host" : "Submit through host"}</Text>
      </Button>
      <Text accessibilityLabel="MoonDynamicNote" testID="moon-dynamic-note">Dynamic moon note</Text>
    </View>
  );
}

export default function App() {
  return params.surface ? <MoonSurface /> : <RemotePanel />;
}

mount(App);
