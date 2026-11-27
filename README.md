# pig-code

Pig Code is a decoupled monorepo with a pure Rust backend workspace and a pure TSX NSH desktop client:

- **`crates/*`** — Pure Rust workspace (zero `nsh-*` dependencies):
  - `pig-daemon`: Long-lived local WebSocket daemon owning sessions, SQLite FTS5 search, PTY terminals, syntax highlighting, file/directory browsing, and the plugin runtime host.
  - `pig-plugin-host`: Standalone QuickJS (`quickjs-jit`) plugin engine, Ed25519 signed package verifier/loader (`.pigpkg` / `.nshpkg`), and marketplace trust store.
  - `pig-core`, `pig-protocol`, `pig-provider`, `pig-utils`, `pig-tui`: Core agent loop, protocol types, model providers, shared utilities, and terminal UI client.
- **`apps/desktop/`** — Standard NSH `--template ts` desktop application (zero Rust code). `App.tsx` is the entry point, `app/daemon-client.ts` connects to `pig-daemon` over WebSocket, `app/view.tsx` composes the interface from `app/components/`, and shared state lives in `app/store.ts` and `app/theme.ts`. Tests live in `apps/desktop/tests/`.
- **`sdk/pig-plugin/`** — Pure TypeScript plugin SDK (`pig/plugin`) executed inside `pig-plugin-host`.

## Development

```sh
# Backend checks and tests
cargo check --all-targets
cargo test --workspace
cargo build --bin pig-daemon

# Desktop app (ensure target/debug is on PATH so pig-daemon can be launched)
cd apps/desktop
nsh check
nsh test
nsh run
```

## Unified plugins

Signed `.pigpkg` (and legacy `.nshpkg`) packages are verified and installed at runtime by `pig-daemon` without rebuilding or restarting the desktop app. Open Plugin Management and provide the package path plus its trusted root public key. Packages can contribute Agent tools, commands, model-provider presets, themes, Skills, MCP servers, and configuration. A package with a backend runs in an isolated `pig-plugin-host` QuickJS runtime inside `pig-daemon`; resource-only packages do not start an engine. Disabling a package stops its backend and removes its runtime contributions.

`examples/moon-plugin` demonstrates a backend tool, a command, a local model provider, themes, a Skill, and configuration. The architecture and protocol contract live in `docs/PLUGIN_SYSTEM.md`.
