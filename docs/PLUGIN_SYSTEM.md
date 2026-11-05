# Plugin system

Pig Code installs signed NSH packages without rebuilding or restarting the app. The plugin API deliberately exposes product capabilities rather than a general host UI tree.

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
  "backend": "backend.ts",
  "minHostVersion": "1.0.0",
  "protocols": {"ui": 1, "services": 1},
  "capabilities": [
    {"name": "ui.add", "targets": ["status", "composer", "settings", "workspace"]}
  ],
  "contributions": {}
}
```

- `uiEntry` is the signed TSX module used by pages and overlay Surfaces.
- `backend` is an optional nonvisual module for tools, commands, subscriptions and declarative control state.
- A package with only declarative resources needs neither entry and does not create an Engine.
- Pages and overlay content do not require a backend. Interactive buttons, toggles, selects and menus, tools and commands do.

`examples/publish-plugin.rs` compiles the two entries independently. The UI bundle is the package's main bundle; the backend is signed as `.pig-backend.nshc`. Loading either one uses the verified package identity, runtime bytecode version and capabilities.

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

The host contributes navigation and mounts the selected page in the corresponding content area. A settings Surface is never inserted into the settings sidebar itself. Workspace pages replace the visible content selection, not the host's existing components.

### Popovers and dialogs

Popover and dialog contributions use the same slot state for their trigger and declare a Surface component and optional dimensions:

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

The host owns the trigger, open state, positioning, outside presses, Escape and focus restoration. It mounts the Surface when the overlay opens and destroys it when the overlay closes. The Surface can call `overlay.close` through the Host API. When keyboard focus is inside the nested AppView, the Surface forwards Escape through that call; `examples/moon-plugin` demonstrates this with `keymap.bind`.

## Surface implementation

A Surface is one plugin TSX module running in one nested NSH `AppView`, with its own Engine and HostChannel. It is used only for pages and custom overlay content.

Pig's host is itself an NSH TSX app, so the platform `AppView` is attached through one generic host-owned Native Component named `pig.pluginSurface`. Plugin packages do not declare or implement native components. The application crate registers the component with `#[native_components(macos)]` and `#[native_components(windows)]`; both platforms share loading, storage, Host API and lifecycle code.

The host synchronizes theme tokens, appearance and locale. Surface loading and signature verification happen off the UI thread. The AppView is loaded only on the platform UI path. Pages fill their allocated content area; overlays use the dimensions declared by the contribution.

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

Each mounted Surface receives an opaque Surface ID and a revision in `host.params.plugin`. Calls to `pig.host` carry the instance, Surface, contribution, revision and request ID. The manager checks them against the mounted descriptor before executing an operation.

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

`examples/moon-plugin` demonstrates a backend tool, a declarative metric and Composer action, a Surface popover, workspace and settings pages, themes, a Skill and configuration.

```sh
nsh check && nsh test
cargo test --workspace
```

The fixture is built by the signed Moon package loader test. Platform tests can then install `target/test-tmp/moon-plugin/moon.nshpkg` with `target/test-tmp/moon-plugin/root.pub`.
