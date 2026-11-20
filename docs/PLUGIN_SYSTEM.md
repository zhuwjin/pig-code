# Plugin system

Pig Code installs signed NSH packages without rebuilding or restarting the app. The main branch supports Agent capabilities and declarative resources. It does not render plugin-provided UI.

## Scope

A package can contribute:

- Agent tools, including model- and session-aware tool selection;
- commands invoked from the host's command search;
- themes and style packs;
- Skills;
- MCP servers;
- configuration declarations;
- lifecycle hooks (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `PostToolUseFailure`, `Stop`);
- HTTP origins and credential declarations;
- versioned host-service requests permitted by the package.

Plugin pages, panels, slots, popovers, dialogs, remote node trees, nested AppViews, native Surfaces and host-Engine component loading are not supported. Plugins cannot patch, replace, wrap, move, remove or otherwise modify host components. The `feat/plugin-surface-host` branch retains the UI experiment; none of its native UI bridges or NSH fork dependencies are part of the main branch.

Themes are deliberately supported even though they affect appearance: they are validated declarative resources, not plugin-rendered UI.

## Package model

A manifest contains package identity, an optional backend entry, capability requests and contributions:

```json
{
  "apiVersion": 1,
  "id": "com.example.plugin",
  "name": {"en": "Example"},
  "version": "1.0.0",
  "build": 1,
  "backend": "backend.ts",
  "minHostVersion": "1.0.0",
  "protocols": {"ui": 1, "services": 1},
  "capabilities": [
    {"name": "tools.provide", "targets": ["DocSearch"]}
  ],
  "contributions": {
    "api_version": 1,
    "tools": [{
      "name": "DocSearch",
      "description": "Search documentation",
      "parameters": {"type": "object"},
      "read_only": true,
      "availability": "static"
    }],
    "actions": ["provide_tools"]
  }
}
```

`backend` is optional for resource-only packages. Tools and commands require it. The backend runs in an independent NSH Engine and communicates only through the versioned `plugin.protocol` envelope. It does not receive host components, a host Store or a plugin UI Surface.

The `protocols.ui` property is the retained name of the envelope protocol version for package compatibility. It does not enable UI rendering.

`examples/publish-plugin.rs` leaves the package's main NSH bundle empty and compiles the backend separately. The signed assets contain `.pig-backend.nshc` and `.pig-backend-version`. Installation verifies the package before loading that bytecode, and the runtime checks that its bytecode version matches the linked NSH Engine.

Pure theme, Skill or other resource packages need no Engine. A package with a backend starts one when enabled.

## Contributions

`PluginContributions` supports these fields:

