# Plugin system

Pig Code installs signed NSH packages without rebuilding or restarting the app. Declarative contributions expose product capabilities rather than a general host UI tree. An installed package can additionally export trusted TSX components for its own pages and overlays.

## Package model

A package manifest contains its identity, capabilities and contributions. Entry points are optional:

```json
{
  "apiVersion": 1,
  "id": "com.example.plugin",
  "name": {"en": "Example"},
  "version": "1.0.0",
  "build": 1,
  "uiEntry": "App.tsx",
  "componentEntry": "component.tsx",
  "backend": "backend.ts",
  "minHostVersion": "1.0.0",
  "protocols": {"ui": 1, "services": 1},
  "capabilities": [
    {"name": "ui.add", "targets": ["status", "composer", "settings", "workspace"]}
  ],
  "contributions": {}
}
```

- `uiEntry` is the signed TSX module used by legacy nested-AppView Surfaces.
- `componentEntry` is an optional TSX module exporting components for pages and overlays. Installing its signed package makes these components trusted host code; this mode does not provide a permission sandbox.
- `backend` is an optional nonvisual module for tools, commands, subscriptions and declarative control state. It keeps its independent Engine and existing permissions when the UI uses host components.
- A package with only declarative resources needs none of these entries and does not create an Engine.
- Pages and overlay content do not require a backend. Interactive buttons, toggles, selects and menus, tools and commands do.

`examples/publish-plugin.rs` compiles the three entries independently. The legacy UI bundle is the package's main bundle; the backend is signed as `.pig-backend.nshc`. The component's bundled JavaScript and source map are signed as `.pig-component.js` and `.pig-component.js.map`. The loader verifies the package before importing a component chunk under its digest-derived ID. All exports of one package share that module and the host's registered native UI modules.

## UI capabilities

### Declarative slots

A UI contribution inserts a control into a named product slot. It cannot patch, replace, wrap, move or remove an existing host component.

```json
{
  "id": "summary",
  "slot": "status",
  "kind": "metric",
  "label": {"en": "Summary"},
  "icon": "activity",
  "order": 10
}
```

Slots currently supported by real host components:

- `status`: session status below the conversation;
- `composer`: actions beside the composer controls;
- `message`: actions associated with one turn.

Control kinds are `button`, `toggle`, `select`, `menu`, `metric`, `indicator`, `popover` and `dialog`. The host owns layout, style, hover, focus, accessibility and interaction behavior. A metric or indicator can publish passive detail rows; select and menu controls use declared choices.

UI declarations are static. A backend can only publish state for an ID declared in the manifest:

```text
ui.publish → {
  id,
  label?,
  icon?,
  visible?,
  enabled?,
  loading?,
  value?,
  choices?,
  details?,
  segments?
}
```

State is data, not a node tree. The manager rejects unknown or duplicate IDs, invalid control values and oversized batches. The latest state lives in the host Store and is removed with its plugin instance.

### Pages

A page declaration has a stable ID, localized title, icon, order and a `kind` of `workspace` or `settings`:

```json
{
  "id": "overview",
  "surface": "Overview",
  "title": {"en": "Overview"},
  "kind": "workspace",
  "icon": "panel-left",
  "order": 10
}
```

The host contributes navigation and mounts the selected page in the corresponding content area. A settings plugin page is never inserted into the settings sidebar itself. Workspace pages replace the visible content selection, not the host's existing components.

Once visited, a page retains its selected content while the user navigates elsewhere. Only the selected page participates in layout and accessibility; hidden pages use `display: none`. Legacy pages also hide their native child view without reloading the signed package. Session changes remount the page with its new context, and disabling, updating or uninstalling its package still releases the retained page.

### Popovers and dialogs

Popover and dialog contributions use the same slot state for their trigger and declare a component export and optional dimensions:

```json
{
  "id": "details",
  "slot": "status",
  "kind": "popover",
  "label": {"en": "Details"},
  "surface": "Details",
  "width": 320,
  "height": 240
}
```

The host owns the trigger, open state, positioning, outside presses, Escape and focus restoration. It mounts the selected content when the overlay opens and destroys it when the overlay closes. A legacy Surface can call `overlay.close` through the Host API. Host-rendered components participate in the host overlay directly and do not need to forward Escape from a child window.

## UI rendering implementations

### Trusted host components

With `componentEntry`, `pig.pluginComponent` verifies the installed package off the UI thread and delivers its JavaScript Chunk to the current Engine. The TSX bridge imports that Chunk and calls the export named by the page or overlay contribution as a normal host component. The native bridge does not create another Engine or a plugin HWND; macOS and Linux use empty native views only to satisfy their component interfaces. Host Dialog and Popover stacking, focus and styling apply to plugin content.

