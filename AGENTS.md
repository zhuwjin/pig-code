# Pig Code

Pig Code is a decoupled monorepo:

- `crates/*`: Pure Rust backend workspace (zero `nsh-*` dependencies) containing `pig-core`, `pig-daemon`, `pig-plugin-host`, `pig-protocol`, `pig-provider`, `pig-tui`, and `pig-utils`.
- `apps/desktop/`: Pure TSX desktop client built on NSH (`nsh init --template ts`, zero Rust code). It communicates with `pig-daemon` over WebSocket.
- `sdk/pig-plugin/`: Pure TypeScript plugin SDK (`pig/plugin`) executed by `pig-plugin-host` (QuickJS).

## UI Development

Before writing or modifying UI in `apps/desktop/`, consult the official NSH examples or documentation. Do not guess how components, props, events, styles, or animations work. Prefer official components and their interaction patterns over reimplementing existing capabilities.

- [Official examples](https://github.com/longbridge/nsh/tree/main/examples)
- [Module documentation](https://nsh.dev/modules/)
- [Element documentation](https://nsh.dev/elements/)
- [Developer documentation](https://nsh.dev/docs/)
- [Design guides](https://nsh.dev/docs/design-guides/)
- [Coding guides](https://nsh.dev/docs/coding-guides/)

Run `cargo check --all-targets && cargo test --workspace` for Rust changes, and `cd apps/desktop && nsh check && nsh test` (with `target/debug` on `PATH` so `pig-daemon` can be launched) after making UI changes.

## Internationalization

- UI copy lives in `apps/desktop/app/locales/en.json` and `apps/desktop/app/locales/zh-CN.json` under hierarchical semantic keys (`settings.appearance.title`, `settings.appearance.mode.light`); call `t("settings.appearance.title", { name })` from `apps/desktop/app/i18n.ts`. English is the fallback.
- Keep both catalogs in sync. A word with different meanings in different contexts gets a distinct key; do not reuse a generic key across contexts.
- Source code, comments and docs are English-only. Only locale catalogs and documentation may contain other languages.
- Tests may pin `locale: "zh-CN"` and assert localized copy.
- Do not add new hardcoded UI strings.
