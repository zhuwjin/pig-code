import { host } from "nsh";

const params = (host as any).params.plugin;
const sequences: Record<string, number> = {};
let requests = 0;
let submissions = 0;

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

function publish(): void {
  send("ui.publish", "ui", [
    {
      id: "moon-metric",
      value: `Moon ${submissions}`,
      details: [{ label: "Submissions", value: String(submissions) }],
    },
    { id: "moon-action", enabled: true },
    { id: "moon-popover", label: `Moon ${submissions}` },
  ]);
}

host.on("plugin.protocol", (message: any) => {
  if (message.kind === "lifecycle.start") {
    send("lifecycle.ready", "lifecycle", { accepted: true });
    publish();
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
  } else if (message.kind === "ui.event" && message.body.kind === "invoke") {
    submissions += 1;
    publish();
    requests += 1;
    send("service.request", "services", {
      service: "composer_submit",
      payload: {},
    }, `moon-submit-${requests}`);
  } else if (message.kind === "ui.event" && message.body.contribution === "moon-popover" && message.body.kind === "change") {
    send("ui.publish", "ui", [{
      id: "moon-popover",
      label: `Moon ${submissions} · ${message.body.value === true ? "open" : "closed"}`,
    }, {
      id: "moon-metric",
      value: message.body.value === true ? `Open ${submissions}` : `Closed ${submissions}`,
    }]);
  } else if (message.kind === "service.event" && message.body.name === "locale.changed") {
    send("log.event", "logs", { name: "locale.changed", locale: message.body.locale });
  }
});

send("lifecycle.hello", "lifecycle", {
  uiProtocol: 1,
  servicesProtocol: 1,
});
