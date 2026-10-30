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

## Executable plugins

All default UI ships with the app as a trusted, built-in TSX bundle at `.nsh/out/default/bundle.js`; it is not downloaded and does
not need plugin-signature verification. External `.nshpkg` packages only replace or extend those built-in surfaces.

`cargo run` starts the custom native host (Windows, macOS and Linux adapters) and registers `pig/plugin-surface`. The regular
`nsh run` host does not register executable plugin surfaces and keeps the default composer. The custom host verifies the signed
`.nshpkg`, runs it in an isolated `AppView`, and forwards composer actions and metrics through `HostChannel`.

On Windows, `pig/window-guard` holds a resize repaint until the root `onLayout` confirms the latest size, with a
50ms fallback that prevents a stuck window. It uses NSH 0.3.0's `WM_DPICHANGED` viewport path rather than changing
any dependency; review `src/host/resize_guard.rs` when upgrading NSH. `PIG_RESIZE_TRACE=1 cargo run` logs the
resize/layout/paint sequence.

```sh
nsh build App.tsx -o .nsh/out/default
cargo run

nsh build App.selftest.tsx -o .nsh/out/selftest
PIG_BUNDLE=.nsh/out/selftest/bundle.js cargo run  # isolated mock environment
```

The sample in `examples/executable-plugin` provides a petal composer, a pet overlay and an approximate token-rate control. To
rebuild its signed package with development keys kept out of source control:

```sh
nsh keygen .nsh/dev-keys/root
nsh keygen .nsh/dev-keys/release
nsh certify --root .nsh/dev-keys/root.secret --key .nsh/dev-keys/release.pub --apps 'com.pigcodets.plugins.*' -o .nsh/dev-keys/release.cert
nsh publish examples/executable-plugin/App.tsx --key .nsh/dev-keys/release.secret --cert .nsh/dev-keys/release.cert
```

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
