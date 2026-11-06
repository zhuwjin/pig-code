# `pig/plugin`

The TypeScript SDK for Pig Code plugin backends. It owns the envelope protocol — instance, epoch,
generation, per-stream sequences, request ids, stale messages and the lifecycle handshake — so a
backend only declares what it offers and what it does.

```
sdk/pig-plugin/
  mod.ts        public entry (`pig/plugin`)
  index.ts      apply(), context, runtime, contributions
  protocol.ts   envelopes, sequences, identity, PluginError
  test-host.ts  createTestHost(): drive a backend without the app
```

## Wiring it up

Map the specifier where the source is bundled. An app's or plugin's `nsh.json`:

```json
{ "resolve": { "alias": { "pig/plugin": "../../sdk/pig-plugin/mod.ts" } } }
```

TypeScript needs the same specifier in its `paths` (the repository's `tsconfig.json` does this for
the host app). Vendor `sdk/pig-plugin/` next to a third-party plugin, or point the alias at its
absolute path.

## Applying

```ts
import { apply, PluginError } from "pig/plugin";

export const myPlugin = (ctx) => {
  ctx.tools.register({
    name: "DocSearch",
    description: "Search the documentation",
    parameters: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
      additionalProperties: false,
    },
    outputSchema: {
      type: "object",
      properties: { hits: { type: "array", items: { type: "string" } } },
      required: ["hits"],
    },
    readOnly: true,
    idempotent: true,
    timeoutMs: 30_000,
    handler: async ({ query }, call) => {
      if (call.signal.cancelled) throw new PluginError("cancelled", "the call was cancelled");
      return { hits: await search(query) };
    },
  });
};

apply(myPlugin);
```

`apply` starts the backend when it runs inside the host (the launch parameters name this instance)
and only records the applier otherwise, so tests can import the module.

## Context

