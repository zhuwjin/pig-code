# Pig Code

The Pig Code desktop client. Its UI is written in TypeScript/TSX with NSH, while Rust provides native capabilities for agents, providers, plugins, and terminals.

## UI Development

Before writing or modifying UI, consult the official NSH examples or documentation. Do not guess how components, props, events, styles, or animations work. Prefer official components and their interaction patterns over reimplementing existing capabilities.

- [Official examples](https://github.com/longbridge/nsh/tree/main/examples)
- [Module documentation](https://nsh.dev/modules/)
- [Element documentation](https://nsh.dev/elements/)
- [Developer documentation](https://nsh.dev/docs/)
- [Design guides](https://nsh.dev/docs/design-guides/)
- [Coding guides](https://nsh.dev/docs/coding-guides/)

Run `nsh check && nsh test` after making changes.

## Internationalization

- UI copy lives in `app/locales/en.json` and `app/locales/zh-CN.json` under hierarchical semantic keys (`settings.appearance.title`, `settings.appearance.mode.light`); call `t("settings.appearance.title", { name })` from `app/i18n.ts`. English is the fallback.
- Keep both catalogs in sync. A word with different meanings in different contexts gets a distinct key; do not reuse a generic key across contexts.
- Source code, comments and docs are English-only. Only locale catalogs and documentation may contain other languages.
- Tests may pin `locale: "zh-CN"` and assert localized copy.
- Do not add new hardcoded UI strings.
