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

- `tools`: name, description, object JSON Schema, read-only marker and `static` or `plugin` availability;
- `commands`: stable ID, localized title, optional export, order and enabled state;
- `themes`: validated theme definitions and package-local background assets;
- `skills`: package directories containing `SKILL.md`;
- `mcp`: validated MCP server configurations;
- `credentials`: plugin-owned or provider credential declarations;
- `http`: explicit HTTP(S) origins, GET/POST methods and allowed credentials;
- `config`: plugin defaults and allowed host configuration reads/writes;
- `actions`: nonvisual service permissions such as `provide_tools`, `provide_themes`, `provide_mcp`, `http`, `read_config` and `write_config`.

Manifest tools, commands, themes, Skills and MCP servers can be supplemented at runtime through `contribution.register` and `contribution.remove`. Runtime registration cannot change credentials, HTTP origins, configuration schemas or permissions. Duplicate IDs are rejected or replaced deterministically, and committed contributions are removed when their backend stops.

## Backend protocol

All backend messages use the same envelope:

```text
protocol, kind, instance, epoch, stream, sequence,
requestId?, generation, body
```

Supported streams are `lifecycle`, `events`, `contributions`, `tools`, `services` and `logs`. Sequence tracking is independent per stream. Instance, epoch and generation checks reject stale backends and late replies.

Lifecycle includes hello/start/ready, health checks, stop/failure, retry with bounded backoff, and exact-digest updates. Tool traffic includes selection, calls, results, timeouts and cancellation. Logs are size-limited and recursively redacted before reaching the host.

Commands are resolved against manifest and committed runtime contributions. Invoking one sends `command.invoke` to the active backend; commands do not open plugin pages.

## Resource budgets and isolation

A backend Engine has a 64 MiB memory budget, operation and call-time budgets, and no canvas-operation allowance. Loading and health checks have independent deadlines. Tool and service requests have concurrency limits, and private storage is limited to 1 MiB per plugin scope. These values are limits enforced by the host, not measurements of actual memory or CPU use.

NSH does not currently expose per-Engine real memory or CPU measurements. Runtime metrics therefore report pending, timed-out and cancelled requests, dropped log events, network bytes and storage bytes rather than presenting resource budgets as observed usage.

## Host services

The backend can request only declared and authorized services:

- private plugin storage reads and writes;
- allowed configuration reads and writes;
- allowed HTTP requests with explicit credential injection;
- owned credential set/clear and separately authorized provider credentials;
- session and workspace projections;
- navigation to built-in host pages;
- Composer submission;
- package resource reads;
- locale reads and locale-change events.

Navigation targets are limited to `main`, `models`, `appearance`, `plugins` and `marketplace`. There is no plugin-page navigation API and no Surface-specific `pig.host` channel.

Requests have timeouts and concurrency limits. Disabling, updating or uninstalling a package stops its backend, cancels pending tools and services, removes runtime contributions and drops late replies.

## Identity and storage

`pig-core::PluginRegistry` owns installed packages, exact content digests, enable choices, capabilities and committed runtime contributions. Runtime instance identity includes the package digest and source scope. The stable private-data key excludes the digest so ordinary updates retain private plugin data under the existing authorization model.

Project plugins additionally require a trusted workspace. Registry and service handlers verify that package files have not changed after discovery.

## Themes

Theme packages remain declarative resources. A theme can define named tokens, light/dark values, styles for the supported theme slots and package-local background assets. Theme validation does not permit changes to host UI structure.

Backend-provided themes become visible only while that backend is active. Resource-only themes do not require a runtime Engine.

## Example

`examples/moon-plugin` contains:

- the `MoonEcho` backend tool and tool-selection response;
- a command that requests navigation to the built-in workspace;
- two themes;
- a Skill;
- a plugin configuration declaration.

It has no TSX UI entry.

## Development and verification

Use focused, serial checks while changing the runtime:

```sh
cargo check -p pig-protocol
cargo test -p nsh-app-pig-code-ts signed_moon_package_loads_and_wrong_root_is_refused -- --test-threads=1
nsh check
nsh test --filter "validates backend envelopes" --target windows
nsh test --filter "loads, applies, disables and restores plugin themes" --target windows
```

Run the broader project suites for a later, separate verification pass rather than starting overlapping Cargo builds.
