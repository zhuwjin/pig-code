# pig-code Workspace Guide

A graphical AI code-agent desktop app: Rust + gpui-kit (crates.io release), single process with two execution domains, UI benchmarked against ZCode. **Read `docs/PLAN.md` before touching anything** — it is the single source of design decisions and implementation records (including past "polish" notes and upstream bug tracking). Sensitive areas (dock layout, MCP, Windows subprocesses, streaming rendering) require reading the relevant sections first.

## Common Commands

```bash
cargo build                          # Build the whole workspace (dev profile sets opt-level=3 for dependencies — do not remove)
cargo run -p pig-app                 # Launch the GUI (binary name: pig-code)
cargo test -p pig-core               # Engine integration tests (tests/ dir; all green before wrapping up)
cargo test -p pig-core --test <name> # A single test file, e.g. --test bash, --test mcp (by file name)
cargo fmt && cargo clippy            # No custom rustfmt/clippy config; use the defaults
PIG_SELFTEST=1 cargo run -p pig-app  # GUI end-to-end selftest (built-in mock provider + isolated temp data dir)
cargo run -p pig-core --example mock_provider  # Standalone mock server for manual testing; point ~/.pigcode/config.toml at the printed base_url
```

## Architecture & Layering (Hard Rules)

Three crates with one-way dependencies: `pig-app → pig-core → pig-protocol`.

- **pig-protocol**: pure serde types (`Op` UI→core commands, `Event` core→UI events, config models), zero business logic, zero heavy deps. Keep protocol changes backward compatible (rollouts store old records; evolve gradually with `#[serde(default)]`).
- **pig-core**: the agent engine, running on a dedicated tokio-runtime thread (entry point `spawn_agent`), exchanging Op/Event with the UI over `async-channel`. **core must never import any gpui type**. Core design: delta events (TextDelta etc.) are for live rendering only and are never persisted; Done events carrying full values are the durable boundary; approval = core sends `ApprovalRequested` (with request_id) then blocks waiting for the UI's `ApprovalReply`; sessions persist as JSONL rollouts (first line meta + one item per line).
- **pig-app**: the gpui-kit GUI, smol executor; `cx.spawn` loops read the Event channel and reduce into per-Entity view state.

## Code Conventions

- **English everywhere in code and docs**: comments (`//`, `///`, `//!`) and documentation (AGENTS.md, PLAN.md) are all English (docs switched 2026-10-07). **Commit messages stay Chinese for now**: `feat:`/`fix:`/`refactor:`/`docs:` prefix + Chinese description.
- **pig-core is fully English and language-agnostic** (Clean/Hexagonal): errors are carried by pig-protocol's `CoreError` enum + English `detail`; rust-i18n is forbidden inside core (i18n.rs has a zero-reference guard test); localization happens at UI render points (pig-app `core_error_text`); upstream error text is passed through verbatim as English detail. The only Chinese left in core is deliberately kept functional CJK test data (mock ticker wide-char rendering lines, `"密".repeat` multibyte truncation, GBK/UTF-16 codec fixtures, and in-comment GBK byte references).
- **Model-facing prompts are always English** (system prompt, tool descriptions, tool-result copy, reminders, built-in subagent profile bodies, compaction/title prompts — aligned with kimi-code/ZCode conventions; output language follows the user via the "Match the user's language" clause).
- **GUI strings always go through pig-app's rust-i18n registry** (`rust_i18n::t!("module.key")` + `crates/pig-app/locales/*.yml`, adding both zh-CN and en) — same mechanism as gpui-component; no hardcoded natural-language strings. The completeness tests in i18n.rs catch missing translations and nonexistent keys. Count labels with plural-sensitive English use `<key>_one` for n == 1 (zh values identical); adding a language = one row in `SUPPORTED` plus translations.
- **Check official components before building UI**: the gpui-kit component catalog is at https://gpui-kit.com/component (a quick-reference table of all 77 components: [docs/gpui-kit-components.md](docs/gpui-kit-components.md)). Use existing components over hand-rolled ones; copy usage from the official stories. Anything that must be custom (e.g. the diff view) gets its rationale and upstream tracking recorded in `docs/PLAN.md` first.
- **fmt/clippy must be clean before committing**: `cargo fmt --check` with no diff, `cargo clippy --all-targets -- -D warnings` with zero warnings (fully zeroed 2026-09-30; keep it that way). Structural lints (e.g. `too_many_arguments`) are `#[allow]`ed per existing convention; no bone-breaking refactors for lints.
- **Module splitting convention**: a single file over ~1000 lines splits into a same-named directory + submodules (see provider/, agent/, session/, thread_view/, composer/, sidebar/, settings/, task/, terminal/). Splitting gotchas: `use super::*` drags in the parent module's `as _` trait imports (the submodule's own imports then look unused); `pub(crate)` globs squeeze visibility down to the crate — former pub items need explicit `pub use` to restore it.
- Logging uses `eprintln!` (no log crate; under the Windows GUI subsystem writes are silently dropped, which is safe). Log output is English.
- Data dir `~/.pigcode` (`PIG_DATA_DIR` env var overrides it; the selftest relies on this for isolation); config at `~/.pigcode/config.toml`; MCP config is user-level `<data_dir>/mcp.json` + project-level `.pigcode/mcp.json` (project entries override same-named user entries).

## Platform & Upstream Gotchas

- **License hard line**: gpui-kit comes from crates.io only; never switch to zed's gpui git dependency (it would pull in GPL-3.0). zed `agent_ui` may be read for design but never copied.
- **Windows**: release builds use the GUI subsystem (`windows_subsystem = "windows"`); **every subprocess spawn must go through `NoConsoleExt` (CREATE_NO_WINDOW)**, otherwise git/cmd pops a new console window on every call; MCP stdio launches must go through `resolve_program` (npx/pnpm are .cmd shims; a bare `Command::new` won't find them).
- **gpui-kit 0.x APIs change**: read the changelog before upgrading; copy component usage from official stories. Known upstream rendering bugs (e.g. #3293, table full-width punctuation swallowed at line ends) are recorded in PLAN.md — when rendering misbehaves, first check whether it's an upstream issue instead of patching downstream (e.g. table CELL_PAD_PX participates in column-width measurement; "fixing" the padding treats the symptom and breaks wrapping elsewhere).
- serde_json Map iteration order depends on the preserve_order feature; sort explicitly when order must be stable (e.g. resume keys).
