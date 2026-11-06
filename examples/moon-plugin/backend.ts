import { host } from "nsh";

const params = (host as any).params.plugin;
const sequences: Record<string, number> = {};
let requests = 0;

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

host.on("plugin.protocol", (message: any) => {
  if (message.kind === "lifecycle.start") {
    send("lifecycle.ready", "lifecycle", { accepted: true });
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
  } else if (message.kind === "host.action" && message.body.name === "command.invoke") {
    requests += 1;
    send("service.request", "services", {
      service: "navigation_open",
      payload: { pageId: "main" },
    }, `moon-command-${requests}`);
  } else if (message.kind === "service.event" && message.body.name === "locale.changed") {
    send("log.event", "logs", { name: "locale.changed", locale: message.body.locale });
  }
});

send("lifecycle.hello", "lifecycle", {
  uiProtocol: 1,
  servicesProtocol: 1,
});