The Chunk ID includes the verified package digest. Reopening a page or rendering a second export imports the same module rather than evaluating it again. The component source is bundled at publication and compiled by the Engine under that Chunk ID, rather than being packaged as an entry module named `bundle.js`. Unmounting releases the rendered UI and its registration; evaluated ES modules remain cached for the host Engine's lifetime and are not claimed to be individually unloaded.

The bridge is implemented for Windows, macOS and Linux, with the local NSH fork and per-platform verification status recorded in `docs/NSH_UPSTREAM_PENDING.md`. The component Host API is not connected yet: session/storage/navigation calls must still use the independent backend or the legacy Surface. The Moon `component.tsx` is therefore a static rendering example, not a completed migration of every Host API.

### Legacy nested Surfaces

A package without `componentEntry` keeps the existing nested-AppView Surface, with its own Engine and HostChannel. The generic host-owned Native Component `pig.pluginSurface` attaches that AppView; plugin packages do not declare or implement native components. Both Windows and macOS retain this fallback.

For legacy Surfaces, the host synchronizes theme tokens, appearance and locale before creating the first native view. Loading and signature verification happen off the UI thread. Pages fill their allocated content area; overlays use the dimensions declared by the contribution.

## Backend protocol

The backend uses the existing versioned envelope over `plugin.protocol`. It handles:

- lifecycle handshake, readiness and health checks;
- tool selection, execution and results;
- command invocation and host services;
- declarative UI state and control events;
- optional runtime contributions for nonvisual capabilities.

Pages and UI declarations cannot be registered dynamically. Tools, commands, themes, Skills and MCP retain their existing validated contribution paths. Runtime contributions cannot change permissions, credentials, HTTP origins or configuration schemas.

A UI event contains the contribution ID, a unique event ID, `invoke` or `change`, its JSON value and the host context (`sessionId` and optional `turnId`). It is forwarded to the backend only when valid for that contribution kind.

## Host API

Each mounted legacy Surface receives an opaque Surface ID and a revision in `host.params.plugin`. Calls to `pig.host` carry the instance, Surface, contribution, revision and request ID. The manager checks them against the mounted descriptor before executing an operation. Host-rendered components do not receive this plugin channel yet; they must not be described as having completed the Host API migration.

The current Host API provides:

- `session.snapshot`, `session.subscribe` and `session.unsubscribe`;
- `storage.read` and `storage.write` using the plugin's stable private data scope;
- `navigation.open` for declared navigation targets;
- `overlay.close` for the current Surface.

Host handlers do not block the plugin Engine. Requests and subscriptions are cancelled when their Surface or plugin instance is retired, and late replies cannot revive them.

## Lifecycle and ownership

`PluginSystemManager` coordinates backend Engines, Surface registrations, service calls, subscriptions and diagnostics. `pig-core::PluginRegistry` owns installed packages, enable choices, capabilities and committed nonvisual runtime contributions.

Disabling, updating or uninstalling a plugin stops its backend, unmounts its Surfaces, cancels requests and subscriptions, removes declarative state and closes its overlays. Runtime instance identity includes the exact package digest; the stable private data key does not, so ordinary updates retain private data under the existing authorization model.

Theme packages remain declarative resources. They can change named tokens, styles and assets without creating an Engine or changing host UI structure.

## Removed design

The following are intentionally absent:

- `MountBatch` projection and `RemoteDocument` / `RemoteNode`;
- remote node event forwarding and UI acknowledgement messages;
- arbitrary host `patch`, `replace`, `wrap`, `move`, `remove` and `restore` operations;
- remote style/property allowlists and host node IDs;
- plugin-specific contribution IDs, overlay anchors or dimensions in host components;
- feature-bit negotiation and legacy UI compatibility fields;
- Composer replacement and arbitrary host DOM modification.

## Development and verification

`examples/moon-plugin` demonstrates a backend tool, a declarative metric and Composer action, a host-component popover, workspace and settings pages, themes, a Skill and configuration. `App.tsx` retains the legacy Surface implementation; `component.tsx` contains the host-rendered counterparts.

```sh
nsh check && nsh test
cargo test --workspace
```

The fixture is built by the signed Moon package loader test. Platform tests can then install `target/test-tmp/moon-plugin/moon.nshpkg` with `target/test-tmp/moon-plugin/root.pub`.
