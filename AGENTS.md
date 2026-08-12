# Aphid

Aphid is a fast and hackable agent harness.

## Goal

- Zero memory copy whenever possible
- Data oriented design
- Fast startup times
- Full debuggability, and extensibility via plugins
- Plugins can be written in rhai (https://github.com/rhaiscript/rhai) — see `docs/aphid/plugins.md`
  and the `aphid-plugin` crate. WebAssembly plugins are still to come

## Instructions

- Run lint, format and tests at every code change
    - `cargo clippy`, `cargo fmt`, `cargo test`
- When fixing bugs, add regression tests
- Documentation should be written in ASD-STE100 simplified technical english
- Keep documentation up to date with changes
- Create git commits with Conventional Commits specification
