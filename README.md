# pig-code-ts

Pig Code's NSH + TSX desktop app. `App.tsx` is the entry point; `app/view.tsx` composes the interface from the
region components in `app/components/`. Shared state lives in `app/store.ts` and `app/theme.ts`. The directory is
also a crate (`Cargo.toml`, `src/lib.rs`; `nsh.json` `"rust": "."`) whose `#[native_module("pig-code-ts/native")]`
exports the native `Agent` connection. The existing agent engine lives in `crates/pig-core`, with `pig-protocol`,
`pig-provider` and `pig-utils` alongside it. TSX sends commands and receives ordered event batches through this
connection. `app/components/icon.tsx` uses the gallery's Canvas drawing pattern with original
`gpui-kit-assets 0.8.1` Lucide geometry in `app/components/icon-elements.ts`; the source SVG files and their
license stay in `assets/icons/`. `assets/pig-code-wordmark.svg` is the original wordmark mask sampled by the
hero's particle canvas.

```sh
nsh run                      # this machine's native backend, no target needed
nsh check                    # cargo check of the crate, then type checks App.tsx against the declarations generated from it
nsh test                     # cargo test, then App.test.tsx, headless; --target to run it on a platform
nsh package --target web     # or ios, android, macos, linux, windows
```

## Unified plugins

Signed `.nshpkg` packages are verified and installed at runtime without rebuilding or restarting the app. Open Plugin Management
and provide the package path plus its trusted root public key. Declarative controls are rendered by the host into fixed product
slots; complete pages and custom overlays use plugin TSX in nested Surfaces. An optional backend handles tools, commands,
subscriptions and control state without rendering UI. Themes, skills, MCP and configuration share the same package identity
and lifecycle. Disabling a package removes its contributions and stops its runtime resources.

`examples/moon-plugin` demonstrates declarative metrics and actions, Surface pages and popovers, a backend tool, a theme,
a skill and configuration. The architecture and protocol contract live in `docs/PLUGIN_SYSTEM.md`.

`nsh` describes the crate's exports with a small program of the crate and builds the app's hosts with it linked in.
TSX edits are hot updates; Rust edits rebuild and restart the app. The first build writes the crate's TypeScript declarations into
`.nsh/types/pig-code-ts/native.d.ts`, doc comments included.

Native methods run according to their Rust signatures: synchronous commands such as `Agent.newSession` return directly
(or throw), while `Agent.listen` returns a Promise and delivers ordered event batches as an NSH task. Add further native
exports in `src/lib.rs` with `registry.register::<…>()` (components), `registry.module::<…>()` and
`registry.class::<…>()`.

The `nsh` commands build the app's hosts from the NSH crates `Cargo.toml` depends on: crates.io's at the version of the
`nsh` you run (keep the `nsh-*` dependencies at that version when you upgrade `nsh`), or an NSH checkout's by path.

NSH-generated files stay in `.nsh/`: runner sources in `.nsh/rust/` and default packages in `.nsh/out/`.
Cargo keeps its normal `target/` directory and respects your existing `CARGO_TARGET_DIR`.