| Member | What it does |
| --- | --- |
| `ctx.tools.register(definition)` | Registers a tool and its contract; returns a disposer. |
| `ctx.tools.select(selector)` | Replaces the default "every `availability: "plugin"` tool" selection. |
| `ctx.commands.register(definition, handler)` | Registers a command from the host's command search. |
| `ctx.providers.register(definition)` | Contributes a model-provider preset; the host executes the selected protocol adapter. Requires the manifest's `provide_providers`. |
| `ctx.hooks.register(definition)` | Contributes a hook (`session_start`, `pre_tool_use`, …); the host runner executes it. Requires the manifest's `provide_hooks`. |
| `ctx.services.request(service, payload?, options?)` | Typed host service (`config_read`, `storage_write`, `session_read`, `navigation_open`, …). |
| `ctx.services.config/storage/session` | Convenience wrappers over the services above. Keyed storage has `list/get/set/delete` with revision CAS. |
| `ctx.services.on(name, listener)` / `ctx.events.on` | Host events (`locale.changed`, …). |
| `ctx.services.session.subscribe(listener, {sessionId})` | Projected session events with cursor/epoch/dropped; call it from `onReady`. |
| `ctx.services.session.create/send/interrupt/wait` | Narrow session control (the session's own permissions apply). |
| `ctx.services.model.request({purpose, prompt, …})` | One bounded non-streaming model call. |
| `ctx.services.agents.list/start/send/wait/stop/read` | Delegate to a contributed or built-in subagent profile. |
| `ctx.services.git.status/diff/log/show/branches` | Read-only Git inspection of the session workspace; requires `git.read`. |
| `ctx.services.files.read` | Bounded workspace text read with line ranges; requires `files.read`. |
| `ctx.log.debug/info/warn/error` | Size-limited, redacted log events. |
| `ctx.onReady(listener)` | Runs once the host accepted the instance. Requests made before that are queued. |
| `ctx.dispose(fn)` | Teardown, run in reverse order on stop. |

Tool definitions take `readOnly`, `idempotent`, `approval` (`on_request` | `always` | `never`),
`timeoutMs`, `maxConcurrency`, `maxOutputBytes` and `availability` (`static` | `plugin`). The host
validates arguments and results against the declared schemas and enforces those limits; the SDK
only forwards them.

## Providers

A provider is a declaration, not a transport implementation: the host runs the selected adapter
and injects the credential.

```ts
ctx.providers.register({
  id: "acme",
  name: "Acme",
  baseUrl: "https://acme.example/v1",
  apiFormat: "OpenAiChat",
  credential: "acme-key",
  models: [
    {
      id: "acme-large",
      contextWindow: 128_000,
      maxOutputTokens: 8_192,
      reasoningLevels: ["low", "high"],
      defaultReasoningLevel: "high",
    },
  ],
});
```

The manifest must declare `provide_providers` and an `owned` credential:

```json
"actions": ["provide_providers", "manage_own_secrets"],
"credentials": [{"kind": "owned", "id": "acme-key", "label": "Acme API key"}]
```

The host exposes the provider as `plugin:<package>:acme`; the user enters the key in the plugin's
settings form. The key lives in the OS credential store and is never readable by the plugin.
Contributed providers cannot shadow user providers, and disabling or updating the package removes
the preset from the next turn.

## Capabilities

New services are granted through the manifest's `capabilities` (not `actions`):

```json
"capabilities": [
  {"name": "session.subscribe", "targets": ["*"]},
  {"name": "session.create", "targets": ["/path/to/workspace"]},
  {"name": "session.control", "targets": ["*"]},
  {"name": "model.request", "targets": ["review", "commit_message"]},
  {"name": "agent.delegate", "targets": ["*"]},
  {"name": "git.read", "targets": ["*"]},
  {"name": "files.read", "targets": ["*"]}
]
```

A project package can only touch sessions inside its trusted workspace. Tool results, session
events and agent reads are projections: the SDK never sees host stores or credentials.

Git and file reads are also session-scoped. Paths are workspace-relative, `.git` and symlinks are
refused, Git runs with fixed argv and hooks/filters disabled, and file output is line-rangeable and
bounded. Both are read-only; Git writes and worktrees are separate capabilities.

## Side-effect recovery

`storage.write`, `storage.set`, `storage.delete` and other side-effecting calls accept
`idempotencyKey`. The host persists the result under that key: a retry (even after a restart)
replays it instead of executing the effect again; a request that was in flight when the process
died returns `busy`. Keyed writes also accept `expectedRevision` for compare-and-swap, rejected as
`stale_revision` with `details.currentRevision`.

## Configuration

Manifest `config.items` are shown in the host's generic options form (string/number/boolean/enum,
validated against each item's JSON Schema). A trusted workspace's `.pigcode/plugins.json` can
override them; the form's scope switch writes either the user value or the workspace override.

## Testing

```ts
import { createTestHost } from "../sdk/pig-plugin/mod";
import { myPlugin } from "../my-plugin/applier";

const host = await createTestHost({ applier: myPlugin }).start();
await host.tool("DocSearch", { query: "hooks" });      // a tool call through the real protocol
await host.select();                                    // a tool selection
await host.command("my-command");                       // command.invoke
host.serviceRequests;                                   // what the plugin asked the host
host.answerService(requestId, value);                   // answer one
host.setSession(projection);                            // answer session_read
await host.emitEvent("locale.changed", { locale: "en" });
await host.health();
host.hooks();                                           // hook contributions, as the host sees them
host.setModelReply("reviewed");                         // answer model.request
host.emitSessionEvent(subscriptionId, { kind: "turn_started", turn_id: "t1" });
host.sendRaw(staleEnvelope);                            // stale/foreign envelopes are ignored
host.stop();
```

The test host checks every outgoing envelope (protocol, identity, per-stream sequence) and fails a
`tool()`/`select()`/`health()` call that does not answer in time. `host.errors` collects what went
wrong on the host side.
