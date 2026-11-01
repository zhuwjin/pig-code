import { host, mount, signal, Text, View } from "nsh";
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

export default function App() {
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

mount(App);