- `tools`: name, description, input and optional output JSON Schema, read-only marker, `static` or `plugin` availability, approval stance, timeout, concurrency, output budget and idempotency (see [Tool contract](#tool-contract));
- `commands`: stable ID, localized title, optional export, order and enabled state;
- `hooks`: one of the seven hook events plus a `HookEntry` (command or process, matcher, timeout, async, label), behind `provide_hooks`;
- `themes`: validated theme definitions and package-local background assets;
- `skills`: package directories containing `SKILL.md`;
- `mcp`: validated MCP server configurations;
- `providers`: model-provider presets (endpoint, host adapter, models and an optional owned credential), behind `provide_providers`;
- `credentials`: plugin-owned or provider credential declarations;
- `http`: explicit HTTP(S) origins, GET/POST methods and allowed credentials;
- `config`: plugin defaults and allowed host configuration reads/writes;
- `actions`: nonvisual service permissions such as `provide_tools`, `provide_themes`, `provide_mcp`, `provide_providers`, `http`, `read_config` and `write_config`.

Manifest tools, commands, hooks, themes, Skills and MCP servers can be supplemented at runtime through `contribution.register` and `contribution.remove`. Runtime registration cannot change credentials, HTTP origins, configuration schemas or permissions; a runtime hook still needs the manifest's static `provide_hooks` action, and a runtime tool is validated against the same contract as a manifest one. Duplicate IDs are rejected or replaced deterministically, and committed contributions are removed when their backend stops.

## Backend protocol

All backend messages use the same envelope:

```text
protocol, kind, instance, epoch, stream, sequence,
requestId?, generation, body
```

Supported streams are `lifecycle`, `events`, `contributions`, `tools`, `services` and `logs`. Sequence tracking is independent per stream. Instance, epoch and generation checks reject stale backends and late replies.

Lifecycle includes hello/start/ready, health checks, stop/failure, retry with bounded backoff, and exact-digest updates. Tool traffic includes selection, calls, results, timeouts and cancellation. Logs are size-limited and recursively redacted before reaching the host.

Commands are resolved against manifest and committed runtime contributions. Invoking one sends `command.invoke` to the active backend; commands do not open plugin pages.

`tools.cancel` tells a backend that a call was settled elsewhere (cancel, timeout, disable): the SDK aborts the handler's signal and does not answer the call. Late `tools.result` messages for settled calls are dropped, so a cancelled or disabled tool can never come back.

## Plugin SDK (`pig/plugin`)

Plugin backends do not hand-roll the envelope. `sdk/pig-plugin` (imported as `pig/plugin`, mapped in `nsh.json`'s `resolve.alias`) owns:

- `apply(context)` plus disposers for every registration;
- tools, commands and hooks registration, including runtime contributions;
- typed service requests (`ctx.services.request("navigation_open", …)`) and host event subscriptions;
- structured `PluginError { code, message, retryable }`;
- instance/epoch/generation, per-stream sequences and stale-envelope filtering;
- a plugin test host (`createTestHost`) that drives the real protocol: `tool`, `select`, `command`, `emitEvent`, `health`, `stop`, `sendRaw` for stale messages, captured service requests and hook contributions.

```ts
import { apply, PluginError } from "pig/plugin";

apply((ctx) => {
  ctx.tools.register({
    name: "DocSearch",
    description: "Search the documentation",
    parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] },
    readOnly: true,
    handler: async ({ query }) => ({ hits: await search(query) }),
  });
});
```

`nsh.json` of an app or plugin points at the SDK source:

```json
{"resolve": {"alias": {"pig/plugin": "../../sdk/pig-plugin/mod.ts"}}}
```

The app's `tsconfig.json` maps the same specifier to the SDK for type checking. `sdk/pig-plugin/README.md` documents the full surface.

## Tool contract

Every plugin tool declares its contract in the manifest or at runtime:

- `parameters` and `output_schema` (JSON Schema, validated before the call and after it);
- `read_only`, `idempotent`, and `approval` (`on_request`, `always`, `never` — the last only for read-only tools);
- `timeout_ms` (default 60 s, hard cap 10 min) and `max_concurrency` (default 8, max 32);
- `max_output_bytes` (default 32 KiB): a larger result is written to `{data_dir}/sessions/{session}/tool-results/` and only a head/tail preview reaches the model.

The accepted JSON Schema subset is closed: `type`, `enum`, `const`, `properties`, `required`, `additionalProperties`, `items`, `minItems`/`maxItems`, `minLength`/`maxLength`, `pattern`, `minimum`/`maximum`, `exclusiveMinimum`/`exclusiveMaximum`, `anyOf`/`oneOf`/`allOf`. Another keyword (including `$ref`) fails installation instead of silently not being enforced. Invalid input never reaches the backend; invalid output never reaches the model. Both arrive as structured failures with `invalid_input` / `invalid_output` codes.

## Model provider presets

A package can contribute model providers. It supplies declarations only: the host runs every
request through one of its existing protocol adapters (`OpenAiChat`, `AnthropicMessages`,
`OpenAiResponses`). Plugins cannot replace the agent loop or execute request code inside the host.

```json
{
  "actions": ["provide_providers", "manage_own_secrets"],
  "credentials": [{"kind": "owned", "id": "acme-key", "label": "Acme API key"}],
  "providers": [{
    "id": "acme",
    "name": "Acme",
    "base_url": "https://acme.example/v1",
    "api_format": "OpenAiChat",
    "credential": "acme-key",
    "models": [{
      "id": "acme-large",
      "context_window": 128000,
      "max_output_tokens": 8192,
      "input_image": true,
      "cap_structured": true,
      "reasoning_levels": ["low", "high"],
      "reasoning_params": {"high": {"effort": "high"}}
    }]
  }]
}
```

- The effective provider id is namespaced as `plugin:<package>:<id>`, so a package can never
  shadow a user-configured provider. Two packages contributing the same raw id resolve
  deterministically: the first package by id wins and the snapshot records a `provider` conflict.
- `credential` must name an `owned` credential declaration. The user enters the secret in the
  plugin's settings form; it is stored in the OS credential store under the digest-scoped plugin
  scope and injected by the host. The plugin never reads it back. Reusing another provider's
  credential (`kind: "provider"`) is rejected for provider contributions.
- `base_url` must be HTTPS (HTTP only for loopback) and must not embed credentials. Model ids are
  unique per provider; `0 < max_output_tokens <= context_window`.
- Enabling, updating or disabling a package rebuilds each session's runtime view, so the model
  picker and the next turn see the change. A removed provider falls back through the existing
  model-resolution path with a warning; it never restores a stale endpoint.
- This item deliberately has no generic OAuth engine. A provider that only offers OAuth is either
  native host support (with the host owning tokens and refresh) or exposes an API-key/token path.
  A plugin-owned credential can be filled by the plugin itself after a device-code flow, but the
  host does not implement authorization flows or refresh scheduling for plugins.

## Host version

`minHostVersion` is parsed as SemVer and compared with the host's plugin-capability version (`PLUGIN_HOST_VERSION`, currently `1.0.0`). A package that needs a newer host stays visible with an `incompatible` diagnostic: its files, configuration and the user's enable choice are kept, but its backend never starts and none of its contributions activate. A malformed version string is a manifest error. Raising the host version activates the same package without touching the choice.

## Hooks

Package hooks reuse the existing `HookSet`/`HookRunner` and `HooksConfig`:

- only the seven event names are accepted, and the package must declare `provide_hooks`;
- the user's master switch gates them, and project packages need a trusted workspace;
- they run through the same shell/process resolution, payload, matcher, timeout, cancellation, output cap and status rows as user and project hooks;
- each run re-checks the declaring package (enabled, same digest, still declaring that hook), so disabling, updating or uninstalling the package revokes it immediately — no old callback survives.

## Resource budgets and isolation

A backend Engine has a 64 MiB memory budget, operation and call-time budgets, and no canvas-operation allowance. Loading and health checks have independent deadlines. Tool and service requests have concurrency limits, and private storage is limited to 1 MiB per plugin scope. These values are limits enforced by the host, not measurements of actual memory or CPU use.

NSH does not currently expose per-Engine total memory or CPU-time measurements. Its performance monitor provides per-Engine JS heap, object and node counts plus engine-thread busy time, while the process sampler's CPU and memory cover the whole host process. Runtime metrics therefore report pending, timed-out and cancelled requests, dropped log events, network bytes and storage bytes rather than presenting resource budgets or process-wide samples as a plugin's observed usage.

## Host services

The backend can request only declared and authorized services:

- private plugin storage reads and writes;
- allowed configuration reads and writes;
- allowed HTTP requests with explicit credential injection;
- owned credential set/clear and separately authorized provider credentials;
- session and workspace projections;
- read-only Git inspection (`git.read`) and bounded workspace text reads (`files.read`);
- navigation to built-in host pages;
- Composer submission;
- package resource reads;
- locale reads and locale-change events.

Navigation targets are limited to `main`, `models`, `appearance`, `plugins` and `marketplace`. There is no plugin-page navigation API and no Surface-specific `pig.host` channel.

Requests have timeouts and concurrency limits. Disabling, updating or uninstalling a package stops its backend, cancels pending tools and services, removes runtime contributions and drops late replies.

## Workspace inspection: Git and files

A plugin can inspect a session's workspace without host stores or an arbitrary
working directory. Every request names a session; the host resolves the cwd,
checks the package's `git.read`/`files.read` capability against that session,
and enforces the project workspace boundary.

`git_status`, `git_diff`, `git_log`, `git_show` and `git_branches` (capability
`git.read`) are read-only:

- status and diff return the same bounded shapes as the UI;
- log is capped at 100 commits (default 20) and show at 1 MiB;
- revisions and paths are validated; paths are workspace-relative and may not
  contain `.git`;
- Git runs with fixed argv, no shell, hooks/fsmonitor/external diff disabled,
  optional locks and terminal prompts off, and no fetch/push;
- a non-repository workspace returns a structured `invalid_input` error.

`workspace_file_read` (capability `files.read`) returns one UTF-8 text file:

- the path is confined to the session workspace lexically and through
  canonicalization; absolute paths, `..`, `.git` components and symlinks are
  refused;
- output is line-rangeable and bounded (256 KiB default, 1 MiB max response,
  5,000 lines max); the file is read up to 4 MiB and marked `truncated` when
  larger;
- binary files and invalid UTF-8 return structured errors. The host never
  returns directory listings or arbitrary filesystem paths.

There is deliberately no Git write, worktree or code-index service yet; those
remain separate capabilities with their own approval and lifecycle design.

## Session observation, control and model calls

A plugin can observe and drive a session only through narrow, declared services, never through the
host's stores or mutable objects:

- `session.subscribe` opens a subscription tied to the backend's instance/epoch/generation and
  returns `{subscription, epoch, cursor}`. The host pushes `service.event` messages named
  `session.event` with a monotonic `cursor` and a `dropped` count; stale or foreign envelopes are
  discarded on both sides, and disabling/stopping the backend drops the subscription.
- The projected event set is closed: `configured`, `turn_started`, `user_message`,
  `assistant_text` (preview), `tool_started`/`tool_finished` (preview), `turn_complete`,
  `turn_aborted`, `error`, `compacted`, `agent`, `permissions`, `plan_mode`.
- `session.create` (capability `session.create`) creates a session in the declared workspace with
  the host's inherited/default permissions; a caller-supplied `permissions` field is rejected.
  `session.send`/`session.interrupt`/`session.wait` (capability `session.control`) use the
  session's own current permissions — a plugin can never widen them — and a project package can
  only touch sessions inside its trusted workspace, compared after canonicalization.
- `model.request` (capability `model.request`, target = purpose) performs one non-streaming call:
  purposes are `review`, `commit_message`, `classify`, `memory`, `summary`, `custom`; the prompt is
  capped at 64 KiB, the system prompt at 16 KiB, output at 4096 tokens, and the call is recorded in
  the ordinary API usage log.

## Delegated subagents

- Packages declare `agents` (behind `provide_agents`): the same name/description/tools/model/
  maxTurns/system prompt fields a Markdown profile has. They rank above the built-ins and below
  the user/project profile directories, so a package never silently replaces a user profile.
- `agent.list`, `agent.start`, `agent.send`, `agent.wait`, `agent.stop`, `agent.read` (capability
  `agent.delegate`) run through the existing subagent preparation/drive pipeline: the child
  inherits the session's workspace, permissions, trust and tool narrowing; the parent must be idle.
  Delegation is background-only; `background: false` is rejected rather than silently ignored, and
  `agent.wait` provides the bounded wait operation.
- A delegated child uses the existing Agent card (`ToolCallBegin` + `SubagentCard`), task
  registry, `ConversationStatus` and terminal notifications; late results and late steering are
  isolated exactly like a model-started child, and stopping it removes the pending approvals and
  releases the context writer.

## User/workspace configuration

- Manifest `config.items` declare a key, a localized description, a default and an optional JSON
  Schema for the value; the schema subset is the same closed one tools use, validated at install
  and on every write.
- A trusted workspace's `.pigcode/plugins.json` overrides options (`config`) and may disable an
  enabled package (`enabled: false`) or enable a resource-only package; undeclared keys keep
  inheriting the user value, and the user store is untouched. An untrusted workspace file is not
  read. Enabling a package with a backend stays a user-level decision (its Engine is global).
- The host renders a generic options form from the declarations (string, number, boolean, enum) in
  the plugin manager; there is no plugin-provided settings page. A scope switch writes either the
  user value or the current workspace's override. Project packages can only read or write the
  configuration scope of sessions in their own canonical workspace.

## Storage and side-effect recovery

- Besides the legacy whole-blob `storage_read`/`storage_write`, keyed storage supports
  `storage_list`, `storage_key_read`, `storage_key_write` and `storage_key_delete` with a per-key
  `revision`. Revisions never reuse a value after deletion: a delete writes a tombstone with the
  next revision, and delete/recreate responses carry the revision for the next CAS. A write/delete
  with `expectedRevision` is compare-and-swap: a stale revision fails with `stale_revision` and
  `details.currentRevision`, never silently overwriting.
- Side-effecting requests (storage writes, config writes, credential changes, HTTP requests,
  session create/send/interrupt, model requests, and agent start/send/stop) persist their result
  under an idempotency key (the request's own id unless the plugin supplies `idempotencyKey`). A
  retry replays the recorded result. An attempt that was in flight when the process died settles
  to a recorded `busy` error instead of executing again; records are never silently evicted.
  Storage files are per-package, private, and separate from the host's business state.

## Conflict rules

- Tools, agent profiles, skills, MCP servers and themes contributed by several enabled packages
  are resolved by package id (the first id wins); the snapshot carries `conflicts` entries naming
  the winner and every loser, and the plugin manager shows them.
- User/project skills and MCP servers always win over a plugin contribution of the same name; a
  plugin only fills names nobody configured. Disabling, updating or uninstalling a package
  re-resolves hooks, skills, profiles, tools and MCP servers in every live session; a session
  owned by a turn refreshes at its wrap-up, and the old MCP manager is dropped (its stdio children
  are killed).

## Identity and storage

`pig-core::PluginRegistry` owns installed packages, exact content digests, enable choices, capabilities and committed runtime contributions. Runtime instance identity includes the package digest and source scope. The stable private-data key excludes the digest so ordinary updates retain private plugin data under the existing authorization model.

Project plugins additionally require a trusted workspace. Registry and service handlers verify that package files have not changed after discovery.

## Themes

Theme packages remain declarative resources. A theme can define named tokens, light/dark values, styles for the supported theme slots and package-local background assets. Theme validation does not permit changes to host UI structure.

Backend-provided themes become visible only while that backend is active. Resource-only themes do not require a runtime Engine.

## Example

`examples/moon-plugin` contains:

- the `MoonEcho` backend tool (registered through the SDK with an input/output contract) and tool-selection response;
- a command that requests navigation to the built-in workspace;
- two themes;
- a Skill;
- a plugin configuration declaration.

It has no TSX UI entry.

## Development and verification

Use focused, serial checks while changing the runtime:

```sh
cargo check -p pig-protocol
cargo test -p nsh-app-pig-code signed_moon_package_loads_and_wrong_root_is_refused -- --test-threads=1
cargo test -p nsh-app-pig-code a_signed_sdk_package_activates_and_answers_tool_calls -- --test-threads=1
cargo test -p pig-core --lib plugins:: -- --test-threads=1
cargo test -p pig-core --test plugin_sessions -- --test-threads=1
nsh check
nsh test --filter "the SDK serves a real backend" --target windows
nsh test --filter "loads, applies, disables and restores plugin themes" --target windows
```

`a_signed_sdk_package_activates_and_answers_tool_calls` builds a real signed package from `examples/moon-plugin` (whose backend uses `pig/plugin`), starts its Engine through the plugin manager, commits the SDK's runtime contributions and asserts the `MoonEcho` answer. `crates/pig-core/tests/plugin_sessions.rs` drives session create/send/wait/interrupt, a bounded model request and delegated agents through the real agent loop. `App.plugin-sdk.test.tsx` runs appliers through the SDK's test host (tools, commands, events, health, structured errors, stale envelopes, hook contributions, session subscriptions/control, model calls, agents and keyed storage).

Run the broader project suites for a later, separate verification pass rather than starting overlapping Cargo builds.
