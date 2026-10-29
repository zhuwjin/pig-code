# pig-code-ts

Pig Code's NSH + TSX desktop app. `App.tsx` is the interface, and the directory is also a crate (`Cargo.toml`,
`src/lib.rs`; `nsh.json` `"rust": "."`) whose `#[native_module("pig-code-ts/native")]` exports the native `Agent`
connection. The existing agent engine lives in `crates/pig-core`, with `pig-protocol`, `pig-provider` and `pig-utils`
alongside it. TSX sends commands and receives ordered event batches through this connection.

```sh
nsh run                      # this machine's native backend, no target needed
nsh check                    # cargo check of the crate, then type checks App.tsx against the declarations generated from it
nsh test                     # cargo test, then App.test.tsx, headless; --target to run it on a platform
nsh package --target web     # or ios, android, macos, linux, windows
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
